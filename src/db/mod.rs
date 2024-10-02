use std::{borrow::Cow, cell::RefCell, sync::Arc};

use anyhow::{anyhow, bail, Context, Result};
use moka::future::Cache;
use objdiff_core::bindings::report::{Report, ReportUnit};
use prost::Message;
use sqlx::{migrate::MigrateDatabase, Pool, Sqlite, SqlitePool};

use crate::{
    config::AppConfig,
    models::{Commit, Project, ProjectInfo, ReportFile},
};

#[derive(Clone)]
pub struct Database {
    pool: Pool<Sqlite>,
    report_cache: Cache<ReportKey, Arc<Report>>,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct ReportKey {
    owner: String,
    repo: String,
    commit: String,
    version: String,
}

// BLAKE3 hash of the unit data
type UnitKey = [u8; 32];

impl Database {
    pub async fn new(config: &AppConfig) -> Result<Self> {
        if !Sqlite::database_exists(&config.db_url).await.unwrap_or(false) {
            tracing::info!(db_url = %config.db_url, "Creating database");
            Sqlite::create_database(&config.db_url).await.context("Failed to create database")?;
            tracing::info!("Database created");
        }
        let pool =
            SqlitePool::connect(&config.db_url).await.context("Failed to connect to database")?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("Failed to run database migrations")?;
        let report_cache = Cache::builder().max_capacity(100).build();
        let db = Self { pool, report_cache };
        db.fixup_report_units().await?;
        Ok(db)
    }

    pub async fn close(&self) { self.pool.close().await }

