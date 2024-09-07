use std::{
    ffi::OsStr,
    io::{Cursor, Read},
    sync::{Arc, OnceLock},
};

use anyhow::{anyhow, Context, Result};
use objdiff_core::bindings::report::Report;
use octocrab::{
    models::{ArtifactId, RunId},
    params::actions::ArchiveFormat,
    Octocrab,
};
use regex::Regex;
use tokio::{sync::Semaphore, task::JoinSet};

use crate::{
    config::AppConfig,
    models::{Commit, Project, ReportFile},
    AppState,
};

pub type Client = Octocrab;

pub async fn create(config: &AppConfig) -> Result<Octocrab> {
    let client = Octocrab::builder()
        .personal_token(config.github_token.clone())
        .build()
        .context("Failed to create GitHub client")?;
    octocrab::initialise(client.clone());
    let profile = client.current().user().await.context("Failed to fetch current user")?;
    tracing::info!("Logged in as {}", profile.login);
    Ok(client)
}

pub async fn run(state: &mut AppState, owner: &str, repo: &str, stop_run_id: u64) -> Result<()> {
    tracing::info!("Refreshing project {}/{}", owner, repo);
    let existing = state.db.get_project_info(owner, repo, None).await?;
    let repo = state.github.repos(owner, repo).get().await.context("Failed to fetch repo")?;
    let branch = repo.default_branch.as_deref().unwrap_or("main");
    let Some(owner) = repo.owner else {
        return Err(anyhow!("Repo has no owner"));
    };
    let project = existing.as_ref().map(|e| e.project.clone()).unwrap_or_else(|| Project {
        id: repo.id.0,
        owner: owner.login.clone(),
        repo: repo.name.clone(),
        name: None,
        short_name: None,
        default_version: None,
    });

    let mut runs = vec![];
    let mut page = 1u32;
    'outer: loop {
        let result = state
            .github
            .workflows(&project.owner, &project.repo)
            .list_runs("build.yml")
            .branch(branch)
            .event("push")
            .status("completed")
            .exclude_pull_requests(true)
            .page(page)
            // .per_page(10)
            .send()
            .await?;
        if result.items.is_empty() {
            break;
        }
        for run in result.items {
            if let Some(existing) = existing.as_ref() {
                if run.head_sha == existing.commit.sha {
                    break 'outer;
                }
            }
            let run_id = run.id;
            runs.push(run);
            if run_id == RunId(stop_run_id) {
                break 'outer;
            }
        }
        page += 1;
    }
    tracing::info!("Fetched {} runs", runs.len());

    struct TaskResult {
        run_id: RunId,
        commit: Commit,
        result: Result<ProcessWorkflowRunResult>,
    }
    let sem = Arc::new(Semaphore::new(10));
    let mut set = JoinSet::new();
    for run in runs {
        let sem = sem.clone();
        let project = project.clone();
        let github = state.github.clone();
        let db = state.db.clone();
        let run_id = run.id;
        let commit = Commit::from(&run.head_commit);
        set.spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            match db.report_exists(&project.owner, &project.repo, &commit.sha).await {
                Ok(true) => {
                    return TaskResult {
                        run_id,
                        commit,
                        result: Ok(ProcessWorkflowRunResult { artifacts: vec![] }),
                    };
                }
                Ok(false) => {}
                Err(e) => return TaskResult { run_id, commit, result: Err(e) },
            }
            let result = process_workflow_run(github, project, run.id).await;
            TaskResult { run_id, commit, result }
        });
    }
    while let Some(join_result) = set.join_next().await {
        match join_result {
            Ok(TaskResult {
                run_id,
                commit,
                result: Ok(ProcessWorkflowRunResult { artifacts }),
            }) => {
                tracing::debug!(
                    "Processed workflow run {} ({}) (artifacts {})",
                    run_id,
                    commit.sha,
                    artifacts.len()
                );
                for artifact in artifacts {
                    let file = ReportFile {
                        project: project.clone(),
                        commit: commit.clone(),
                        version: artifact.version,
                        report: artifact.report,
                    };
                    let start = std::time::Instant::now();
                    state.db.insert_report(&file).await?;
                    let duration = start.elapsed();
                    tracing::info!(
                        "Inserted report {} ({}) in {}ms",
                        file.version,
                        file.commit.sha,
                        duration.as_millis()
                    );
                }
            }
            Ok(TaskResult { run_id, commit, result: Err(e) }) => {
                tracing::error!(
                    "Failed to process workflow run {} ({}): {:?}",
                    run_id,
                    commit.sha,
                    e
                );
            }
            Err(e) => {
                tracing::error!("Failed to process workflow run: {:?}", e);
            }
        }
    }
    Ok(())
}

