pub mod changes;
pub mod graphql;
pub mod webhook;

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    ffi::{OsStr, OsString},
    io::{Cursor, Read},
    pin::pin,
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use decomp_dev_core::{
    config::GitHubConfig,
    models::{Commit, Project},
};
use decomp_dev_db::Database;
use futures_util::TryStreamExt;
use http::StatusCode;
use objdiff_core::bindings::report::Report;
use octocrab::{
    GitHubError, Octocrab,
    models::{
        ArtifactId, InstallationId, InstallationRepositories, Repository, RunId,
        repos::RepoCommitPage, workflows::HeadCommit,
    },
    params::actions::ArchiveFormat,
};
use regex::Regex;
use time::UtcDateTime;
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinSet,
    time::sleep,
};

#[derive(Clone)]
pub struct GitHub {
    pub client: Octocrab,
    pub installations: Option<Arc<Mutex<Installations>>>,
}

pub struct CachedInstallation {
    pub client: Octocrab,
    pub repositories: Vec<InstallationRepository>,
}

pub struct Installations {
    pub app_client: Octocrab,
    pub clients: HashMap<InstallationId, CachedInstallation>,
    pub repo_to_installation: HashMap<u64, InstallationId>,
}

pub struct InstallationRepository {
    pub id: u64,
    pub owner: String,
    pub name: String,
}

impl From<Repository> for InstallationRepository {
    fn from(value: Repository) -> Self {
        Self {
            id: value.id.into_inner(),
            owner: value.owner.map(|o| o.login).unwrap_or_default(),
            name: value.name,
        }
    }
}

impl Installations {
    pub async fn client_for_installation(
        &mut self,
        installation_id: InstallationId,
    ) -> Result<Octocrab> {
        match self.clients.entry(installation_id) {
            Entry::Occupied(entry) => Ok(entry.get().client.clone()),
            Entry::Vacant(entry) => {
                // Create a new client for the installation
                let client = self.app_client.installation(installation_id)?;
                let repositories = list_installation_repositories(&client)
                    .await
                    .context("Failed to fetch installation repositories")?;
                self.repo_to_installation
                    .extend(repositories.iter().map(|r| (r.id, installation_id)));
                entry.insert(CachedInstallation { client: client.clone(), repositories });
                Ok(client)
            }
        }
    }

