use std::io::Cursor;

use anyhow::{Context, Result};
use apalis::prelude::TaskSink;
use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use axum_typed_multipart::{TryFromMultipart, TypedMultipart};
use bytes::Bytes;
use decomp_dev_auth::CurrentUser;
use decomp_dev_core::{
    AppError,
    models::{
        ALL_PLATFORMS, CachedReportFile, Project, ProjectInfo, ProjectVisibility, PullReportStyle,
        project_visibility,
    },
};
use decomp_dev_github::{
    check_for_reports, extract_github_url, graphql::RepositoryPermission, refresh_project,
};
use decomp_dev_jobs::RefreshProjectJob;
use itertools::Itertools;
use maud::{DOCTYPE, Markup, html};
use serde::{Deserialize, Serialize};
use tower_sessions::Session;

use crate::{
    AppState,
    handlers::common::{Load, TemplateContext, nav_links},
};

pub async fn manage(
    mut ctx: TemplateContext,
    State(state): State<AppState>,
    current_user: CurrentUser,
) -> Result<Response, AppError> {
    let projects = state
        .db
        .get_projects()
        .await?
        .into_iter()
        .filter(|p| current_user.can_manage_project(&p.project))
        .sorted_by(|a, b| lexicmp::natural_lexical_cmp(&a.project.name(), &b.project.name()))
        .collect::<Vec<_>>();

    let rendered = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { "Manage • decomp.dev" }
                (ctx.header().await)
                (ctx.chunks("main", Load::Deferred).await)
                (ctx.chunks("manage", Load::Deferred).await)
            }
            body {
                header {
                    nav {
                        ul {
                            li {
                                a href="/" { strong { "decomp.dev" } }
                            }
                            li {
                                a href="/manage" { "Manage" }
                            }
                        }
                        (nav_links())
                    }
                }
                main {
                    h3 { "Projects" }
                    p { a href="/manage/new" role="button" { "Add New" } }
                    @if projects.is_empty() {
                        article { "No projects found." }
                    }
                    @for project in projects {
                        (project_fragment(&project))
                    }
                }
            }
            (ctx.footer(Some(&current_user)))
        }
    };
    Ok((ctx, rendered).into_response())
}

fn project_fragment(info: &ProjectInfo) -> Markup {
    let project_path = format!("/manage/{}/{}", info.project.owner, info.project.repo);
    html! {
        article.project {
            .project-header {
                h3.project-title {
                    a href=(project_path) { (info.project.name()) }
                }
            }
            .project-info {
                a href=(info.project.repo_url()) { (info.project.repo_url()) }
            }
        }
    }
}

pub async fn new(
    ctx: TemplateContext,
    State(state): State<AppState>,
    current_user: CurrentUser,
) -> Result<Response, AppError> {
    render_new(ctx, &state, &current_user, None, None).await
}

