use std::time::Instant;

use anyhow::{Context, Result};
use apalis::prelude::*;
use decomp_dev_core::models::Commit;
use decomp_dev_github::{
    changes::{
        generate_changes, generate_combined_comment, generate_comment,
        generate_missing_report_comment, post_pr_comment,
    },
    commit_from_head_commit, fetch_workflow_run_artifacts,
};
use octocrab::{
    Octocrab,
    models::{RepositoryId, RunId, pulls::PullRequest, workflows::Run},
};
use serde::{Deserialize, Serialize};

use crate::JobContext;

/// Job to process a completed GitHub Actions workflow run.
///
/// This job downloads artifacts from the workflow run, parses report files,
/// inserts them into the database (for push events), and posts/updates
/// PR comments (for pull request events).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessWorkflowRunJob {
    /// The repository ID (used to look up the project).
    pub repository_id: RepositoryId,
    /// The workflow run ID to process.
    pub run_id: RunId,
    /// The event that triggered the workflow ("push", "pull_request", etc.).
    pub event: String,
    /// The commit that triggered the workflow.
    pub head_commit: Commit,
    /// The branch that triggered the workflow.
    pub head_branch: String,
    /// The repository that triggered the workflow (may be a fork).
    pub head_repository_owner: Option<String>,
}

impl From<&Run> for ProcessWorkflowRunJob {
    fn from(run: &Run) -> Self {
        Self {
            repository_id: run.repository.id,
            run_id: run.id,
            event: run.event.clone(),
            head_commit: commit_from_head_commit(&run.head_commit),
            head_branch: run.head_branch.clone(),
            head_repository_owner: run
                .head_repository
                .as_ref()
                .and_then(|repo| repo.owner.as_ref().map(|o| o.login.clone())),
        }
    }
}

/// Process a completed workflow run job.
///
/// This handles:
/// - Fetching the project info from the database
/// - Getting the appropriate GitHub client (installation or personal token)
/// - Processing workflow run artifacts
/// - Inserting reports for push events
/// - Generating and posting PR comments for pull request events
pub async fn process_workflow_run_job(
    job: ProcessWorkflowRunJob,
    ctx: Data<JobContext>,
) -> Result<()> {
    tracing::info!(
        "Processing workflow run job: repo={} run={} event={}",
        job.repository_id,
        job.run_id,
        job.event,
    );

    match job.event.as_str() {
        "push" => {
            process_workflow_run_push(&ctx, &job).await?;
        }
        "pull_request" | "pull_request_target" => {
            process_workflow_run_pull_request(&ctx, &job).await?;
        }
        _ => {}
    }

    Ok(())
}

async fn process_workflow_run_push(ctx: &JobContext, job: &ProcessWorkflowRunJob) -> Result<()> {
    let project_id = job.repository_id.0;
    let Some(project_info) = ctx
        .db
        .get_project_info_by_id(project_id, None)
        .await
        .context("Failed to fetch project info")?
    else {
        tracing::warn!("No project found for repository ID {}", job.repository_id);
        return Ok(());
    };
    if !project_info.project.reports_enabled() {
        tracing::info!(
            "Skipping disabled project {}/{} for workflow run {}",
            project_info.project.owner,
            project_info.project.repo,
            job.run_id
        );
        return Ok(());
    }

    // Fetch repository info
    let client = ctx.github.client_for(job.repository_id.0).await?;
    let repository =
        client.repos_by_id(job.repository_id).get().await.context("Failed to fetch repository")?;

    let Some(owner) = repository.owner.as_ref().map(|o| o.login.as_str()) else {
        tracing::warn!("No owner found for repository ID {}", job.repository_id);
        return Ok(());
    };
    let repo = repository.name.as_str();

    // Only process runs on the default branch
    let is_default_branch = match (repository.default_branch.as_deref(), job.head_branch.as_str()) {
        (Some(default_branch), head_branch) => default_branch == head_branch,
        (None, "master" | "main") => true,
        _ => false,
    };
    if !is_default_branch {
        tracing::info!(
            "Skipping workflow run {} on non-default branch {}",
            job.run_id,
            job.head_branch,
        );
        return Ok(());
    }

    // Get all versions that exist on the base branch
    let base_versions = if let Some(base_commit) = &project_info.commit {
        ctx.db
            .get_versions_for_commit(project_id, &base_commit.sha)
            .await
            .context("Failed to get base versions")?
    } else {
        Vec::new()
    };

    // Process the workflow run to get artifacts
    let result =
        fetch_workflow_run_artifacts(&client, owner, repo, job.run_id, Some(&base_versions))
            .await
            .context("Failed to process workflow run")?;

    tracing::debug!(
        "Processed workflow run {} ({}) (artifacts {})",
        job.run_id,
        job.head_commit.sha,
        result.artifacts.len()
    );
    if result.artifacts.is_empty() {
        return Ok(());
    }

    // Insert reports into the database
    for artifact in result.artifacts {
        let start = Instant::now();
        ctx.db
            .insert_report(
                &project_info.project,
                &job.head_commit,
                &artifact.version,
                artifact.report,
            )
            .await
            .context("Failed to insert report")?;
        let duration = start.elapsed();
        tracing::info!(
            "Inserted report {} ({}) in {}ms",
            artifact.version,
            job.head_commit.sha,
            duration.as_millis()
        );
    }

    Ok(())
}

