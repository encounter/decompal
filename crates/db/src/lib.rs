use core::mem;
use std::{borrow::Cow, cell::RefCell, collections::HashMap, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use decomp_dev_core::{
    config::DbConfig,
    models::{
        CachedReport, CachedReportFile, Commit, FrogressMapping, FullReport, FullReportFile,
        ImageId, Project, ProjectInfo, UnitKey,
    },
};
use futures_util::TryStreamExt;
use moka::future::Cache;
use objdiff_core::bindings::report::{REPORT_VERSION, Report, ReportUnit};
use prost::Message;
use sqlx::{
    Connection, Executor, Pool, Row, Sqlite, SqliteConnection, SqlitePool, migrate::MigrateDatabase,
};
use time::{OffsetDateTime, UtcDateTime, macros::format_description};

#[derive(Clone)]
pub struct Database {
    pub pool: Pool<Sqlite>,
    report_cache: Cache<ReportKey, CachedReportFile>,
    report_unit_cache: Cache<UnitKey, Arc<ReportUnit>>,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct ReportKey {
    project_id: u64,
    commit: String,
    version: String,
}

// Maximum number of bind parameters in a single query (SQLite limit)
const BIND_LIMIT: usize = 32766;

impl Database {
    pub async fn new(config: &DbConfig) -> Result<Arc<Self>> {
        if !Sqlite::database_exists(&config.url).await.unwrap_or(false) {
            tracing::info!(url = %config.url, "Creating database");
            Sqlite::create_database(&config.url).await.context("Failed to create database")?;
            tracing::info!("Database created");
        }
        let pool =
            SqlitePool::connect(&config.url).await.context("Failed to connect to database")?;
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .context("Failed to run database migrations")?;
        let report_cache = Cache::<ReportKey, CachedReportFile>::builder()
            .max_capacity(8192)
            .eviction_listener(|k, _v, _cause| {
                tracing::debug!(
                    "Evicting report from cache: {}@{}:{}",
                    k.project_id,
                    k.commit,
                    k.version
                );
            })
            .build();
        let report_unit_cache = Cache::<UnitKey, Arc<ReportUnit>>::builder()
            .weigher(|_, v| v.encoded_len() as u32)
            .max_capacity(512 * 1024 * 1024) // 512 MB
            .eviction_listener(|k, _v, _cause| {
                tracing::debug!("Evicting report unit from cache: {:?}", hex::encode(k.as_ref()));
            })
            .build();
        let db = Self { pool, report_cache, report_unit_cache };
        db.fixup_report_units().await.context("Fixing report units")?;
        db.migrate_reports().await.context("Migrating reports")?;
        // db.cleanup_report_units().await.context("Running report cleanup")?;
        Ok(Arc::new(db))
    }

    pub async fn close(&self) { self.pool.close().await }

    pub async fn insert_report(
        &self,
        project: &Project,
        commit: &Commit,
        version: &str,
        mut report: Box<Report>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let project_id = project.id as i64;
        let header_image_id = project.header_image_id.as_ref().map(|b| b.as_slice());
        let pr_report_style = project.pr_report_style.as_str();
        sqlx::query!(
            r#"
            INSERT INTO projects (id, owner, repo, name, short_name, default_category, default_version, platform, workflow_id, enable_pr_comments, pr_report_style, header_image_id, enabled, permanently_disabled, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            ON CONFLICT (id) DO NOTHING
            "#,
            project_id,
            project.owner,
            project.repo,
            project.name,
            project.short_name,
            project.default_category,
            project.default_version,
            project.platform,
            project.workflow_id,
            project.enable_pr_comments,
            pr_report_style,
            header_image_id,
            project.enabled,
            project.permanently_disabled,
        )
            .execute(&mut *tx)
            .await?;
        report.migrate()?;
        let units = mem::take(&mut report.units);
        let data = compress(&report.encode_to_vec());
        let timestamp = to_primitive_date_time(commit.timestamp);
        let report_id = sqlx::query!(
            r#"
            INSERT INTO reports (project_id, version, git_commit, git_commit_message, timestamp, data, data_version)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (project_id, version COLLATE NOCASE, git_commit COLLATE NOCASE) DO UPDATE
            SET timestamp = EXCLUDED.timestamp
            RETURNING id
            "#,
            project_id,
            version,
            commit.sha,
            commit.message,
            timestamp,
            data,
            report.version,
        )
            .fetch_one(&mut *tx)
            .await?
            .id;
        Self::insert_report_units(&mut tx, &units, report_id).await?;
        tx.commit().await?;
        // self.report_cache
        //     .insert(
        //         ReportKey {
        //             project_id: project.id,
        //             commit: commit.sha.to_ascii_lowercase(),
        //             version: version.to_ascii_lowercase(),
        //         },
        //         file.clone(),
        //     )
        //     .await;
        Ok(())
    }

    async fn insert_report_units(
        conn: &mut SqliteConnection,
        units: &[ReportUnit],
        report_id: i64,
    ) -> Result<()> {
        let mut keys = Vec::with_capacity(units.len());
        for chunk in units.chunks(BIND_LIMIT / 3) {
            let mut builder =
                sqlx::QueryBuilder::<Sqlite>::new("INSERT INTO report_units (id, data, name) ");
            builder.push_values(chunk, |mut b, unit| {
                let mut data = unit.encode_to_vec();
                let key: UnitKey = blake3::hash(&data).into();
                keys.push(key);
                data = compress(&data);
                b.push_bind(key.to_vec()).push_bind(data).push_bind(&unit.name);
            });
            builder.push(" ON CONFLICT (id) DO NOTHING");
            conn.execute(builder.build()).await?;
        }
        let mut unit_index = 0;
        for chunk in keys.chunks(BIND_LIMIT / 3) {
            let mut builder = sqlx::QueryBuilder::<Sqlite>::new(
                "INSERT INTO report_report_units (report_id, report_unit_id, unit_index) ",
            );
            builder.push_values(chunk, |mut b, key| {
                b.push_bind(report_id).push_bind(key.as_slice()).push_bind(unit_index);
                unit_index += 1;
            });
            conn.execute(builder.build()).await?;
        }
        Ok(())
    }

    pub async fn get_versions_for_commit(
        &self,
        project_id: u64,
        commit: &str,
    ) -> Result<Vec<String>> {
        let mut conn = self.pool.acquire().await?;
        let project_id_db = project_id as i64;
        let versions = sqlx::query!(
            r#"
            SELECT version
            FROM reports JOIN projects ON reports.project_id = projects.id
            WHERE projects.id = ? AND git_commit = ? COLLATE NOCASE
            ORDER BY version
            "#,
            project_id_db,
            commit
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|row| row.version)
        .collect();
        Ok(versions)
    }

    pub async fn get_report(
        &self,
        project_id: u64,
        commit: &str,
        version: &str,
    ) -> Result<Option<CachedReportFile>> {
        let key = ReportKey {
            project_id,
            commit: commit.to_ascii_lowercase(),
            version: version.to_ascii_lowercase(),
        };
        if let Some(report) = self.report_cache.get(&key).await {
            return Ok(Some(report));
        }
        let mut conn = self.pool.acquire().await?;
        let project_id_db = project_id as i64;
        let (report_id, commit, version, mut report) = match sqlx::query!(
            r#"
            SELECT
                reports.id as "report_id!",
                git_commit,
                git_commit_message,
                timestamp,
                version,
                data
            FROM reports JOIN projects ON reports.project_id = projects.id
            WHERE projects.id = ? AND version = ? COLLATE NOCASE AND git_commit = ? COLLATE NOCASE
            "#,
            project_id_db,
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
                    Commit {
                        sha: row.git_commit,
                        message: row.git_commit_message,
                        timestamp: row.timestamp.to_utc(),
                    },
                    row.version,
                    CachedReport {
                        version: report.version,
                        measures: report.measures.unwrap_or_default(),
                        units: vec![],
                        categories: report.categories.clone(),
                    },
                )
            }
            None => return Ok(None),
        };
        let mut stream = sqlx::query!(
            r#"
            SELECT ru.id AS "id!", ru.data, rru.unit_index
            FROM report_report_units rru JOIN report_units ru ON rru.report_unit_id = ru.id
            WHERE rru.report_id = ?
            ORDER BY rru.unit_index
            "#,
            report_id
        )
        .fetch(&mut *conn);
        while let Some(row) = stream.try_next().await? {
            let idx = row.unit_index as usize;
            if idx != report.units.len() {
                bail!("Report unit index mismatch: {} but expected {}", idx, report.units.len());
            }
            let key: UnitKey = row.id.as_slice().try_into()?;
            report.units.push(key);
        }
        let report_file = CachedReportFile {
            commit: commit.clone(),
            version: version.clone(),
            report: Arc::new(report),
        };
        self.report_cache.insert(key, report_file.clone()).await;
        Ok(Some(report_file))
    }

    pub async fn upgrade_report(&self, file: &CachedReportFile) -> Result<FullReportFile> {
        let mut units = Vec::with_capacity(file.report.units.len());
        let mut missing_unit_keys = Vec::with_capacity(file.report.units.len());
        let mut missing_unit_idx = HashMap::<UnitKey, Vec<usize>>::new();
        for (idx, &key) in file.report.units.iter().enumerate() {
            if let Some(unit) = self.report_unit_cache.get(&key).await {
                units.push(Some(unit));
            } else {
                units.push(None);
                missing_unit_keys.push(key);
                missing_unit_idx.entry(key).or_default().push(idx);
            }
        }
        if !missing_unit_keys.is_empty() {
            let mut conn = self.pool.acquire().await?;
            for chunk in missing_unit_keys.chunks(BIND_LIMIT) {
                let mut builder = sqlx::QueryBuilder::<Sqlite>::new(
                    "SELECT ru.id, ru.data FROM report_units ru WHERE ru.id IN (",
                );
                let mut separated = builder.separated(", ");
                for key in chunk {
                    separated.push_bind(&key[..]);
                }
                separated.push_unseparated(")");
                let mut stream = conn.fetch(builder.build());
                while let Some(row) = stream.try_next().await? {
                    let row_id: Box<[u8]> = row.get(0);
                    let row_data: Box<[u8]> = row.get(1);
                    let key = row_id.as_ref().try_into()?;
                    let data =
                        decompress(&row_data).context("Failed to decompress report unit data")?;
                    // Skip hash check since we're rehydrating a cached report
                    // Units were checked when the report was initially loaded
                    let unit = Arc::new(
                        ReportUnit::decode(data.as_ref())
                            .context("Failed to decode report unit")?,
                    );
                    self.report_unit_cache.insert(key, unit.clone()).await;
                    if let Some(v) = missing_unit_idx.get(&key) {
                        for &idx in v {
                            units[idx] = Some(unit.clone());
                        }
                    } else {
                        tracing::error!("Unexpected unit index: {:?}", hex::encode(key));
                    }
                }
            }
        }
        let mut out_units = Vec::with_capacity(file.report.units.len());
        for (unit, &unit_key) in units.into_iter().zip(&file.report.units) {
            if let Some(unit) = unit {
                out_units.push(unit);
            } else {
                tracing::error!("Failed to load report unit: {}", hex::encode(unit_key));
            }
        }
        Ok(FullReportFile {
            commit: file.commit.clone(),
            version: file.version.clone(),
            report: FullReport {
                version: file.report.version,
                measures: file.report.measures,
                units: out_units,
                categories: file.report.categories.clone(),
            },
        })
    }

    pub async fn report_exists(&self, project_id: u64, commit: &str) -> Result<bool> {
        let mut conn = self.pool.acquire().await?;
        let project_id_db = project_id as i64;
        let exists = sqlx::query!(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM reports JOIN projects ON reports.project_id = projects.id
                WHERE projects.id = ? AND git_commit = ? COLLATE NOCASE
            ) AS "exists!"
            "#,
            project_id_db,
            commit
        )
        .fetch_one(&mut *conn)
        .await?
        .exists
            != 0;
        Ok(exists)
    }

    pub async fn project_exists(&self, project_id: u64) -> Result<bool> {
        let mut conn = self.pool.acquire().await?;
        let project_id_db = project_id as i64;
        let exists = sqlx::query!(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM projects
                WHERE id = ?
            ) AS "exists!"
            "#,
            project_id_db,
        )
        .fetch_one(&mut *conn)
        .await?
        .exists
            != 0;
        Ok(exists)
    }

    pub async fn get_project(&self, owner: &str, repo: &str) -> Result<Option<Project>> {
        let mut conn = self.pool.acquire().await?;
        self.get_project_inner(&mut conn, owner, repo).await
    }

    pub async fn get_project_by_id(&self, project_id: u64) -> Result<Option<Project>> {
        let mut conn = self.pool.acquire().await?;
        self.get_project_by_id_inner(&mut conn, project_id).await
    }

    async fn get_project_inner(
        &self,
        conn: &mut SqliteConnection,
        owner: &str,
        repo: &str,
    ) -> Result<Option<Project>> {
        Ok(sqlx::query!(
            r#"
            SELECT id AS "id!", owner, repo, name, short_name, default_category, default_version, platform, workflow_id, enable_pr_comments, pr_report_style AS "pr_report_style!", header_image_id, enabled, permanently_disabled
            FROM projects
            WHERE owner = ? COLLATE NOCASE AND repo = ? COLLATE NOCASE
            "#,
            owner,
            repo
        )
        .fetch_optional(&mut *conn)
        .await?
        .map(|row| {
            Project {
                id: row.id as u64,
                owner: row.owner,
                repo: row.repo,
                name: row.name,
                short_name: row.short_name,
                default_category: row.default_category,
                default_version: row.default_version,
                platform: row.platform,
                workflow_id: row.workflow_id,
                enable_pr_comments: row.enable_pr_comments,
                pr_report_style: row.pr_report_style.parse().unwrap_or_default(),
                header_image_id: row.header_image_id.and_then(|b| b.try_into().ok()),
                enabled: row.enabled,
                permanently_disabled: row.permanently_disabled,
            }
        }))
    }

    async fn get_project_by_id_inner(
        &self,
        conn: &mut SqliteConnection,
        project_id: u64,
    ) -> Result<Option<Project>> {
        let project_id_db = project_id as i64;
        Ok(sqlx::query!(
            r#"
            SELECT id AS "id!", owner, repo, name, short_name, default_category, default_version, platform, workflow_id, enable_pr_comments, pr_report_style AS "pr_report_style!", header_image_id, enabled, permanently_disabled
            FROM projects
            WHERE id = ?
            "#,
            project_id_db
        )
        .fetch_optional(&mut *conn)
        .await?
        .map(|row| {
            Project {
                id: row.id as u64,
                owner: row.owner,
                repo: row.repo,
                name: row.name,
                short_name: row.short_name,
                default_category: row.default_category,
                default_version: row.default_version,
                platform: row.platform,
                workflow_id: row.workflow_id,
                enable_pr_comments: row.enable_pr_comments,
                pr_report_style: row.pr_report_style.parse().unwrap_or_default(),
                header_image_id: row.header_image_id.and_then(|b| b.try_into().ok()),
                enabled: row.enabled,
                permanently_disabled: row.permanently_disabled,
            }
        }))
    }

    pub async fn get_project_info(
        &self,
        owner: &str,
        repo: &str,
        commit: Option<&str>,
    ) -> Result<Option<ProjectInfo>> {
        let mut conn = self.pool.acquire().await?;
        let Some(project) = self.get_project_inner(&mut conn, owner, repo).await? else {
            return Ok(None);
        };
        self.get_project_info_inner(&mut conn, project, commit).await
    }

    pub async fn get_project_info_by_id(
        &self,
        project_id: u64,
        commit: Option<&str>,
    ) -> Result<Option<ProjectInfo>> {
        let mut conn = self.pool.acquire().await?;
        let Some(project) = self.get_project_by_id_inner(&mut conn, project_id).await? else {
            return Ok(None);
        };
        self.get_project_info_inner(&mut conn, project, commit).await
    }

    async fn get_project_info_inner(
        &self,
        conn: &mut SqliteConnection,
        project: Project,
        commit: Option<&str>,
    ) -> Result<Option<ProjectInfo>> {
        let project_id = project.id as i64;
        struct ReportInfo {
            git_commit: String,
            git_commit_message: Option<String>,
            timestamp: OffsetDateTime,
            version: String,
        }
        let reports = if let Some(commit) = commit {
            // Fetch all reports for the specified commit
            sqlx::query!(
                r#"
                SELECT git_commit, git_commit_message, timestamp, version
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
                git_commit_message: row.git_commit_message,
                timestamp: row.timestamp,
                version: row.version,
            })
            .collect::<Vec<_>>()
        } else {
            // Fetch the latest report for each version
            sqlx::query!(
                r#"
                SELECT git_commit, git_commit_message, timestamp, version
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
                git_commit_message: row.git_commit_message,
                timestamp: row.timestamp,
                version: row.version,
            })
            .collect::<Vec<_>>()
        };
        let mut info = ProjectInfo {
            project,
            commit: None,
            report_versions: reports.iter().map(|r| r.version.clone()).collect(),
            prev_commit: None,
            next_commit: None,
        };
        if let Some(first_report) = reports.first() {
            // Fetch previous and next commits
            let timestamp = to_primitive_date_time(first_report.timestamp.to_utc());
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

            info.commit = Some(Commit {
                sha: first_report.git_commit.clone(),
                timestamp: first_report.timestamp.to_utc(),
                message: first_report.git_commit_message.clone(),
            });
            info.prev_commit = prev_commit;
            info.next_commit = next_commit;
        }
        Ok(Some(info))
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
                default_category,
                default_version,
                platform,
                workflow_id,
                enable_pr_comments AS "enable_pr_comments!",
                pr_report_style AS "pr_report_style!",
                header_image_id,
                enabled AS "enabled!",
                permanently_disabled AS "permanently_disabled!",
                git_commit,
                git_commit_message,
                MAX(timestamp) AS "timestamp: time::OffsetDateTime",
                JSON_GROUP_ARRAY(version ORDER BY version)
                    FILTER (WHERE version IS NOT NULL) AS versions
            FROM projects LEFT JOIN reports ON (
                reports.project_id = projects.id
                AND reports.timestamp = (
                    SELECT MAX(timestamp)
                    FROM reports
                    WHERE project_id = projects.id
                )
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
                default_category: row.default_category,
                default_version: row.default_version,
                platform: row.platform,
                workflow_id: row.workflow_id,
                enable_pr_comments: row.enable_pr_comments,
                pr_report_style: row.pr_report_style.parse().unwrap_or_default(),
                header_image_id: row.header_image_id.and_then(|b| b.try_into().ok()),
                enabled: row.enabled,
                permanently_disabled: row.permanently_disabled,
            },
            commit: match (row.git_commit, row.timestamp) {
                (Some(sha), Some(timestamp)) => Some(Commit {
                    sha,
                    timestamp: timestamp.to_utc(),
                    message: row.git_commit_message,
                }),
                _ => None,
            },
            report_versions: row
                .versions
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default(),
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

    pub async fn fetch_all_reports(
        &self,
        project: &Project,
        version: &str,
    ) -> Result<Vec<CachedReportFile>> {
        let mut conn = self.pool.acquire().await?;
        let project_id_db = project.id as i64;
        let commits = sqlx::query!(
            r#"
            SELECT git_commit, git_commit_message, timestamp
            FROM reports
            WHERE project_id = ? AND version = ? COLLATE NOCASE
            ORDER BY timestamp DESC
            "#,
            project_id_db,
            version
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|row| Commit {
            sha: row.git_commit,
            timestamp: row.timestamp.to_utc(),
            message: row.git_commit_message,
        })
        .collect::<Vec<_>>();
        let mut reports = Vec::with_capacity(commits.len());
        for commit in commits {
            let report = self.get_report(project.id, &commit.sha, version).await?;
            if let Some(report) = report {
                reports.push(report);
            } else {
                bail!(
                    "Report not found for {}/{}@{}:{}",
                    project.owner,
                    project.repo,
                    commit.sha,
                    version
                );
            }
        }
        Ok(reports)
    }

    async fn migrate_reports(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut stream = sqlx::query!(
            r#"
            SELECT id, data
            FROM reports
            WHERE data_version < ?
            "#,
            REPORT_VERSION,
        )
        .fetch(&mut *conn);
        let mut reports = Vec::new();
        while let Some(row) = stream.try_next().await? {
            let report_id = row.id;
            let data = decompress(&row.data).context("Failed to decompress report data")?;
            let report = Report::decode(data.as_ref()).context("Failed to decode report")?;
            reports.push((report_id, report));
        }
        drop(stream);
        for (report_id, mut report) in reports {
            if report.version == REPORT_VERSION {
                // Report is already up-to-date
                sqlx::query!(
                    r#"
                    UPDATE reports
                    SET data_version = ?
                    WHERE id = ?
                    "#,
                    REPORT_VERSION,
                    report_id,
                )
                .execute(&mut *conn)
                .await?;
                continue;
            }
            tracing::info!("Migrating report {} from version {}", report_id, report.version);
            // Fetch all report units
            let mut unit_stream = sqlx::query!(
                r#"
                SELECT ru.id AS "id!", ru.data
                FROM report_report_units rru JOIN report_units ru ON rru.report_unit_id = ru.id
                WHERE rru.report_id = ?
                ORDER BY rru.unit_index
                "#,
                report_id,
            )
            .fetch(&mut *conn);
            while let Some(unit_row) = unit_stream.try_next().await? {
                let data =
                    decompress(&unit_row.data).context("Failed to decompress report unit data")?;
                let unit =
                    ReportUnit::decode(data.as_ref()).context("Failed to decode report unit")?;
                report.units.push(unit);
            }
            drop(unit_stream);
            // Migrate report
            report.migrate()?;
            let units = mem::take(&mut report.units);
            let data = compress(&report.encode_to_vec());
            // Persist updated report
            let mut tx = conn.begin().await?;
            sqlx::query!(
                r#"
                UPDATE reports
                SET data = ?, data_version = ?
                WHERE id = ?
                "#,
                data,
                REPORT_VERSION,
                report_id,
            )
            .execute(&mut *tx)
            .await?;
            // Delete existing report units
            sqlx::query!(
                r#"
                DELETE FROM report_report_units
                WHERE report_id = ?
                "#,
                report_id,
            )
            .execute(&mut *tx)
            .await?;
            // Insert updated report units
            Self::insert_report_units(&mut tx, &units, report_id).await?;
            tx.commit().await?;
        }
        Ok(())
    }

    pub async fn cleanup_report_units(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        let deleted_reports = sqlx::query!(
            r#"
            DELETE FROM reports
            WHERE project_id NOT IN (SELECT id FROM projects)
            "#,
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let deleted_report_units = sqlx::query!(
            r#"
            DELETE FROM report_units
            WHERE id NOT IN (SELECT report_unit_id FROM report_report_units)
            "#,
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        if deleted_reports > 0 || deleted_report_units > 0 {
            tracing::info!(
                "Deleted {} orphaned reports and {} orphaned report units",
                deleted_reports,
                deleted_report_units,
            );
        }
        Ok(())
    }

    pub async fn get_frogress_mappings(&self) -> Result<Vec<FrogressMapping>> {
        let mut conn = self.pool.acquire().await?;
        let mappings = sqlx::query!(
            r#"
            SELECT frogress_slug,
                   frogress_version,
                   frogress_category,
                   frogress_measure,
                   project_id,
                   version,
                   category,
                   category_name,
                   measure
            FROM frogress_mappings
            ORDER BY frogress_slug, frogress_version, frogress_category
            "#,
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|row| FrogressMapping {
            frogress_slug: row.frogress_slug,
            frogress_version: row.frogress_version,
            frogress_category: row.frogress_category,
            frogress_measure: row.frogress_measure,
            project_id: row.project_id as u64,
            project_version: row.version,
            project_category: row.category,
            project_category_name: row.category_name,
            project_measure: row.measure,
        })
        .collect();
        Ok(mappings)
    }

    pub async fn update_report_message(
        &self,
        project_id: u64,
        commit: &str,
        message: &str,
    ) -> Result<()> {
        let count = {
            let mut conn = self.pool.acquire().await?;
            let project_id_db = project_id as i64;
            sqlx::query!(
                r#"
                UPDATE reports
                SET git_commit_message = ?
                WHERE project_id = ? AND git_commit = ? COLLATE NOCASE
                "#,
                message,
                project_id_db,
                commit,
            )
            .execute(&mut *conn)
            .await?
            .rows_affected()
        };
        if count == 0 {
            bail!("Report not found for project ID {} and commit {}", project_id, commit);
        }
        let keys = {
            let mut conn = self.pool.acquire().await?;
            let project_id_db = project_id as i64;
            let result = sqlx::query!(
                r#"
                SELECT version FROM reports WHERE project_id = ? AND git_commit = ? COLLATE NOCASE
                "#,
                project_id_db,
                commit,
            )
            .fetch_all(&mut *conn)
            .await?;
            result
                .iter()
                .map(|r| ReportKey {
                    project_id,
                    commit: commit.to_ascii_lowercase(),
                    version: r.version.to_ascii_lowercase(),
                })
                .collect::<Vec<_>>()
        };
        for key in keys {
            if let Some(mut report) = self.report_cache.get(&key).await {
                report.commit.message = Some(message.to_owned());
                self.report_cache.insert(key, report).await;
            }
        }
        Ok(())
    }

    pub async fn update_project_workflow_id(
        &self,
        project_id: u64,
        workflow_id: &str,
    ) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let project_id_db = project_id as i64;
        sqlx::query!(
            r#"
            UPDATE projects
            SET workflow_id = ?, updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
            "#,
            workflow_id,
            project_id_db,
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub async fn update_project_owner_repo(
        &self,
        project_id: u64,
        owner: &str,
        repo: &str,
    ) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let project_id_db = project_id as i64;
        sqlx::query!(
            r#"
            UPDATE projects
            SET owner = ?, repo = ?, updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
            "#,
            owner,
            repo,
            project_id_db,
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub async fn update_project(&self, project: &Project) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let project_id = project.id as i64;
        let header_image_id = project.header_image_id.as_ref().map(|b| b.as_slice());
        let pr_report_style = project.pr_report_style.as_str();
        sqlx::query!(
            r#"
            UPDATE projects
            SET owner = ?, repo = ?, name = ?, short_name = ?, default_category = ?, default_version = ?, platform = ?, workflow_id = ?, enable_pr_comments = ?, pr_report_style = ?, header_image_id = ?, enabled = ?, permanently_disabled = ?, updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
            "#,
            project.owner,
            project.repo,
            project.name,
            project.short_name,
            project.default_category,
            project.default_version,
            project.platform,
            project.workflow_id,
            project.enable_pr_comments,
            pr_report_style,
            header_image_id,
            project.enabled,
            project.permanently_disabled,
            project_id,
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub async fn create_project(&self, project: &Project) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let project_id = project.id as i64;
        let header_image_id = project.header_image_id.as_ref().map(|b| b.as_slice());
        let pr_report_style = project.pr_report_style.as_str();
        sqlx::query!(
            r#"
            INSERT INTO projects (id, owner, repo, name, short_name, default_category, default_version, platform, workflow_id, enable_pr_comments, pr_report_style, header_image_id, enabled, permanently_disabled, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            "#,
            project_id,
            project.owner,
            project.repo,
            project.name,
            project.short_name,
            project.default_category,
            project.default_version,
            project.platform,
            project.workflow_id,
            project.enable_pr_comments,
            pr_report_style,
            header_image_id,
            project.enabled,
            project.permanently_disabled,
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub async fn create_image(
        &self,
        mime_type: &str,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Result<ImageId> {
        let mut conn = self.pool.acquire().await?;
        let key: ImageId = blake3::hash(data).into();
        let key_db = &key[..];
        sqlx::query!(
            r#"
            INSERT INTO images (id, mime_type, width, height, data, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            ON CONFLICT (id) DO NOTHING
            "#,
            key_db,
            mime_type,
            width,
            height,
            data,
        )
        .execute(&mut *conn)
        .await?;
        Ok(key)
    }

    pub async fn get_image(&self, id: ImageId) -> Result<Option<(String, u32, u32, Vec<u8>)>> {
        let mut conn = self.pool.acquire().await?;
        let id_db = &id[..];
        let row = sqlx::query!(
            r#"
            SELECT mime_type, width, height, data
            FROM images
            WHERE id = ?
            "#,
            id_db,
        )
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(|row| (row.mime_type, row.width as u32, row.height as u32, row.data)))
    }

    pub async fn update_project_header(&self, project_id: u64, image_id: ImageId) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let image_id = &image_id[..];
        let project_id_db = project_id as i64;
        sqlx::query!(
            r#"
            UPDATE projects
            SET header_image_id = ?, updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
            "#,
            image_id,
            project_id_db,
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub async fn cleanup_images(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        let deleted_images = sqlx::query!(
            r#"
            DELETE FROM images
            WHERE id NOT IN (
                SELECT header_image_id FROM projects
                WHERE header_image_id IS NOT NULL
            )
            "#,
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        if deleted_images > 0 {
            tracing::info!("Deleted {} orphaned images", deleted_images);
        }
        Ok(())
    }

    pub async fn delete_reports_by_commit(
        &self,
        project_id: u64,
        commit_sha: &str,
    ) -> Result<usize> {
        let mut conn = self.pool.acquire().await?;
        let project_id_db = project_id as i64;
        let deleted_count = sqlx::query!(
            r#"
            DELETE FROM reports
            WHERE project_id = ? AND git_commit = ? COLLATE NOCASE
            "#,
            project_id_db,
            commit_sha,
        )
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if deleted_count > 0 {
            tracing::info!(
                "Deleted {} reports for project ID {} commit {}",
                deleted_count,
                project_id,
                commit_sha
            );
        }
        Ok(deleted_count as usize)
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

fn decompress(data: &[u8]) -> Result<Cow<'_, [u8]>> {
    match zstd::zstd_safe::get_frame_content_size(data) {
        Ok(Some(size)) => {
            let mut buffer = Vec::with_capacity(size as usize);
            DECOMPRESSOR.with_borrow_mut(|z| z.decompress_to_buffer(data, &mut buffer))?;
            Ok(Cow::Owned(buffer))
        }
        Ok(None) => Err(anyhow!("Decompressed data size is unknown")),
        Err(_) => Ok(Cow::Borrowed(data)), // Assume uncompressed
    }
}

#[inline]
fn to_primitive_date_time(date: UtcDateTime) -> String {
    date.format(format_description!("[year]-[month]-[day] [hour]:[minute]:[second]")).unwrap()
}