async fn render_new(
    mut ctx: TemplateContext,
    state: &AppState,
    current_user: &CurrentUser,
    message: Option<&str>,
    prefill: Option<&Project>,
) -> Result<Response, AppError> {
    let projects = state.db.get_projects().await?;

    let repos = current_user
        .data
        .repositories
        .iter()
        .filter(|r| r.permission == RepositoryPermission::Admin)
        .map(|r| {
            (r.id, format!("{}/{}", r.owner, r.name), projects.iter().any(|p| p.project.id == r.id))
        })
        .sorted_by(|a, b| lexicmp::lexical_cmp(&a.1, &b.1))
        .collect::<Vec<_>>();

    let current_url = prefill.as_ref().map(|p| p.repo_url()).unwrap_or_default();
    let current_name = prefill.as_ref().and_then(|p| p.name.as_deref()).unwrap_or("");
    let current_short_name = prefill.as_ref().and_then(|p| p.short_name.as_deref()).unwrap_or("");
    let current_platform = prefill.as_ref().and_then(|p| p.platform.as_deref());

    let repo_options = html! {
        @for (id, repo, exists) in repos {
            @if exists {
                option value=(id) disabled { (repo) }
            } @else if prefill.is_some_and(|p| p.id == id) {
                option value=(id) selected { (repo) }
            } @else {
                option value=(id) { (repo) }
            }
        }
    };

    let rendered = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { "New Project • decomp.dev" }
                (ctx.header().await)
                (ctx.chunks("main", Load::Deferred).await)
                (ctx.chunks("manage", Load::Deferred).await)
            }
            body {
                header {
                    nav {
                        ul {
                            li {
                                a href="/" { strong { "decomp.dev" } }
                            }
                            li {
                                a href="/manage" { "Manage" }
                            }
                            li {
                                a href="/manage/new" { "New" }
                            }
                        }
                        (nav_links())
                    }
                }
                main {
                    h3 { "Add New" }
                    form method="post" data-loading="Processing..." {
                        fieldset {
                            label {
                                "Repository"
                                @if current_user.super_admin {
                                    input name="repository_url" type="url" required
                                        aria-invalid=[message.map(|_| "true")]
                                        value=(current_url);
                                } @else {
                                    select name="repository_id" aria-invalid=[message.map(|_| "true")] { (repo_options) }
                                }
                                @if let Some(message) = message {
                                    small { (message) }
                                } @else {
                                    small { "Repository must be public. Admin permissions are required." }
                                }
                            }
                            label {
                                "Game name"
                                input name="name" required value=(current_name);
                                small { "Please use the full name of the game, e.g. \"The Legend of Zelda: Ocarina of Time\"." }
                            }
                            label {
                                "Short name "
                                small { "(optional)" }
                                input name="short_name" value=(current_short_name);
                                small { "If the game has a long prefix, e.g. \"The Legend of Zelda\", the short name will be \"Ocarina of Time\"." }
                            }
                            label {
                                "Platform"
                                select name="platform" required { (platform_options(current_platform)) }
                                small { "Platform not listed? Please open an issue on GitHub." }
                            }
                        }
                        button type="submit" { "Add" }
                    }
                }
            }
            (ctx.footer(Some(current_user)))
        }
    };
    Ok((ctx, rendered).into_response())
}

fn platform_options(current_platform: Option<&str>) -> Markup {
    html! {
        @if current_platform.is_none() {
            option value="" disabled selected { "Select one" }
        }
        @for platform in ALL_PLATFORMS {
            @let platform_str = platform.to_str();
            @if current_platform == Some(platform_str) {
                option value=(platform_str) selected { (platform.name()) }
            } @else {
                option value=(platform_str) { (platform.name()) }
            }
        }
    }
}

#[derive(Deserialize)]
pub struct NewForm {
    repository_id: Option<u64>,
    repository_url: Option<String>,
    name: String,
    short_name: String,
    platform: String,
}