struct ProcessWorkflowRunResult {
    artifacts: Vec<ProcessArtifactResult>,
}

struct ProcessArtifactResult {
    version: String,
    report: Arc<Report>,
}

async fn process_workflow_run(
    github: Client,
    project: Project,
    run_id: RunId,
) -> Result<ProcessWorkflowRunResult> {
    let artifacts = github
        .all_pages(
            github
                .actions()
                .list_workflow_run_artifacts(&project.owner, &project.repo, run_id)
                .send()
                .await
                .context("Failed to fetch artifacts")?
                .value
                .unwrap(),
        )
        .await?;
    tracing::debug!("Run {} (artifacts {})", run_id, artifacts.len());
    let mut result = ProcessWorkflowRunResult { artifacts: vec![] };
    if artifacts.is_empty() {
        return Ok(result);
    }
    static REGEX: OnceLock<Regex> = OnceLock::new();
    let regex = REGEX
        .get_or_init(|| Regex::new(r"^(?P<version>[A-z0-9_\-]+)[_-]report(?:[_-].*)?$").unwrap());
    let sem = Arc::new(Semaphore::new(3));
    let mut set = JoinSet::new();
    struct TaskResult {
        artifact_name: String,
        result: DownloadArtifactResult,
    }
    for artifact in &artifacts {
        let artifact_name = artifact.name.clone();
        let version =
            if let Some(version) = regex.captures(&artifact_name).and_then(|c| c.name("version")) {
                version.as_str().to_string()
            } else if artifact_name == "progress" || artifact_name == "progress.json" {
                // bfbb compatibility
                static MAPS_REGEX: OnceLock<Regex> = OnceLock::new();
                let maps_regex = MAPS_REGEX
                    .get_or_init(|| Regex::new(r"^(?P<version>[A-z0-9_\-]+)_maps$").unwrap());
                if let Some(version) = artifacts.iter().find_map(|a| {
                    maps_regex
                        .captures(&a.name)
                        .and_then(|c| c.name("version"))
                        .map(|m| m.as_str().to_string())
                }) {
                    version
                } else {
                    continue;
                }
            } else {
                continue;
            };
        let sem = sem.clone();
        let github = github.clone();
        let project = project.clone();
        let artifact_id = artifact.id;
        set.spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            TaskResult {
                artifact_name,
                result: download_artifact(github, project, artifact_id, version).await,
            }
        });
    }
    while let Some(join_result) = set.join_next().await {
        match join_result {
            Ok(TaskResult { artifact_name: name, result: Ok(reports) }) => {
                if reports.is_empty() {
                    tracing::warn!("No report found in artifact {}", name);
                } else {
                    for (version, report) in reports {
                        tracing::info!("Processed artifact {} ({})", name, version);
                        result.artifacts.push(ProcessArtifactResult { version, report });
                    }
                }
            }
            Ok(TaskResult { artifact_name: name, result: Err(e) }) => {
                tracing::error!("Failed to process artifact {}: {:?}", name, e);
            }
            Err(e) => {
                tracing::error!("Failed to process artifact: {:?}", e);
            }
        }
    }
    Ok(result)
}

type DownloadArtifactResult = Result<Vec<(String, Arc<Report>)>>;

async fn download_artifact(
    github: Client,
    project: Project,
    artifact_id: ArtifactId,
    version: String,
) -> DownloadArtifactResult {
    let bytes = github
        .actions()
        .download_artifact(&project.owner, &project.repo, artifact_id, ArchiveFormat::Zip)
        .await?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let Some(path) = file.enclosed_name() else {
            continue;
        };
        if path.file_stem() == Some(OsStr::new("report"))
            || path.file_stem() == Some(OsStr::new("progress"))
        {
            let mut contents = Vec::with_capacity(file.size() as usize);
            file.read_to_end(&mut contents)?;
            let mut report = Report::parse(&contents)?;
            report.migrate()?;
            // Split combined reports into individual reports TODO
            // if version.eq_ignore_ascii_case("combined") {
            //     return Ok(report
            //         .split()
            //         .into_iter()
            //         .map(|(version, report)| (version, Arc::new(report)))
            //         .collect());
            // }
            return Ok(vec![(version, Arc::new(report))]);
        }
    }
    Ok(vec![])
}