    pub async fn client_for_repo(&mut self, repo_id: u64) -> Result<Option<Octocrab>> {
        if let Some(installation_id) = self.repo_to_installation.get(&repo_id) {
            return self.client_for_installation(*installation_id).await.map(Some);
        }
        Ok(None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GetCommit {
    owner: String,
    repo: String,
    sha: String,
}

#[derive(serde::Serialize)]
struct PageParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    per_page: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
}

async fn list_installation_repositories(
    app_client: &Octocrab,
) -> Result<Vec<InstallationRepository>> {
    let mut page = 1;
    let mut response: InstallationRepositories = app_client
        .get(
            "/installation/repositories",
            Some(&PageParams { per_page: Some(100), page: Some(page) }),
        )
        .await?;
    let mut repositories =
        response.repositories.into_iter().map(InstallationRepository::from).collect::<Vec<_>>();
    while repositories.len() < response.total_count as usize {
        page += 1;
        response = app_client
            .get(
                &format!("/installation/repositories?page={page}"),
                Some(&PageParams { per_page: Some(100), page: Some(page) }),
            )
            .await?;
        if response.repositories.is_empty() {
            break;
        }
        repositories.extend(response.repositories.into_iter().map(InstallationRepository::from));
    }
    Ok(repositories)
}

async fn list_installations(app_client: Octocrab) -> Result<Installations> {
    let mut clients = HashMap::new();
    let mut repo_to_installation = HashMap::new();
    {
        let mut stream =
            pin!(app_client.apps().installations().send().await?.into_stream(&app_client));
        while let Some(installation) = stream.try_next().await? {
            let client = app_client.installation(installation.id)?;
            let repositories = list_installation_repositories(&client).await?;
            for repository in &repositories {
                if repo_to_installation.insert(repository.id, installation.id).is_some() {
                    tracing::warn!(
                        "Duplicate installation for repository {}/{}",
                        repository.owner,
                        repository.name
                    );
                }
            }
            clients.insert(installation.id, CachedInstallation { client, repositories });
        }
    }
    Ok(Installations { app_client, clients, repo_to_installation })
}

impl GitHub {
    pub async fn new(config: &GitHubConfig) -> Result<Arc<Self>> {
        let client = Octocrab::builder()
            .personal_token(config.token.clone())
            .build()
            .context("Failed to create GitHub client")?;
        octocrab::initialise(client.clone());
        let profile = client.current().user().await.context("Failed to fetch current user")?;
        tracing::info!("Logged in as {}", profile.login);

        let installations = if let Some(app_config) = &config.app {
            let app_client = Octocrab::builder()
                .app(
                    app_config.id.into(),
                    jsonwebtoken::EncodingKey::from_rsa_pem(app_config.private_key.as_bytes())?,
                )
                .build()
                .context("Failed to create GitHub client")?;
            let result =
                list_installations(app_client).await.context("Failed to fetch installations")?;
            tracing::info!("Found {} installations", result.clients.len());
            for (installation_id, cached) in &result.clients {
                let owners =
                    cached.repositories.iter().map(|r| r.owner.as_str()).collect::<HashSet<_>>();
                let mut owner = String::new();
                for o in owners {
                    if !owner.is_empty() {
                        owner.push_str(", ");
                    }
                    owner.push_str(o);
                }
                tracing::info!(
                    "  - {}: {} ({} repositories)",
                    owner,
                    installation_id,
                    cached.repositories.len()
                );
            }
            Some(Arc::new(Mutex::new(result)))
        } else {
            None
        };
        Ok(Arc::new(Self { client, installations }))
    }

    pub async fn get_commit(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> Result<Option<RepoCommitPage>> {
        match self.client.repos(owner, repo).list_commits().sha(sha).per_page(1).send().await {
            Ok(page) => Ok(page.items.into_iter().next().map(|c| c.commit)),
            Err(octocrab::Error::GitHub { source, .. })
                if matches!(*source, GitHubError { status_code: StatusCode::NOT_FOUND, .. }) =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn client_for(&self, repo_id: u64) -> Result<Octocrab> {
        if let Some(installations) = &self.installations {
            let mut installations = installations.lock().await;
            if let Some(client) = installations.client_for_repo(repo_id).await? {
                return Ok(client);
            }
        }
        Ok(self.client.clone())
    }
}

pub async fn refresh_project(
    github: &GitHub,
    db: &Database,
    repo_id: u64,
    client_override: Option<&Octocrab>,
    full_refresh: bool,
) -> Result<usize> {
    let mut project_info = db
        .get_project_info_by_id(repo_id, None)
        .await
        .context("Failed to fetch project info")?
        .with_context(|| format!("Failed to fetch project info for ID {repo_id}"))?;
    if !project_info.project.reports_enabled() {
        tracing::info!(
            "Skipping disabled project {}/{}",
            project_info.project.owner,
            project_info.project.repo
        );
        return Ok(0);
    }
    let client = match client_override {
        Some(client) => client.clone(),
        None => github.client_for(repo_id).await?,
    };
    let repo = client
        .repos_by_id(project_info.project.id)
        .get()
        .await
        .with_context(|| format!("Failed to fetch repo for ID {repo_id}"))?;
    let branch = repo.default_branch.as_deref().unwrap_or("main");

    let owner = repo.owner.context("Repository has no owner")?;
    if project_info.project.owner != owner.login || project_info.project.repo != repo.name {
        tracing::info!(
            "Migrating project from {}/{} to {}/{}",
            project_info.project.owner,
            project_info.project.repo,
            owner.login,
            repo.name
        );
        db.update_project_owner_repo(project_info.project.id, &owner.login, &repo.name).await?;
        project_info = db
            .get_project_info_by_id(repo_id, None)
            .await
            .context("Failed to fetch project info")?
            .with_context(|| format!("Failed to fetch project info for ID {repo_id}"))?;
    }

    let project = &project_info.project;
    tracing::debug!("Refreshing project {}/{}", project.owner, project.repo);

    let workflow_ids = if let Some(workflow_id) = &project.workflow_id {
        vec![workflow_id.clone()]
    } else {
        let workflows = client
            .workflows(&project.owner, &project.repo)
            .list()
            .send()
            .await
            .context("Failed to fetch workflows")?;
        workflows.items.into_iter().map(|w| w.path).collect()
    };
    if workflow_ids.is_empty() {
        tracing::warn!("No workflows found for {}/{}", project.owner, project.repo);
        return Ok(0);
    }
    for workflow_id in workflow_ids {
        let workflow_id =
            workflow_id.strip_prefix(".github/workflows/").unwrap_or(workflow_id.as_str());
        let mut runs = vec![];
        let mut page = 1u32;
        'outer: loop {
            let result = client
                .workflows(&project.owner, &project.repo)
                .list_runs(workflow_id)
                .branch(branch)
                .event("push")
                .status("completed")
                .exclude_pull_requests(true)
                .page(page)
                .send()
                .await;
            let items = match result {
                Ok(result) if result.items.is_empty() => break,
                Ok(result) => result.items,
                Err(octocrab::Error::GitHub { source, .. })
                    if matches!(*source, GitHubError {
                        status_code: StatusCode::NOT_FOUND,
                        ..
                    }) =>
                {
                    break;
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("Failed to fetch workflows page {page}"));
                }
            };
            for run in items {
                if !full_refresh
                    && let Some(commit) = project_info.commit.as_ref()
                    && run.head_sha == commit.sha
                {
                    break 'outer;
                }
                runs.push(run);
            }
            page += 1;
        }
        tracing::info!(
            "Fetched {} runs for project {}/{} {}",
            runs.len(),
            project.owner,
            project.repo,
            workflow_id
        );

        struct TaskResult {
            run_id: RunId,
            commit: Commit,
            result: Result<WorkflowRunArtifacts>,
        }
        let sem = Arc::new(Semaphore::new(10));
        let mut set = JoinSet::new();
        for run in runs {
            let sem = sem.clone();
            let project_id = project.id;
            let owner = project.owner.clone();
            let repo = project.repo.clone();
            let client = client.clone();
            let db = db.clone();
            let run_id = run.id;
            let commit = commit_from_head_commit(&run.head_commit);
            set.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                match db.report_exists(project_id, &commit.sha).await {
                    Ok(true) => {
                        return TaskResult {
                            run_id,
                            commit,
                            result: Ok(WorkflowRunArtifacts { artifacts: vec![] }),
                        };
                    }
                    Ok(false) => {}
                    Err(e) => return TaskResult { run_id, commit, result: Err(e) },
                }
                let result =
                    fetch_workflow_run_artifacts(&client, &owner, &repo, run.id, None).await;
                TaskResult { run_id, commit, result }
            });
        }
        let mut imported_artifacts = 0;
        while let Some(join_result) = set.join_next().await {
            match join_result {
                Ok(TaskResult {
                    run_id,
                    commit,
                    result: Ok(WorkflowRunArtifacts { artifacts }),
                }) => {
                    tracing::debug!(
                        "Processed workflow run {} ({}) (artifacts {})",
                        run_id,
                        commit.sha,
                        artifacts.len()
                    );
                    for artifact in artifacts {
                        let start = std::time::Instant::now();
                        db.insert_report(project, &commit, &artifact.version, artifact.report)
                            .await?;
                        let duration = start.elapsed();
                        tracing::info!(
                            "Inserted report {} ({}) in {}ms",
                            artifact.version,
                            commit.sha,
                            duration.as_millis()
                        );
                        imported_artifacts += 1;
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

        if imported_artifacts > 0 {
            if project.workflow_id.is_none() {
                db.update_project_workflow_id(project.id, workflow_id).await?;
            }
            return Ok(imported_artifacts);
        }
    }

    Ok(0)
}

pub struct WorkflowRunArtifacts {
    pub artifacts: Vec<WorkflowRunArtifact>,
}

pub struct WorkflowRunArtifact {
    pub version: String,
    pub report: Box<Report>,
}

struct WorkflowRunArtifactList {
    pub version: String,
    pub name: String,
    pub id: ArtifactId,
}

async fn map_workflow_run_artifacts(
    client: &Octocrab,
    owner: &str,
    repo: &str,
    run_id: RunId,
) -> Result<Vec<WorkflowRunArtifactList>> {
    let artifacts = client
        .all_pages(
            client
                .actions()
                .list_workflow_run_artifacts(owner, repo, run_id)
                .send()
                .await
                .context("Failed to fetch artifacts")?
                .value
                .unwrap_or_default(),
        )
        .await?;
    tracing::debug!("Run {} (artifacts {})", run_id, artifacts.len());
    static REGEX: OnceLock<Regex> = OnceLock::new();
    let regex = REGEX
        .get_or_init(|| Regex::new(r"^(?P<version>[A-z0-9_.\-]+)[_-]report(?:[_-].*)?$").unwrap());
    static MAPS_REGEX: OnceLock<Regex> = OnceLock::new();
    let maps_regex =
        MAPS_REGEX.get_or_init(|| Regex::new(r"^(?P<version>[A-z0-9_\-]+)_maps$").unwrap());
    let mut result = Vec::new();
    for artifact in &artifacts {
        if artifact.expired {
            continue;
        }
        let version =
            if let Some(version) = regex.captures(&artifact.name).and_then(|c| c.name("version")) {
                version.as_str().to_string()
            } else if artifact.name == "progress" || artifact.name == "progress.json" {
                // bfbb compatibility
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
        result.push(WorkflowRunArtifactList {
            version,
            name: artifact.name.clone(),
            id: artifact.id,
        });
    }
    Ok(result)
}

pub async fn fetch_workflow_run_artifacts(
    client: &Octocrab,
    owner: &str,
    repo: &str,
    run_id: RunId,
    base_versions: Option<&[String]>,
) -> Result<WorkflowRunArtifacts> {
    // Some artifacts may take a few seconds to appear, so if we're provided with
    // expected base_versions, retry a few times to ensure we get them all.
    let mut attempt = 0;
    let artifacts = loop {
        let artifacts = map_workflow_run_artifacts(client, owner, repo, run_id).await?;
        if let Some(base_versions) = &base_versions {
            if base_versions
                .iter()
                .all(|base_version| artifacts.iter().any(|a| &a.version == base_version))
            {
                break artifacts;
            }
            attempt += 1;
            if attempt >= 5 {
                tracing::warn!(
                    "Workflow run {} missing expected artifacts after {} attempts",
                    run_id,
                    attempt
                );
                break artifacts;
            }
            tracing::info!(
                "Workflow run {} missing expected artifacts, retrying (attempt {}/{})",
                run_id,
                attempt,
                5
            );
            sleep(Duration::from_secs(1 << attempt)).await;
        } else {
            break artifacts;
        }
    };
    tracing::debug!("Run {} (artifacts {})", run_id, artifacts.len());
    let mut result = WorkflowRunArtifacts { artifacts: vec![] };
    if artifacts.is_empty() {
        return Ok(result);
    }
    let sem = Arc::new(Semaphore::new(3));
    let mut set = JoinSet::new();
    struct TaskResult {
        artifact_name: String,
        result: DownloadArtifactResult,
    }
    for artifact in artifacts {
        let sem = sem.clone();
        let client = client.clone();
        let owner = owner.to_owned();
        let repo = repo.to_owned();
        set.spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            TaskResult {
                artifact_name: artifact.name,
                result: download_artifact(client, &owner, &repo, artifact.id, artifact.version)
                    .await,
            }
        });
    }
    while let Some(join_result) = set.join_next().await {
        match join_result {
            Ok(TaskResult { artifact_name: name, result: Ok(reports) }) => {
                if reports.is_empty() {
                    tracing::warn!("No report found in workflow run {} artifact {}", run_id, name);
                } else {
                    for (version, report) in reports {
                        tracing::info!(
                            "Processed workflow run {} artifact {} ({})",
                            run_id,
                            name,
                            version
                        );
                        result.artifacts.push(WorkflowRunArtifact { version, report });
                    }
                }
            }
            Ok(TaskResult { artifact_name: name, result: Err(e) }) => {
                tracing::error!(
                    "Failed to process workflow run {} artifact {}: {:?}",
                    run_id,
                    name,
                    e
                );
            }
            Err(e) => {
                tracing::error!("Failed to process workflow run {} artifact: {:?}", run_id, e);
            }
        }
    }
    Ok(result)
}

type DownloadArtifactResult = Result<Vec<(String, Box<Report>)>>;

async fn download_artifact(
    client: Octocrab,
    owner: &str,
    repo: &str,
    artifact_id: ArtifactId,
    version: String,
) -> DownloadArtifactResult {
    let bytes =
        client.actions().download_artifact(owner, repo, artifact_id, ArchiveFormat::Zip).await?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    let version_report = OsString::from(format!("{}_report", version));
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let Some(path) = file.enclosed_name() else {
            continue;
        };
        if path.file_stem() == Some(OsStr::new("report"))
            || path.file_stem() == Some(&version_report)
            || path.file_stem() == Some(OsStr::new("progress"))
        {
            let mut contents = Vec::with_capacity(file.size() as usize);
            file.read_to_end(&mut contents)?;
            let mut report = Box::new(Report::parse(&contents)?);
            report.migrate()?;
            // Split combined reports into individual reports
            if version.eq_ignore_ascii_case("combined") {
                return Ok(report
                    .split()
                    .into_iter()
                    .map(|(version, report)| (version, Box::new(report)))
                    .collect());
            }
            return Ok(vec![(version, report)]);
        }
    }
    Ok(vec![])
}

pub fn commit_from_head_commit(commit: &HeadCommit) -> Commit {
    Commit {
        sha: commit.id.clone(),
        timestamp: UtcDateTime::from_unix_timestamp(
            commit.timestamp.to_utc().timestamp_millis() / 1000,
        )
        .unwrap_or(UtcDateTime::UNIX_EPOCH),
        message: (!commit.message.is_empty()).then(|| commit.message.clone()),
    }
}

pub async fn check_for_reports(
    client: &Octocrab,
    project: &Project,
    repo: &Repository,
) -> Result<String> {
    let workflow_ids = if let Some(workflow_id) = &project.workflow_id {
        vec![workflow_id.clone()]
    } else {
        let workflows = client
            .workflows(&project.owner, &project.repo)
            .list()
            .send()
            .await
            .context("Failed to fetch workflows")?;
        workflows.items.into_iter().map(|w| w.path).collect()
    };
    if workflow_ids.is_empty() {
        bail!("No workflows found in repository.");
    }
    let branch = repo.default_branch.as_deref().unwrap_or("main");
    for workflow_id in workflow_ids {
        let workflow_id =
            workflow_id.strip_prefix(".github/workflows/").unwrap_or(workflow_id.as_str());
        let result = client
            .workflows(&project.owner, &project.repo)
            .list_runs(workflow_id)
            .branch(branch)
            .event("push")
            .status("completed")
            .exclude_pull_requests(true)
            .send()
            .await;
        let items = match result {
            Ok(result) if result.items.is_empty() => continue,
            Ok(result) => result.items,
            Err(octocrab::Error::GitHub { source, .. })
                if matches!(*source, GitHubError { status_code: StatusCode::NOT_FOUND, .. }) =>
            {
                continue;
            }
            Err(e) => {
                return Err(e).context("Failed to fetch workflow runs");
            }
        };
        let run = items.first().unwrap();
        let result =
            fetch_workflow_run_artifacts(client, &project.owner, &project.repo, run.id, None)
                .await?;
        if !result.artifacts.is_empty() {
            return Ok(workflow_id.to_string());
        }
    }
    Err(anyhow!("No workflow runs containing reports found."))
}

pub fn extract_github_url(url: &str) -> Option<(&str, &str)> {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    let caps = REGEX
        .get_or_init(|| {
            Regex::new(r"^https?://github\.com/(?P<owner>[^/]+)/(?P<repo>[^/]+?)(?:\.git)?(?:/|$)")
                .unwrap()
        })
        .captures(url)?;
    let owner = caps.name("owner").map(|m| m.as_str()).unwrap_or_default();
    let repo = caps.name("repo").map(|m| m.as_str()).unwrap_or_default();
    Some((owner, repo))
}

#[cfg(test)]
mod tests {
    use super::extract_github_url;

    #[test]
    fn test_extract_github_url() {
        let cases: &[(&str, Option<(&str, &str)>)] = &[
            ("https://github.com/foo/bar", Some(("foo", "bar"))),
            ("http://github.com/foo/bar/", Some(("foo", "bar"))),
            ("https://github.com/foo/bar.git", Some(("foo", "bar"))),
            ("https://github.com/foo/bar/issues/17", Some(("foo", "bar"))),
            ("https://gitlab.com/foo/bar", None),
            ("https://github.com/foo", None),
            ("https://github.com/foo/bar.git/issues", Some(("foo", "bar"))),
        ];
        for &(url, expected) in cases {
            assert_eq!(extract_github_url(url), expected);
        }
    }
}