pub async fn new_save(
    ctx: TemplateContext,
    State(state): State<AppState>,
    current_user: CurrentUser,
    Form(form): Form<NewForm>,
) -> Result<Response, AppError> {
    let client = current_user.client(&state.config.github)?;
    let (repository_id, repo) = match (form.repository_id, form.repository_url) {
        (Some(id), _) => (id, None),
        (None, Some(url)) => {
            let Some((owner, repo)) = extract_github_url(&url) else {
                return render_new(
                    ctx,
                    &state,
                    &current_user,
                    Some("Invalid repository URL."),
                    None,
                )
                .await;
            };
            let Ok(repo) = client.repos(owner, repo).get().await else {
                return render_new(ctx, &state, &current_user, Some("Repository not found."), None)
                    .await;
            };
            (repo.id.into_inner(), Some(repo))
        }
        (None, None) => {
            return render_new(ctx, &state, &current_user, Some("Repository is required."), None)
                .await;
        }
    };
    if let Some(existing) = state.db.get_project_info_by_id(repository_id, None).await? {
        return Ok(Redirect::to(&format!("/{}/{}", existing.project.owner, existing.project.repo))
            .into_response());
    }
    let Some(platform) = ALL_PLATFORMS.iter().find(|p| p.to_str() == form.platform) else {
        return Err(AppError::Status(StatusCode::BAD_REQUEST));
    };
    let repo = match repo {
        Some(repo) => repo,
        None => match client.repos_by_id(repository_id).get().await {
            Ok(repo) => repo,
            Err(e) => {
                tracing::error!("Failed to fetch repository: {:?}", e);
                return render_new(
                    ctx,
                    &state,
                    &current_user,
                    Some("Failed to fetch repository information."),
                    None,
                )
                .await;
            }
        },
    };

    let name = form.name.trim();
    let short_name = form.short_name.trim();
    let mut project = Project {
        id: repo.id.into_inner(),
        owner: repo.owner.as_ref().map(|o| o.login.clone()).unwrap_or_default(),
        repo: repo.name.clone(),
        name: (!name.is_empty()).then_some(name.to_string()),
        short_name: (!short_name.is_empty()).then_some(short_name.to_string()),
        platform: Some(platform.to_str().to_string()),
        ..Default::default()
    };
    if !current_user.super_admin && repo.permissions.as_ref().is_none_or(|p| !p.admin) {
        return render_new(
            ctx,
            &state,
            &current_user,
            Some("You do not have admin permissions on this repository."),
            Some(&project),
        )
        .await;
    }

    let workflow_id = match check_for_reports(&client, &project, &repo).await {
        Ok(workflow_id) => workflow_id,
        Err(e) => {
            let message = e.to_string();
            return render_new(ctx, &state, &current_user, Some(&message), Some(&project)).await;
        }
    };
    project.workflow_id = Some(workflow_id);
    state.db.create_project(&project).await?;
    refresh_project(&state.github, &state.db, project.id, Some(&client), true).await?;
    Ok(Redirect::to(&format!("/{}/{}", project.owner, project.repo)).into_response())
}

pub async fn manage_project(
    Path(params): Path<ProjectParams>,
    State(state): State<AppState>,
    current_user: CurrentUser,
    ctx: TemplateContext,
    session: Session,
) -> Result<Response, AppError> {
    let Some(info) = state.db.get_project_info(&params.owner, &params.repo, None).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    if !current_user.can_manage_project(&info.project) {
        return Err(AppError::Status(StatusCode::FORBIDDEN));
    }

    let report = if let (Some(version), Some(commit)) = (info.default_version(), &info.commit) {
        state.db.get_report(info.project.id, &commit.sha, version).await.with_context(|| {
            format!(
                "Failed to fetch report for {}/{} sha {} version {}",
                info.project.owner, info.project.repo, commit.sha, version
            )
        })?
    } else {
        None
    };
    render_manage_project(ctx, &state, &info, report.as_ref(), &current_user, session).await
}

#[derive(Debug, Serialize, Deserialize, Default)]
enum Message {
    #[default]
    None,
    Info(String),
    Error(String),
}

fn render_message(message: &Message) -> Markup {
    match message {
        Message::None => Markup::default(),
        Message::Info(msg) => html! {
            article.info-card { (msg) }
        },
        Message::Error(msg) => html! {
            article.error-card { (msg) }
        },
    }
}