    pub async fn insert_report(&mut self, file: &ReportFile) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let project_id = file.project.id as i64;
        sqlx::query!(
            r#"
            INSERT INTO projects (id, owner, repo, name, short_name, default_version, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            ON CONFLICT (id) DO NOTHING
            "#,
            project_id,
            file.project.owner,
            file.project.repo,
            file.project.name,
            file.project.short_name,
            file.project.default_version,
        )
        .execute(&mut *tx)
        .await?;
        let data = compress(
            &Report {
                measures: file.report.measures,
                units: vec![],
                version: file.report.version,
                categories: file.report.categories.clone(),
            }
            .encode_to_vec(),
        );
        let report_id = sqlx::query!(
            r#"
            INSERT INTO reports (project_id, version, git_commit, timestamp, data)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT (project_id, version COLLATE NOCASE, git_commit COLLATE NOCASE) DO UPDATE
            SET timestamp = EXCLUDED.timestamp
            RETURNING id
            "#,
            project_id,
            file.version,
            file.commit.sha,
            file.commit.timestamp,
            data,
        )
        .fetch_one(&mut *tx)
        .await?
        .id;
        let mut keys = Vec::with_capacity(file.report.units.len());
        for unit in &file.report.units {
            let mut data = unit.encode_to_vec();
            let key: UnitKey = blake3::hash(&data).into();
            keys.push(key);
            let key = key.to_vec();
            data = compress(&data);
            sqlx::query!(
                r#"
                INSERT INTO report_units (id, data, name)
                VALUES (?, ?, ?)
                ON CONFLICT (id) DO NOTHING
                "#,
                key,
                data,
                unit.name,
            )
            .execute(&mut *tx)
            .await?;
        }
        for (idx, key) in keys.iter().enumerate() {
            let key = key.to_vec();
            let idx = idx as i32;
            sqlx::query!(
                r#"
                INSERT INTO report_report_units (report_id, report_unit_id, unit_index)
                VALUES (?, ?, ?)
                ON CONFLICT DO NOTHING
                "#,
                report_id,
                key,
                idx,
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.report_cache
            .insert(
                ReportKey {
                    owner: file.project.owner.to_ascii_lowercase(),
                    repo: file.project.repo.to_ascii_lowercase(),
                    commit: file.commit.sha.to_ascii_lowercase(),
                    version: file.version.to_ascii_lowercase(),
                },
                file.report.clone(),
            )
            .await;
        Ok(())
    }

    pub async fn get_report(
        &self,
        owner: &str,
        repo: &str,
        commit: &str,
        version: &str,
    ) -> Result<Option<ReportFile>> {
        let mut conn = self.pool.acquire().await?;
        let (report_id, project, commit, version, mut report) = match sqlx::query!(
            r#"
            SELECT
                reports.id as "report_id!",
                git_commit,
                timestamp,
                version,
                data,
                projects.id as "project_id!",
                owner,
                repo,
                name,
                short_name,
                default_version,
                platform
            FROM reports JOIN projects ON reports.project_id = projects.id
            WHERE projects.owner = ? COLLATE NOCASE AND projects.repo = ? COLLATE NOCASE
                  AND version = ? COLLATE NOCASE AND git_commit = ? COLLATE NOCASE
            "#,
            owner,
            repo,
            version,
            commit
        )
        .fetch_optional(&mut *conn)
        .await?
        {
            Some(row) => {
                let data = decompress(&row.data).context("Failed to decompress report data")?;
                let report = Report::decode(data.as_ref()).context("Failed to decode report")?;
                (
                    row.report_id,
                    Project {
                        id: row.project_id as u64,
                        owner: row.owner,
                        repo: row.repo,
                        name: row.name,
                        short_name: row.short_name,
                        default_version: row.default_version,
                        platform: row.platform,
                    },
                    Commit { sha: row.git_commit, timestamp: row.timestamp.and_utc() },
                    row.version,
                    report,
                )
            }
            None => return Ok(None),
        };
        let key = ReportKey {
            owner: owner.to_ascii_lowercase(),
            repo: repo.to_ascii_lowercase(),
            commit: commit.sha.to_ascii_lowercase(),
            version: version.to_ascii_lowercase(),
        };
        if let Some(report) = self.report_cache.get(&key).await {
            return Ok(Some(ReportFile { project, commit, version, report }));
        }
        for row in sqlx::query!(
            r#"
            SELECT ru.id AS "id!", ru.data, rru.unit_index
            FROM report_report_units rru JOIN report_units ru ON rru.report_unit_id = ru.id
            WHERE rru.report_id = ?
            ORDER BY rru.unit_index
            "#,
            report_id
        )
        .fetch_all(&mut *conn)
        .await?
        {
            let idx = row.unit_index as usize;
            if idx != report.units.len() {
                bail!("Report unit index mismatch: {} but expected {}", idx, report.units.len());
            }
            let key: UnitKey = row.id.as_slice().try_into()?;
            let data = decompress(&row.data).context("Failed to decompress report unit data")?;
            let hash: UnitKey = blake3::hash(data.as_ref()).into();
            if hash != key {
                bail!("Report unit data hash mismatch for unit {}", idx);
            }
            let unit = ReportUnit::decode(data.as_ref()).context("Failed to decode report unit")?;
            report.units.push(unit);
        }
        report.migrate()?;
        let report = Arc::new(report);
        self.report_cache.insert(key, report.clone()).await;
        Ok(Some(ReportFile { project, commit, version, report }))
    }

    pub async fn report_exists(&self, owner: &str, repo: &str, commit: &str) -> Result<bool> {
        let mut conn = self.pool.acquire().await?;
        let exists = sqlx::query!(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM reports JOIN projects ON reports.project_id = projects.id
                WHERE projects.owner = ? COLLATE NOCASE AND projects.repo = ? COLLATE NOCASE
                      AND git_commit = ? COLLATE NOCASE
            ) AS "exists!"
            "#,
            owner,
            repo,
            commit
        )
        .fetch_one(&mut *conn)
        .await?
        .exists
            != 0;
        Ok(exists)
    }

    pub async fn get_project_info(
        &self,
        owner: &str,
        repo: &str,
        commit: Option<&str>,
    ) -> Result<Option<ProjectInfo>> {
        let mut conn = self.pool.acquire().await?;
        let project = match sqlx::query!(
            r#"
            SELECT id AS "id!", owner, repo, name, short_name, default_version, platform
            FROM projects
            WHERE owner = ? COLLATE NOCASE AND repo = ? COLLATE NOCASE
            "#,
            owner,
            repo
        )
        .fetch_optional(&mut *conn)
        .await?
        {
            Some(row) => Project {
                id: row.id as u64,
                owner: row.owner,
                repo: row.repo,
                name: row.name,
                short_name: row.short_name,
                default_version: row.default_version,
                platform: row.platform,
            },
            None => return Ok(None),
        };
        let project_id = project.id as i64;
        struct ReportInfo {
            git_commit: String,
            timestamp: chrono::NaiveDateTime,
            version: String,
        }
        let reports = if let Some(commit) = commit {
            // Fetch all reports for the specified commit
            sqlx::query!(
                r#"
                SELECT git_commit, timestamp, version
                FROM reports
                WHERE project_id = ? AND git_commit = ? COLLATE NOCASE
                ORDER BY version
                "#,
                project_id,
                commit,
            )
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|row| ReportInfo {
                git_commit: row.git_commit,
                timestamp: row.timestamp,
                version: row.version,
            })
            .collect::<Vec<_>>()
        } else {
            // Fetch the latest report for each version
            sqlx::query!(
                r#"
                SELECT git_commit, timestamp, version
                FROM reports
                WHERE project_id = ? AND timestamp = (
                    SELECT MAX(timestamp)
                    FROM reports
                    WHERE project_id = ?
                )
                ORDER BY version
                "#,
                project_id,
                project_id,
            )
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|row| ReportInfo {
                git_commit: row.git_commit,
                timestamp: row.timestamp,
                version: row.version,
            })
            .collect::<Vec<_>>()
        };
        let Some(first_report) = reports.first() else {
            return Ok(None);
        };
        // Fetch previous and next commits
        let timestamp = first_report.timestamp.and_utc();
        let prev_commit = sqlx::query!(
            r#"
            SELECT git_commit
            FROM reports
            WHERE project_id = ? AND timestamp < ?
            ORDER BY timestamp DESC
            LIMIT 1
            "#,
            project_id,
            timestamp,
        )
        .fetch_optional(&mut *conn)
        .await?
        .map(|row| row.git_commit);
        let next_commit = sqlx::query!(
            r#"
            SELECT git_commit
            FROM reports
            WHERE project_id = ? AND timestamp > ?
            ORDER BY timestamp
            LIMIT 1
            "#,
            project_id,
            timestamp,
        )
        .fetch_optional(&mut *conn)
        .await?
        .map(|row| row.git_commit);
        Ok(Some(ProjectInfo {
            project,
            commit: Commit {
                sha: first_report.git_commit.clone(),
                timestamp: first_report.timestamp.and_utc(),
            },
            report_versions: reports.iter().map(|r| r.version.clone()).collect(),
            prev_commit,
            next_commit,
        }))
    }