async fn process_workflow_run_pull_request(
    ctx: &JobContext,
    job: &ProcessWorkflowRunJob,
) -> Result<()> {
    let project_id = job.repository_id.0;
    let Some(project_info) = ctx
        .db
        .get_project_info_by_id(project_id, None)
        .await
        .context("Failed to fetch project info")?
    else {
        tracing::warn!("No project found for repository ID {}", job.repository_id);
        return Ok(());
    };
    if !project_info.project.reports_enabled() {
        tracing::info!(
            "Skipping disabled project {}/{} for workflow run {}",
            project_info.project.owner,
            project_info.project.repo,
            job.run_id
        );
        return Ok(());
    }

    if !project_info.project.enable_pr_comments {
        return Ok(());
    }

    // Actions pull_request builds always merge with the latest commit on the base branch.
    // We can't use the base commit from the workflow run or pull request APIs, those are
    // both lies. For simplicity, we'll just always compare against the latest commit that
    // we have stored.
    let Some(base_commit) = project_info.commit else {
        tracing::warn!("No base commit found for project ID {}", project_id);
        return Ok(());
    };

    // Get all versions that exist on the base branch
    let base_versions = ctx
        .db
        .get_versions_for_commit(project_id, &base_commit.sha)
        .await
        .context("Failed to get base versions")?;

    let client = ctx.github.client_for(job.repository_id.0).await?;
    let repository =
        client.repos_by_id(job.repository_id).get().await.context("Failed to fetch repository")?;

    let Some(owner) = repository.owner.as_ref().map(|o| o.login.as_str()) else {
        tracing::warn!("No owner found for repository ID {}", job.repository_id);
        return Ok(());
    };
    let repo = repository.name.as_str();

    let result =
        fetch_workflow_run_artifacts(&client, owner, repo, job.run_id, Some(&base_versions))
            .await
            .context("Failed to process workflow run")?;

    tracing::debug!(
        "Processed workflow run {} ({}) (artifacts {})",
        job.run_id,
        job.head_commit.sha,
        result.artifacts.len()
    );
    if result.artifacts.is_empty() {
        return Ok(());
    }

    let mut version_comments = Vec::new();

    // Process existing artifacts from PR
    for artifact in &result.artifacts {
        let cached_report = ctx
            .db
            .get_report(project_id, &base_commit.sha, &artifact.version)
            .await
            .context("Failed to get cached report")?;

        if let Some(cached_report) = cached_report {
            let report_file =
                ctx.db.upgrade_report(&cached_report).await.context("Failed to upgrade report")?;
            let report = report_file.report.flatten();
            let changes = generate_changes(&report, &artifact.report)
                .context("Failed to generate changes")?;
            version_comments.push(generate_comment(
                &report,
                &artifact.report,
                Some(&report_file.version),
                Some(&report_file.commit),
                Some(&job.head_commit),
                changes,
            ));
        } else {
            tracing::warn!(
                "No base report found for version {} (base {})",
                artifact.version,
                base_commit.sha
            );
            version_comments.push(generate_missing_report_comment(
                &artifact.version,
                Some(&base_commit),
                Some(&job.head_commit),
                true,
            ));
        }
    }

    // Check for versions that exist on base but are missing from PR
    for base_version in &base_versions {
        if !result.artifacts.iter().any(|a| a.version == *base_version) {
            version_comments.push(generate_missing_report_comment(
                base_version,
                Some(&base_commit),
                Some(&job.head_commit),
                false,
            ));
        }
    }

    if !version_comments.is_empty() {
        let combined_comment = generate_combined_comment(version_comments);

        let pull_requests = fetch_workflow_run_pull_requests(
            &client,
            job,
            owner,
            repo,
            repository.default_branch.as_deref(),
        )
        .await
        .context("Failed to fetch pull requests for workflow run")?;

        if pull_requests.is_empty() {
            tracing::info!(
                "No associated pull requests found for workflow run {} in {}/{}",
                job.run_id,
                owner,
                repo
            );
            return Ok(());
        }

        // Post/update comments for each associated PR
        for pull_request in &pull_requests {
            post_pr_comment(
                &client,
                &project_info.project,
                job.repository_id,
                pull_request,
                &combined_comment,
            )
            .await
            .context("Failed to post PR comment")?;
        }
    }

    Ok(())
}

async fn fetch_workflow_run_pull_requests(
    client: &Octocrab,
    job: &ProcessWorkflowRunJob,
    owner: &str,
    repo: &str,
    default_branch: Option<&str>,
) -> Result<Vec<PullRequest>> {
    let head = if let Some(head_owner) = job.head_repository_owner.as_deref() {
        format!("{}:{}", head_owner, job.head_branch)
    } else {
        job.head_branch.clone()
    };
    let mut pull_requests =
        client.all_pages(client.pulls(owner, repo).list().head(&head).send().await?).await?;
    tracing::info!("Found {} pull requests for {}", pull_requests.len(), head);

    pull_requests.retain(|pull_request| {
        if pull_request.head.sha != job.head_commit.sha {
            tracing::warn!(
                "Pull request {} head SHA {} does not match workflow run head SHA {}",
                pull_request.id,
                pull_request.head.sha,
                job.head_commit.sha
            );
            return false;
        }
        if default_branch.is_none_or(|b| pull_request.base.ref_field != *b) {
            tracing::warn!(
                "Pull request {} base branch {} does not match default branch {}",
                pull_request.id,
                pull_request.base.ref_field,
                default_branch.unwrap_or("[unknown]")
            );
            return false;
        }
        true
    });

    Ok(pull_requests)
}