async fn render_manage_project(
    mut ctx: TemplateContext,
    state: &AppState,
    project_info: &ProjectInfo,
    latest_report: Option<&CachedReportFile>,
    current_user: &CurrentUser,
    session: Session,
) -> Result<Response, AppError> {
    let project_short_name = project_info.project.short_name();
    let project_manage_path =
        format!("/manage/{}/{}", project_info.project.owner, project_info.project.repo);
    let refresh_path =
        format!("/manage/{}/{}/refresh", project_info.project.owner, project_info.project.repo);
    let default_version = project_info.default_version();

    let current_name = project_info.project.name.as_deref().unwrap_or("");
    let current_short_name = project_info.project.short_name.as_deref().unwrap_or("");
    let current_platform = project_info.project.platform.as_deref();
    let current_workflow_id = project_info.project.workflow_id.as_deref().unwrap_or("");

    let installation_id = if let Some(installations) = &state.github.installations {
        let installations = installations.lock().await;
        installations.repo_to_installation.get(&project_info.project.id).cloned()
    } else {
        None
    };

    let message = session
        .remove::<Message>(&format!("manage_{}_message", project_info.project.id))
        .await?
        .unwrap_or_default();

    // Check if the project is hidden based on matched code percentage
    let visibility =
        project_visibility(&project_info.project, latest_report.map(|r| &r.report.measures));

    let rendered = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { (project_short_name) " • Manage" }
                (ctx.header().await)
                (ctx.chunks("main", Load::Deferred).await)
                (ctx.chunks("manage", Load::Deferred).await)
            }
            body {
                header {
                    nav {
                        ul {
                            li {
                                a href="/" { strong { "decomp.dev" } }
                            }
                            li {
                                a href="/manage" { "Manage" }
                            }
                            li {
                                a href=(project_manage_path) { (project_short_name) }
                            }
                        }
                        (nav_links())
                    }
                }
                main {
                    h3 { "Edit " (project_short_name) }
                    (render_message(&message))
                    @match visibility {
                        ProjectVisibility::Visible => {},
                        ProjectVisibility::Disabled => {
                            article.warning-card { "This project is disabled." }
                        }
                        ProjectVisibility::Hidden => {
                            article.warning-card { "This project is hidden until it has reached a minimum of 0.5% matched code." }
                        }
                    }
                    form method="post" enctype="multipart/form-data" data-loading="Saving..." {
                        fieldset {
                            label {
                                input name="enabled" type="checkbox" role="switch"
                                    checked[project_info.project.enabled];
                                "Enable project"
                                br;
                                small.muted { "Disabled projects will not be listed, and reports will not be fetched." }
                            }
                            @if current_user.super_admin {
                                label {
                                    input name="permanently_disabled" type="checkbox" role="switch"
                                        checked[project_info.project.permanently_disabled];
                                    "Permanently disable project"
                                    br;
                                    small.muted { "Project owners cannot manage permanently disabled projects." }
                                }
                            }
                            label {
                                "Repository"
                                input type="text" readonly disabled value=(project_info.project.repo_url());
                            }
                            label {
                                "Game name"
                                input name="name" required value=(current_name);
                                small { "Please use the full name of the game, e.g. \"The Legend of Zelda: Ocarina of Time\"." }
                            }
                            label {
                                "Short name "
                                small { "(optional)" }
                                input name="short_name" value=(current_short_name) placeholder=(current_name);
                                small { "If the game has a long prefix, e.g. \"The Legend of Zelda\", the short name will be \"Ocarina of Time\"." }
                            }
                            label {
                                "Platform"
                                select name="platform" { (platform_options(current_platform)) }
                                small { "Platform not listed? Please open an issue on GitHub." }
                            }
                            label {
                                "Default version"
                                select name="default_version" {
                                    @for version in &project_info.report_versions {
                                        @let selected = default_version == Some(version.as_str());
                                        option value=(version) selected[selected] { (version) }
                                    }
                                }
                            }
                            label {
                                "GitHub workflow ID"
                                input name="workflow_id" type="text" value=(current_workflow_id);
                                small { "The GitHub Actions workflow that contains report artifacts." }
                            }
                            label {
                                input name="enable_pr_comments" type="checkbox" role="switch"
                                    disabled[installation_id.is_none()]
                                    checked[project_info.project.enable_pr_comments];
                                "Enable PR comments"
                                @if installation_id.is_none() {
                                    " (requires GitHub App installation)"
                                }
                            }
                            label {
                                "PR report style"
                                select name="pr_report_style" disabled[installation_id.is_none()] {
                                    @for &style in PullReportStyle::variants() {
                                        option value=(style.as_str()) selected[project_info.project.pr_report_style == style] { (style) }
                                    }
                                }
                                @if installation_id.is_none() {
                                    " (requires GitHub App installation)"
                                }
                            }
                            hr;
                            label {
                                "Hero image "
                                small { "(optional)" }
                                input name="header_image" type="file" accept="image/*";
                                small {
                                    "Image should be at least 1024×256. A common size is 1920×620."
                                    br;
                                    "Upload the best quality/resolution available; the image will be resized to fit."
                                    br;
                                    a href="https://www.steamgriddb.com/heroes" target="_blank" { "SteamGridDB" }
                                    " has a large collection of images."
                                }
                            }
                            label {
                                input name="clear_header_image" type="checkbox" role="switch"
                                    disabled[project_info.project.header_image_id.is_none()];
                                "Clear the current hero image"
                            }
                        }
                        button type="submit" { "Save" }
                    }
                    h4 { "Debug" }
                    @if let Some(installation_id) = installation_id {
                        p {
                            "GitHub App installation ID: "
                            kbd { (installation_id) }
                        }
                    } @else {
                        p {
                            "No GitHub App installation found. "
                            a href="https://github.com/apps/decomp-dev" target="_blank" { "Install the app" }
                        }
                    }
                    .grid {
                        form action=(refresh_path) method="post" data-loading="Refreshing..." {
                            button .outline .secondary type="submit" { "Force refresh" }
                            small { "Fetches any missing report artifacts." }
                        }
                    }
                    form.mt-spacing action=(format!("/manage/{}/{}/delete-commit", project_info.project.owner, project_info.project.repo)) method="post" data-loading="Deleting..." {
                        label {
                            "Delete reports"
                            fieldset role="group" {
                                input name="commit_sha" type="text" placeholder="Full commit SHA (40 characters)" pattern="[a-f0-9]{40}" required;
                                button.outline type="submit" { "Delete" }
                            }
                            small { "Delete all reports for a specific commit. Must be the full 40-character SHA." }
                        }
                    }
                }
            }
            (ctx.footer(Some(current_user)))
        }
    };
    Ok((ctx, rendered).into_response())
}