    pub async fn get_projects(&self) -> Result<Vec<ProjectInfo>> {
        let mut conn = self.pool.acquire().await?;
        let projects = sqlx::query!(
            r#"
            SELECT
                projects.id AS "project_id!",
                owner AS "owner!",
                repo AS "repo!",
                name,
                short_name,
                default_version,
                platform,
                git_commit AS "git_commit!",
                MAX(timestamp) AS "timestamp: chrono::NaiveDateTime",
                JSON_GROUP_ARRAY(version ORDER BY version) AS versions
            FROM projects JOIN reports ON projects.id = reports.project_id
            WHERE timestamp = (
                SELECT MAX(timestamp)
                FROM reports
                WHERE project_id = projects.id
            )
            GROUP BY projects.id
            ORDER BY MAX(timestamp) DESC
            "#,
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|row| ProjectInfo {
            project: Project {
                id: row.project_id as u64,
                owner: row.owner,
                repo: row.repo,
                name: row.name,
                short_name: row.short_name,
                default_version: row.default_version,
                platform: row.platform,
            },
            commit: Commit { sha: row.git_commit, timestamp: row.timestamp.and_utc() },
            report_versions: serde_json::from_str(&row.versions).unwrap_or_default(),
            prev_commit: None,
            next_commit: None,
        })
        .collect();
        Ok(projects)
    }

    async fn fixup_report_units(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        for row in sqlx::query!(
            r#"
            SELECT id, data
            FROM report_units
            WHERE name IS NULL
            "#,
        )
        .fetch_all(&mut *conn)
        .await?
        {
            let data = decompress(&row.data).context("Failed to decompress report unit data")?;
            let unit = ReportUnit::decode(data.as_ref()).context("Failed to decode report unit")?;
            sqlx::query!(
                r#"
                UPDATE report_units
                SET name = ?
                WHERE id = ?
                "#,
                unit.name,
                row.id,
            )
            .execute(&mut *conn)
            .await?;
        }
        Ok(())
    }
}

thread_local! {
    pub static COMPRESSOR: RefCell<zstd::bulk::Compressor<'static>> = {
        let mut compressor = zstd::bulk::Compressor::new(1).unwrap();
        // Always include the content size in the compressed data
        compressor.set_parameter(zstd::zstd_safe::CParameter::ContentSizeFlag(true)).unwrap();
        RefCell::new(compressor)
    };
    pub static DECOMPRESSOR: RefCell<zstd::bulk::Decompressor<'static>> = {
        RefCell::new(zstd::bulk::Decompressor::new().unwrap())
    };
}

fn compress(data: &[u8]) -> Vec<u8> { COMPRESSOR.with_borrow_mut(|z| z.compress(data).unwrap()) }

fn decompress(data: &[u8]) -> Result<Cow<[u8]>> {
    match zstd::zstd_safe::get_frame_content_size(data) {
        Ok(Some(size)) => {
            Ok(Cow::Owned(DECOMPRESSOR.with_borrow_mut(|z| z.decompress(data, size as usize))?))
        }
        Ok(None) => Err(anyhow!("Decompressed data size is unknown")),
        Err(_) => Ok(Cow::Borrowed(data)), // Assume uncompressed
    }
}