#[derive(Debug, TryFromMultipart)]
pub struct ProjectForm {
    pub name: String,
    pub short_name: String,
    pub platform: String,
    pub default_version: Option<String>,
    pub workflow_id: String,
    pub enable_pr_comments: Option<String>,
    pub pr_report_style: Option<String>,
    pub header_image: Option<Bytes>,
    pub clear_header_image: Option<String>,
    pub enabled: Option<String>,
    pub permanently_disabled: Option<String>,
}

#[derive(Deserialize)]
pub struct ProjectParams {
    owner: String,
    repo: String,
}

pub async fn manage_project_save(
    Path(params): Path<ProjectParams>,
    State(state): State<AppState>,
    current_user: CurrentUser,
    TypedMultipart(form): TypedMultipart<ProjectForm>,
) -> Result<Response, AppError> {
    let Some(project_info) = state.db.get_project_info(&params.owner, &params.repo, None).await?
    else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    if !current_user.can_manage_project(&project_info.project) {
        return Err(AppError::Status(StatusCode::FORBIDDEN));
    }
    if !ALL_PLATFORMS.iter().any(|p| p.to_str() == form.platform) {
        return Err(AppError::Status(StatusCode::BAD_REQUEST));
    };

    let mut header_image_id = project_info.project.header_image_id;
    if let Some(header_image) = form.header_image.filter(|b| !b.is_empty()) {
        let format = image::guess_format(&header_image)?;
        let (width, height) =
            image::ImageReader::with_format(Cursor::new(&header_image[..]), format)
                .into_dimensions()?;
        let id = state.db.create_image(format.to_mime_type(), width, height, &header_image).await?;
        header_image_id = Some(id);
    } else if form.clear_header_image.is_some_and(|v| v == "on") {
        header_image_id = None;
    }

    let installation_id = if let Some(installations) = &state.github.installations {
        let installations = installations.lock().await;
        installations.repo_to_installation.get(&project_info.project.id).cloned()
    } else {
        None
    };
    let name = form.name.trim();
    let short_name = form.short_name.trim();
    let platform = form.platform.trim();
    let workflow_id = form.workflow_id.trim();
    let permanently_disabled = if current_user.super_admin {
        form.permanently_disabled.is_some_and(|v| v == "on")
    } else {
        project_info.project.permanently_disabled
    };
    let project = Project {
        id: project_info.project.id,
        owner: project_info.project.owner,
        repo: project_info.project.repo,
        name: (!name.is_empty()).then_some(name.to_string()),
        short_name: (!short_name.is_empty()).then_some(short_name.to_string()),
        default_category: project_info.project.default_category,
        default_version: form.default_version,
        platform: (!platform.is_empty()).then_some(platform.to_string()),
        workflow_id: (!workflow_id.is_empty()).then_some(workflow_id.to_string()),
        // If there's no installation ID, use the existing value
        enable_pr_comments: if installation_id.is_some() {
            form.enable_pr_comments.is_some_and(|v| v == "on")
        } else {
            project_info.project.enable_pr_comments
        },
        pr_report_style: if installation_id.is_some() {
            form.pr_report_style.as_deref().and_then(|s| s.parse().ok()).unwrap_or_default()
        } else {
            project_info.project.pr_report_style
        },
        header_image_id,
        enabled: form.enabled.is_some_and(|v| v == "on") && !permanently_disabled,
        permanently_disabled,
    };
    state.db.update_project(&project).await?;
    let redirect_url = format!("/{}/{}", params.owner, params.repo);
    Ok(Redirect::to(&redirect_url).into_response())
}

pub async fn manage_project_refresh(
    Path(params): Path<ProjectParams>,
    State(state): State<AppState>,
    current_user: CurrentUser,
    session: Session,
) -> Result<Response, AppError> {
    let Some(info) = state.db.get_project_info(&params.owner, &params.repo, None).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    if !current_user.can_manage_project(&info.project) {
        return Err(AppError::Status(StatusCode::FORBIDDEN));
    }

    // Enqueue a refresh job instead of processing synchronously
    let job = RefreshProjectJob { repository_id: info.project.id, full_refresh: true };

    let mut storage = state.jobs.refresh_project();
    let message = match storage.push(job).await {
        Ok(()) => Message::Info("Refresh job queued. Reports will be updated shortly.".to_string()),
        Err(e) => {
            tracing::error!("Failed to enqueue refresh job: {:?}", e);
            Message::Error(format!("Failed to queue refresh: {e}"))
        }
    };

    session.insert(&format!("manage_{}_message", info.project.id), message).await?;
    let redirect_url = format!("/manage/{}/{}", params.owner, params.repo);
    Ok(Redirect::to(&redirect_url).into_response())
}

#[derive(Deserialize)]
pub struct DeleteCommitForm {
    commit_sha: String,
}

pub async fn delete_commit(
    Path(params): Path<ProjectParams>,
    State(state): State<AppState>,
    current_user: CurrentUser,
    session: Session,
    Form(form): Form<DeleteCommitForm>,
) -> Result<Response, AppError> {
    let Some(info) = state.db.get_project_info(&params.owner, &params.repo, None).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    if !current_user.can_manage_project(&info.project) {
        return Err(AppError::Status(StatusCode::FORBIDDEN));
    }
    let num_reports_deleted =
        state.db.delete_reports_by_commit(info.project.id, &form.commit_sha).await?;
    let message = if num_reports_deleted > 0 {
        Message::Info(format!("Deleted {num_reports_deleted} reports"))
    } else {
        Message::Error("No reports found. Is the commit SHA correct?".to_string())
    };
    session.insert(&format!("manage_{}_message", info.project.id), message).await?;
    let redirect_url = format!("/manage/{}/{}", params.owner, params.repo);
    Ok(Redirect::to(&redirect_url).into_response())
}
