use std::borrow::Cow;

use anyhow::{Context, Result};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use decomp_dev_auth::CurrentUser;
use decomp_dev_core::{
    AppError, FullUri,
    models::{FullReportFile, ProjectInfo, ProjectVisibility, project_visibility},
    util::{UrlExt, format_percent, size},
};
use decomp_dev_images::{
    badge,
    treemap::{layout_units, unit_color},
};
use image::ImageFormat;
use maud::{DOCTYPE, PreEscaped, html};
use mime::Mime;
use objdiff_core::bindings::report::{Measures, ReportCategory, ReportUnit};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use url::Url;

use super::{parse_accept, treemap};
use crate::{
    AppState,
    handlers::{
        common::{Load, TemplateContext, escape_script, nav_links},
        project::ProjectResponse,
    },
    proto::{PROTOBUF, Protobuf},
};

#[derive(Deserialize)]
pub struct ReportParams {
    id: Option<String>,
    owner: Option<String>,
    repo: Option<String>,
    version: Option<String>,
    commit: Option<String>,
}

const DEFAULT_IMAGE_WIDTH: u32 = 950;
const DEFAULT_IMAGE_HEIGHT: u32 = 475;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportQuery {
    mode: Option<String>,
    version: Option<String>,
    category: Option<String>,
    commit: Option<String>,
    w: Option<u32>,
    h: Option<u32>,
    #[serde(flatten)]
    shield: badge::ShieldParams,
    unit: Option<String>,
}

impl ReportQuery {
    pub fn size(&self) -> (u32, u32) {
        (self.w.unwrap_or(DEFAULT_IMAGE_WIDTH), self.h.unwrap_or(DEFAULT_IMAGE_HEIGHT))
    }
}

#[derive(Serialize)]
pub struct ReportTemplateUnit<'a> {
    pub name: &'a str,
    pub total_code: u64,
    pub fuzzy_match_percent: f32,
    pub color: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub is_linked: bool,
}

#[derive(Serialize, Clone)]
struct ReportCategoryItem<'a> {
    id: &'a str,
    name: &'a str,
    path: String,
}

#[derive(Serialize, Clone)]
struct ReportCategoryGroup<'a> {
    category: ReportCategoryItem<'a>,
    subcategories: Vec<ReportCategoryItem<'a>>,
}

struct CategorySelection<'a> {
    categories: Vec<ReportCategoryGroup<'a>>,
    current_top_index: usize,
    current_sub_index: Option<usize>,
}

fn build_category_selection<'a>(
    canonical_url: &Url,
    report_categories: &'a [ReportCategory],
    current_category: Option<&'a ReportCategory>,
    default_category: &str,
) -> CategorySelection<'a> {
    let all_url = canonical_url
        .query_param("category", if default_category == "all" { None } else { Some("all") });
    let all_category =
        ReportCategoryItem { id: "all", name: "All", path: all_url.path_and_query().to_string() };
    let mut categories =
        vec![ReportCategoryGroup { category: all_category, subcategories: Vec::new() }];

    for c in report_categories {
        if let Some((parent_id, _)) = c.id.split_once('.') {
            let path =
                canonical_url.query_param("category", Some(&c.id)).path_and_query().to_string();
            if let Some(group) = categories.iter_mut().find(|g| g.category.id == parent_id) {
                group.subcategories.push(ReportCategoryItem { id: &c.id, name: &c.name, path });
            } else {
                let parent_path = canonical_url
                    .query_param("category", Some(parent_id))
                    .path_and_query()
                    .to_string();
                categories.push(ReportCategoryGroup {
                    category: ReportCategoryItem {
                        id: parent_id,
                        name: parent_id,
                        path: parent_path,
                    },
                    subcategories: vec![ReportCategoryItem { id: &c.id, name: &c.name, path }],
                });
            }
        } else {
            let path =
                canonical_url.query_param("category", Some(&c.id)).path_and_query().to_string();
            if let Some(group) = categories.iter_mut().find(|g| g.category.id == c.id) {
                group.category.name = &c.name;
                group.category.path = path;
            } else {
                categories.push(ReportCategoryGroup {
                    category: ReportCategoryItem { id: &c.id, name: &c.name, path },
                    subcategories: Vec::new(),
                });
            }
        }
    }

    let current_category_id = current_category.map(|c| c.id.as_str()).unwrap_or("all");
    let (current_top_id, current_sub_id) = current_category_id
        .split_once('.')
        .map(|(top, _)| (top, Some(current_category_id)))
        .unwrap_or((current_category_id, None));
    let current_top_index =
        categories.iter().position(|c| c.category.id == current_top_id).unwrap_or(0);
    let current_sub_index = current_sub_id.and_then(|id| {
        categories[current_top_index].subcategories.iter().position(|sc| sc.id == id)
    });

    CategorySelection { categories, current_top_index, current_sub_index }
}

#[derive(Serialize, Clone)]
struct ReportTemplateVersion<'a> {
    id: &'a str,
    path: String,
}

/// Duplicate of Measures to avoid omitting empty fields
#[derive(Serialize, Default, Clone)]
pub struct TemplateMeasures {
    pub fuzzy_match_percent: f32,
    pub total_code: u64,
    pub matched_code: u64,
    pub matched_code_percent: f32,
    pub total_data: u64,
    pub matched_data: u64,
    pub matched_data_percent: f32,
    pub total_functions: u32,
    pub matched_functions: u32,
    pub matched_functions_percent: f32,
    pub complete_code: u64,
    pub complete_code_percent: f32,
    pub complete_data: u64,
    pub complete_data_percent: f32,
    pub total_units: u32,
    pub complete_units: u32,
}

impl From<&Measures> for TemplateMeasures {
    fn from(
        &Measures {
            fuzzy_match_percent,
            total_code,
            matched_code,
            matched_code_percent,
            total_data,
            matched_data,
            matched_data_percent,
            total_functions,
            matched_functions,
            matched_functions_percent,
            complete_code,
            complete_code_percent,
            complete_data,
            complete_data_percent,
            total_units,
            complete_units,
        }: &Measures,
    ) -> Self {
        Self {
            fuzzy_match_percent,
            total_code,
            matched_code,
            matched_code_percent,
            total_data,
            matched_data,
            matched_data_percent,
            total_functions,
            matched_functions,
            matched_functions_percent,
            complete_code,
            complete_code_percent,
            complete_data,
            complete_data_percent,
            total_units,
            complete_units,
        }
    }
}

fn is_valid_extension(ext: &str) -> bool {
    // FIXME: hack for versions that have .nn where nn is a number
    ext.chars().any(|c| c.is_ascii_alphabetic())
}

fn extract_extension(params: ReportParams) -> (ReportParams, Option<String>) {
    if let Some(commit) = params.commit.as_deref() {
        if let Some((commit, ext)) = commit.rsplit_once('.')
            && is_valid_extension(ext)
        {
            return (
                ReportParams { commit: Some(commit.to_string()), ..params },
                Some(ext.to_string()),
            );
        }
    } else if let Some(version) = params.version.as_deref() {
        if let Some((version, ext)) = version.rsplit_once('.')
            && is_valid_extension(ext)
        {
            return (
                ReportParams { version: Some(version.to_string()), ..params },
                Some(ext.to_string()),
            );
        }
    } else if let Some(repo) = params.repo.as_deref() {
        if let Some((repo, ext)) = repo.rsplit_once('.')
            && is_valid_extension(ext)
        {
            return (
                ReportParams { repo: Some(repo.to_string()), ..params },
                Some(ext.to_string()),
            );
        }
    } else if let Some(id) = params.id.as_deref()
        && let Some((id, ext)) = id.rsplit_once('.')
        && is_valid_extension(ext)
    {
        return (ReportParams { id: Some(id.to_string()), ..params }, Some(ext.to_string()));
    }
    (params, None)
}

pub async fn get_report(
    Path(params): Path<ReportParams>,
    Query(query): Query<ReportQuery>,
    headers: HeaderMap,
    FullUri(uri): FullUri,
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    ctx: TemplateContext,
) -> Result<Response, AppError> {
    let (params, ext) = extract_extension(params);
    let acceptable = parse_accept(&headers, ext.as_deref());
    if acceptable.is_empty() {
        return Err(AppError::Status(StatusCode::NOT_ACCEPTABLE));
    }

    let mut commit = query.commit.as_deref().or(params.commit.as_deref());
    if matches!(commit, Some(c) if c.eq_ignore_ascii_case("latest")) {
        commit = None;
    }
    let Some(project_info) = (match (&params.id, &params.owner, &params.repo) {
        (Some(id), _, _) => {
            let id: u64 = id.parse().map_err(|_| AppError::Status(StatusCode::BAD_REQUEST))?;
            state.db.get_project_info_by_id(id, commit).await?
        }
        (_, Some(owner), Some(repo)) => state.db.get_project_info(owner, repo, commit).await?,
        _ => return Err(AppError::Status(StatusCode::BAD_REQUEST)),
    }) else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let Some(commit) = project_info.commit.as_ref() else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let version = if let Some(version) = query.version.as_deref().or(params.version.as_deref()) {
        if version.eq_ignore_ascii_case("default") {
            project_info.default_version().ok_or(AppError::Status(StatusCode::NOT_FOUND))?
        } else {
            version
        }
    } else if let Some(default_version) = project_info.default_version() {
        default_version
    } else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let Some(report) = state.db.get_report(project_info.project.id, &commit.sha, version).await?
    else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let report = state.db.upgrade_report(&report).await?;

    let scope = apply_scope(&report, &project_info, &query)?;
    match query.mode.as_deref().unwrap_or("overview").to_ascii_lowercase().as_str() {
        "history" => mode_history(&scope, &state, uri, query, ctx, &acceptable, current_user).await,
        "measures" => mode_measures(&scope, &acceptable),
        "overview" => {
            mode_overview(&scope, &state, uri, query, ctx, &acceptable, current_user).await
        }
        "report" => mode_report(&scope, &state, uri, query, ctx, &acceptable, current_user).await,
        "shield" => mode_shield(&scope, query, &acceptable),
        _ => Err(AppError::Status(StatusCode::BAD_REQUEST)),
    }
}

async fn mode_overview(
    scope: &Scope<'_>,
    state: &AppState,
    uri: Uri,
    query: ReportQuery,
    ctx: TemplateContext,
    acceptable: &[Mime],
    current_user: Option<CurrentUser>,
) -> Result<Response, AppError> {
    for mime in acceptable {
        if (mime.type_() == mime::STAR && mime.subtype() == mime::STAR)
            || (mime.type_() == mime::TEXT && mime.subtype() == mime::HTML)
        {
            return render_report(scope, state, uri, current_user, ctx).await;
        } else if mime.type_() == mime::APPLICATION && mime.subtype() == mime::JSON {
            let result =
                ProjectResponse::new(scope.project_info, scope.measures, &scope.report.report);
            return Ok(Json(result).into_response());
        } else if mime.type_() == mime::IMAGE && mime.subtype() == mime::SVG {
            let (w, h) = query.size();
            let svg = treemap::render_svg(&scope.units, w, h);
            return Ok(([(header::CONTENT_TYPE, mime::IMAGE_SVG.as_ref())], svg).into_response());
        } else if mime.type_() == mime::IMAGE {
            let format = if mime.subtype() == mime::STAR {
                // Default to PNG
                ImageFormat::Png
            } else {
                ImageFormat::from_mime_type(mime.essence_str())
                    .ok_or_else(|| AppError::Status(StatusCode::NOT_ACCEPTABLE))?
            };
            let (w, h) = query.size();
            let data = treemap::render_image(&scope.units, w, h, format)?;
            return Ok(([(header::CONTENT_TYPE, format.to_mime_type())], data).into_response());
        }
    }
    Err(AppError::Status(StatusCode::NOT_ACCEPTABLE))
}

async fn mode_report(
    scope: &Scope<'_>,
    state: &AppState,
    uri: Uri,
    query: ReportQuery,
    ctx: TemplateContext,
    acceptable: &[Mime],
    current_user: Option<CurrentUser>,
) -> Result<Response, AppError> {
    for mime in acceptable {
        if (mime.type_() == mime::STAR && mime.subtype() == mime::STAR)
            || (mime.type_() == mime::TEXT && mime.subtype() == mime::HTML)
        {
            return render_report(scope, state, uri, current_user, ctx).await;
        } else if mime.type_() == mime::APPLICATION && mime.subtype() == mime::JSON {
            let flattened = scope.report.report.flatten();
            return Ok(Json(flattened).into_response());
        } else if mime.type_() == mime::APPLICATION && mime.subtype() == PROTOBUF {
            let flattened = scope.report.report.flatten();
            return Ok(Protobuf(&flattened).into_response());
        } else if mime.type_() == mime::IMAGE && mime.subtype() == mime::SVG {
            let (w, h) = query.size();
            let svg = treemap::render_svg(&scope.units, w, h);
            return Ok(([(header::CONTENT_TYPE, mime::IMAGE_SVG.as_ref())], svg).into_response());
        } else if mime.type_() == mime::IMAGE {
            let format = if mime.subtype() == mime::STAR {
                // Default to PNG
                ImageFormat::Png
            } else {
                ImageFormat::from_mime_type(mime.essence_str())
                    .ok_or_else(|| AppError::Status(StatusCode::NOT_ACCEPTABLE))?
            };
            let (w, h) = query.size();
            let data = treemap::render_image(&scope.units, w, h, format)?;
            return Ok(([(header::CONTENT_TYPE, format.to_mime_type())], data).into_response());
        }
    }
    Err(AppError::Status(StatusCode::NOT_ACCEPTABLE))
}

fn mode_shield(
    &Scope { project_info, measures, label, .. }: &Scope<'_>,
    query: ReportQuery,
    acceptable: &[Mime],
) -> Result<Response, AppError> {
    let label = label.unwrap_or_else(|| project_info.project.short_name());
    for mime in acceptable {
        if (mime.type_() == mime::STAR && mime.subtype() == mime::STAR)
            || (mime.type_() == mime::IMAGE && mime.subtype() == mime::SVG)
            || (mime.type_() == mime::TEXT && mime.subtype() == mime::HTML)
        {
            let data = badge::render_svg(measures, label, &query.shield)?;
            return Ok(([(header::CONTENT_TYPE, mime::IMAGE_SVG.as_ref())], data).into_response());
        } else if mime.type_() == mime::APPLICATION && mime.subtype() == mime::JSON {
            let data = badge::render(measures, label, &query.shield)?;
            return Ok(Json(data).into_response());
        } else if mime.type_() == mime::IMAGE {
            let format = if mime.subtype() == mime::STAR {
                // Default to PNG
                ImageFormat::Png
            } else {
                ImageFormat::from_mime_type(mime.essence_str())
                    .ok_or_else(|| AppError::Status(StatusCode::NOT_ACCEPTABLE))?
            };
            let data = badge::render_image(measures, label, &query.shield, format)?;
            return Ok(([(header::CONTENT_TYPE, format.to_mime_type())], data).into_response());
        }
    }
    Err(AppError::Status(StatusCode::NOT_ACCEPTABLE))
}

fn mode_measures(
    &Scope { measures, .. }: &Scope<'_>,
    acceptable: &[Mime],
) -> Result<Response, AppError> {
    for mime in acceptable {
        if (mime.type_() == mime::STAR && mime.subtype() == mime::STAR)
            || (mime.type_() == mime::APPLICATION && mime.subtype() == mime::JSON)
        {
            return Ok(Json(measures).into_response());
        } else if mime.type_() == mime::APPLICATION && mime.subtype() == PROTOBUF {
            return Ok(Protobuf(measures).into_response());
        }
    }
    Err(AppError::Status(StatusCode::NOT_ACCEPTABLE))
}

#[derive(Serialize)]
struct ReportHistoryEntry {
    timestamp: String,
    commit_sha: String,
    commit_message: Option<String>,
    measures: TemplateMeasures,
}

async fn mode_history(
    scope: &Scope<'_>,
    state: &AppState,
    uri: Uri,
    query: ReportQuery,
    ctx: TemplateContext,
    acceptable: &[Mime],
    current_user: Option<CurrentUser>,
) -> Result<Response, AppError> {
    let report_measures =
        state.db.fetch_all_reports(&scope.project_info.project, &scope.report.version).await?;
    let mut result = Vec::with_capacity(report_measures.len());
    for report in report_measures {
        let mut measures =
            Some(*report.report.measures(scope.project_info.project.default_category.as_deref()));
        if let Some(unit_name) = query.unit.as_ref() {
            let full_report = state.db.upgrade_report(&report).await?;
            measures = full_report
                .report
                .units
                .iter()
                .find(|u| &u.name == unit_name)
                .and_then(|c| c.measures.as_ref())
                .copied();
        } else if let Some(category_id) = query.category.as_ref() {
            measures = report
                .report
                .categories
                .iter()
                .find(|c| &c.id == category_id)
                .and_then(|c| c.measures.as_ref())
                .copied();
        }
        let Some(measures) = &measures else {
            continue;
        };
        result.push(ReportHistoryEntry {
            timestamp: report
                .commit
                .timestamp
                .format(&Rfc3339)
                .unwrap_or_else(|_| "[invalid]".to_string()),
            commit_sha: report.commit.sha,
            commit_message: report.commit.message,
            measures: TemplateMeasures::from(measures),
        });
    }
    for mime in acceptable {
        if (mime.type_() == mime::STAR && mime.subtype() == mime::STAR)
            || (mime.type_() == mime::TEXT && mime.subtype() == mime::HTML)
        {
            let rendered = render_history(scope, state, uri, current_user, ctx, result).await?;
            return Ok(rendered.into_response());
        } else if mime.type_() == mime::APPLICATION && mime.subtype() == mime::JSON {
            return Ok(Json(result).into_response());
        }
    }
    Err(AppError::Status(StatusCode::NOT_ACCEPTABLE))
}

const EMPTY_MEASURES: Measures = Measures {
    fuzzy_match_percent: 0.0,
    total_code: 0,
    matched_code: 0,
    matched_code_percent: 0.0,
    total_data: 0,
    matched_data: 0,
    matched_data_percent: 0.0,
    total_functions: 0,
    matched_functions: 0,
    matched_functions_percent: 0.0,
    complete_code: 0,
    complete_code_percent: 0.0,
    complete_data: 0,
    complete_data_percent: 0.0,
    total_units: 0,
    complete_units: 0,
};

struct Scope<'a> {
    report: &'a FullReportFile,
    project_info: &'a ProjectInfo,
    measures: &'a Measures,
    current_category: Option<&'a ReportCategory>,
    current_unit: Option<&'a ReportUnit>,
    units: Vec<ReportTemplateUnit<'a>>,
    label: Option<&'a str>,
}

fn apply_scope<'a>(
    report: &'a FullReportFile,
    project_info: &'a ProjectInfo,
    query: &ReportQuery,
) -> Result<Scope<'a>> {
    let mut measures = &report.report.measures;
    let mut current_category = None;
    let mut category_id_filter = None;
    if let Some(category) = query
        .category
        .as_deref()
        .or(project_info.project.default_category.as_deref())
        .and_then(|id| report.report.categories.iter().find(|c| c.id == *id))
    {
        measures = category.measures.as_ref().unwrap_or(&EMPTY_MEASURES);
        current_category = Some(category);
        category_id_filter = Some(category.id.clone());
    }
    let mut current_unit = None;
    if let Some(unit) = query
        .unit
        .as_ref()
        .and_then(|unit_name| report.report.units.iter().find(|u| u.name == *unit_name))
    {
        measures = unit.measures.as_ref().unwrap_or(&EMPTY_MEASURES);
        current_unit = Some(unit.as_ref());
    }
    let (w, h) = query.size();
    let mut units = if let Some(unit) = current_unit {
        let linked = unit.metadata.as_ref().is_some_and(|m| m.complete.is_some_and(|c| c));
        unit.functions
            .iter()
            .filter_map(|f| {
                if f.size == 0 {
                    return None;
                }
                Some(ReportTemplateUnit {
                    name: f
                        .metadata
                        .as_ref()
                        .and_then(|m| m.demangled_name.as_deref())
                        .unwrap_or(&f.name),
                    total_code: f.size,
                    fuzzy_match_percent: f.fuzzy_match_percent,
                    color: unit_color(f.fuzzy_match_percent),
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 0.0,
                    is_linked: linked,
                })
            })
            .collect::<Vec<_>>()
    } else {
        report
            .report
            .units
            .iter()
            .filter_map(|unit| {
                if let Some(category_id) = &category_id_filter
                    && !unit
                        .metadata
                        .as_ref()
                        .is_some_and(|m| m.progress_categories.iter().any(|c| c == category_id))
                {
                    return None;
                }
                let measures = unit.measures.as_ref()?;
                if measures.total_code == 0 {
                    return None;
                }
                Some(ReportTemplateUnit {
                    name: &unit.name,
                    total_code: measures.total_code,
                    fuzzy_match_percent: measures.fuzzy_match_percent,
                    color: unit_color(measures.fuzzy_match_percent),
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 0.0,
                    is_linked: unit
                        .metadata
                        .as_ref()
                        .is_some_and(|m| m.complete.is_some_and(|c| c)),
                })
            })
            .collect::<Vec<_>>()
    };
    layout_units(
        &mut units,
        w as f32 / h as f32,
        |i| i.total_code as f32,
        |i, r| {
            i.x = r.x;
            i.y = r.y;
            i.w = r.w;
            i.h = r.h;
        },
    );
    let label = current_unit
        .as_ref()
        .map(|u| u.name.rsplit_once('/').map_or(u.name.as_str(), |(_, name)| name))
        .or_else(|| {
            // Only show a category label if it is not the default category
            let default_category_id =
                project_info.project.default_category.as_deref().unwrap_or("all");
            let current_category_id = current_category.map(|c| c.id.as_str()).unwrap_or("all");
            (current_category_id != default_category_id)
                .then(|| current_category.map(|c| c.name.as_str()).unwrap_or("All"))
        });
    Ok(Scope { report, project_info, measures, current_category, current_unit, units, label })
}

async fn render_report(
    scope: &Scope<'_>,
    state: &AppState,
    uri: Uri,
    current_user: Option<CurrentUser>,
    mut ctx: TemplateContext,
) -> Result<Response, AppError> {
    let Scope {
        report,
        project_info,
        measures,
        current_category: current_category_ref,
        current_unit,
        units,
        label,
    } = scope;
    let current_category = *current_category_ref;

    let mut commit_message = report.commit.message.clone();
    if commit_message.is_none() {
        let commit = match state
            .github
            .get_commit(&project_info.project.owner, &project_info.project.repo, &report.commit.sha)
            .await
        {
            Ok(commit) => commit,
            Err(e) => {
                tracing::warn!(
                    "Failed to get commit {}/{}@{}: {}",
                    project_info.project.owner,
                    project_info.project.repo,
                    report.commit.sha,
                    e
                );
                None
            }
        };
        if let Some(commit) = commit {
            state
                .db
                .update_report_message(project_info.project.id, &report.commit.sha, &commit.message)
                .await?;
            commit_message = Some(commit.message);
        }
    }

    let request_url = Url::parse(&uri.to_string()).context("Failed to parse URI")?;
    let project_base_path =
        format!("/{}/{}", project_info.project.owner, project_info.project.repo);
    let project_history_path = request_url.query_param("mode", Some("history"));
    let project_manage_path =
        format!("/manage/{}/{}", project_info.project.owner, project_info.project.repo);
    let can_manage =
        current_user.as_ref().is_some_and(|u| u.can_manage_project(&project_info.project));
    let default_category = project_info.project.default_category();

    let is_default_version = project_info.default_version() == Some(report.version.as_str());
    let is_latest_commit = project_info.next_commit.is_none();
    let is_default_category = current_category.is_none_or(|c| c.id == default_category);
    let is_primary_view =
        is_latest_commit && is_default_version && is_default_category && current_unit.is_none();

    let canonical_url = if is_default_version && is_latest_commit {
        request_url
            .with_path(&format!("/{}/{}", project_info.project.owner, project_info.project.repo))
    } else if is_latest_commit {
        request_url.with_path(&format!(
            "/{}/{}/{}",
            project_info.project.owner, project_info.project.repo, report.version
        ))
    } else {
        request_url.with_path(&format!(
            "/{}/{}/{}/{}",
            project_info.project.owner,
            project_info.project.repo,
            report.version,
            report.commit.sha
        ))
    };

    let image_url = canonical_url
        .with_path(&format!("{}.png", canonical_url.path()))
        .query_param("mode", Some("report"));

    let versions = project_info
        .report_versions
        .iter()
        .map(|version| {
            let version_url = if is_latest_commit {
                request_url.with_path(&format!(
                    "/{}/{}/{}",
                    project_info.project.owner, project_info.project.repo, version
                ))
            } else {
                request_url.with_path(&format!(
                    "/{}/{}/{}/{}",
                    project_info.project.owner,
                    project_info.project.repo,
                    version,
                    report.commit.sha
                ))
            };
            ReportTemplateVersion { id: version, path: version_url.path_and_query().to_string() }
        })
        .collect::<Vec<_>>();

    let CategorySelection { categories, current_top_index, current_sub_index } =
        build_category_selection(
            &canonical_url,
            &report.report.categories,
            current_category,
            default_category,
        );
    let current_top_category = &categories[current_top_index];
    let current_category_item = current_sub_index
        .map(|i| current_top_category.subcategories[i].clone())
        .unwrap_or_else(|| ReportCategoryItem {
            id: current_top_category.category.id,
            name: "All",
            path: current_top_category.category.path.clone(),
        });

    let prev_commit_path = project_info.prev_commit.as_deref().map(|commit| {
        let url = request_url.with_path(&format!(
            "/{}/{}/{}/{}",
            project_info.project.owner, project_info.project.repo, report.version, commit
        ));
        url.path_and_query().to_string()
    });
    let next_commit_path = project_info.next_commit.as_deref().map(|commit| {
        let url = request_url.with_path(&format!(
            "/{}/{}/{}/{}",
            project_info.project.owner, project_info.project.repo, report.version, commit
        ));
        url.path_and_query().to_string()
    });
    let latest_commit_path = project_info.next_commit.as_deref().map(|_| {
        let url = request_url.with_path(&format!(
            "/{}/{}/{}",
            project_info.project.owner, project_info.project.repo, report.version
        ));
        url.path_and_query().to_string()
    });

    let units_path = canonical_url.query_param("unit", None).path_and_query().to_string();
    let commit_message = commit_message.as_deref().and_then(|message| message.lines().next());
    let commit_url = format!("{}/commit/{}", project_info.project.repo_url(), report.commit.sha);
    let source_file_url = current_unit
        .and_then(|u| u.metadata.as_ref())
        .and_then(|m| m.source_path.as_deref())
        .map(|path| {
            format!("{}/blob/{}/{}", project_info.project.repo_url(), report.commit.sha, path)
        });
    let project_name = if let Some(label) = label {
        Cow::Owned(format!("{} ({})", project_info.project.name(), label))
    } else {
        project_info.project.name()
    };
    let project_short_name = project_info.project.short_name();
    let project_short_name_with_label = if let Some(label) = label {
        Cow::Owned(format!("{project_short_name} ({label})"))
    } else {
        Cow::Borrowed(project_short_name)
    };

    // Only show visibility banners if we're on the primary view
    let visibility = if is_primary_view {
        project_visibility(&project_info.project, Some(measures))
    } else {
        ProjectVisibility::Visible
    };

    // Load blocking resources first so we don't duplicate them
    let header = ctx.header().await;
    let report_chunks = ctx.chunks("report", Load::Blocking).await;

    let rendered = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { (project_short_name_with_label) " • Progress Report" }
                (header)
                (ctx.chunks("main", Load::Deferred).await)
                (ctx.chunks("report", Load::Preload).await)
                link rel="canonical" href=(canonical_url);
                @if let Some(prev_commit_path) = prev_commit_path.as_deref() {
                    link rel="prev" href=(prev_commit_path);
                }
                @if let Some(next_commit_path) = next_commit_path.as_deref() {
                    link rel="next" href=(next_commit_path);
                }
                meta name="description" content=(format!("Decompilation progress report for {project_name}"));
                meta property="og:title" content=(format!("{project_short_name_with_label} is {} decompiled", format_percent(measures.matched_code_percent)));
                meta property="og:description" content=(format!("Decompilation progress report for {project_name}"));
                meta property="og:image" content=(image_url);
                meta property="og:url" content=(canonical_url);
                @if !is_primary_view {
                    // Prevent search engines from indexing anything but the primary report
                    meta name="robots" content="noindex";
                }
            }
            body {
                header {
                    nav {
                        ul {
                            li {
                                a href="/" { strong { "decomp.dev" } }
                            }
                            li {
                                a href="/projects" { "Projects" }
                            }
                            li {
                                a href=(project_base_path) { (project_short_name) }
                            }
                            li.md {
                                details.dropdown {
                                    summary { (report.version) }
                                    ul {
                                        @for version in &versions {
                                            li {
                                                a href=(version.path) { (version.id) }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        (nav_links())
                    }
                }
                main {
                    @match visibility {
                        ProjectVisibility::Visible => {},
                        ProjectVisibility::Disabled => {
                            article.warning-card { "This project is disabled." }
                        }
                        ProjectVisibility::Hidden => {
                            article.warning-card { "This project is hidden until it has reached a minimum of 0.5% matched code." }
                        }
                    }
                    .actions {
                        details.dropdown {
                            summary {}
                            ul dir="rtl" {
                                li {
                                    a href=(format!("/api?project={}", project_info.project.id)) {
                                        "API "
                                        span.icon-code { " " }
                                    }
                                }
                                li {
                                    a href=(project_history_path) {
                                        "History "
                                        span.icon-chart-line { " " }
                                    }
                                }
                                @if can_manage {
                                    li {
                                        a href=(project_manage_path) {
                                            "Manage "
                                            span.icon-cog { " " }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    a.secondary.outline.repo-link role="button"
                        href=(project_info.project.repo_url()) target="_blank" {
                        span.icon-github { " " }
                        span.md { "Repository" }
                    }
                    h3.report-title { (format!("{project_short_name_with_label} is {} decompiled", format_percent(measures.matched_code_percent))) }
                    @if current_unit.is_none() && measures.complete_code_percent > 0.0 {
                        h4.muted { (format!("{} fully linked", format_percent(measures.complete_code_percent))) }
                    }
                    @if let Some(source_file_url) = source_file_url {
                        h4.muted {
                            a href=(source_file_url) target="_blank" { "View source file" }
                        }
                    }
                    details.dropdown.sm {
                        summary { (report.version) }
                        ul {
                            @for version in &versions {
                                li {
                                    a href=(version.path) { (version.id) }
                                }
                            }
                        }
                    }
                    @if measures.total_code > 0 {
                        h6 {
                            "Code "
                            small.muted { "(" (size(measures.total_code)) ")" }
                        }
                        (ctx.code_progress_sections(&measures))
                    }
                    @if measures.total_data > 0 {
                        h6 {
                            "Data "
                            small.muted { "(" (size(measures.total_data)) ")" }
                        }
                        (ctx.data_progress_sections(&measures))
                    }
                    h6 { "Commit" }
                    div {
                        @if let Some(message) = commit_message {
                            pre {
                                a href=(commit_url) target="_blank" {
                                    (report.commit.sha[..7])
                                }
                                " | "
                                (message)
                            }
                        }
                        div role="group" {
                            @if let Some(prev_commit_path) = prev_commit_path {
                                a.outline.secondary role="button" href=(prev_commit_path) {
                                    span .icon-left-open .md {}
                                    " Previous"
                                }
                            } @else {
                                button.outline.secondary disabled {
                                    span .icon-left-open .md {}
                                    " Previous"
                                }
                            }
                            @if let Some(next_commit_path) = next_commit_path {
                                a.outline.secondary role="button" href=(next_commit_path) {
                                    "Next "
                                    span .icon-right-open .md {}
                                }
                            } @else {
                                button.outline.secondary disabled {
                                    "Next "
                                    span .icon-right-open .md {}
                                }
                            }
                            @if let Some(latest_commit_path) = latest_commit_path {
                                a.primary role="button" href=(latest_commit_path) {
                                    "Latest"
                                }
                            } @else {
                                button.primary disabled {
                                    "Latest"
                                }
                            }
                        }
                    }
                    @if current_unit.is_some() {
                        h6 { "Functions" }
                        div role="group" {
                            a role="button" href=(units_path) { "Back to units" }
                        }
                    } @else {
                        h6 { "Units" }
                        @if categories.len() > 1 {
                            details.dropdown {
                                summary { (current_top_category.category.name) }
                                ul {
                                    @for category in &categories {
                                        li {
                                            a href=(category.category.path) { (category.category.name) }
                                        }
                                    }
                                }
                            }
                            @if !current_top_category.subcategories.is_empty() {
                                details.dropdown {
                                    summary { (current_category_item.name) }
                                    ul {
                                        li {
                                            a href=(current_top_category.category.path) { "All" }
                                        }
                                        @for sub in &current_top_category.subcategories {
                                            li {
                                                a href=(sub.path) { (sub.name) }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    label {
                        input name="filter" required placeholder="Filter, e.g.: 'camera <70% >10kb'";
                    }
                    @if units.is_empty() {
                        p.muted {
                            @if current_unit.is_some() {
                                "No function information available."
                            } @else {
                                "No unit information available."
                            }
                        }
                    } @else {
                        canvas #treemap {}
                        (report_chunks)
                        script nonce=[ctx.nonce.as_deref()] {
                            (PreEscaped(r#"window.units="#))
                            (escape_script(&serde_json::to_string(&units)?))
                            (PreEscaped(r#";drawTreemap("treemap","#))
                            (current_unit.is_none())
                            (PreEscaped(r#",window.units)"#))
                        }
                        noscript {
                            style nonce=[ctx.nonce.as_deref()] { "canvas{display:none}" }
                            img #treemap src=(image_url) alt="Progress graph";
                        }
                    }
                }
            }
            (ctx.footer(current_user.as_ref()))
        }
    };
    Ok((ctx, rendered).into_response())
}

async fn render_history(
    scope: &Scope<'_>,
    _state: &AppState,
    uri: Uri,
    current_user: Option<CurrentUser>,
    mut ctx: TemplateContext,
    result: Vec<ReportHistoryEntry>,
) -> Result<Response, AppError> {
    let Scope {
        report,
        project_info,
        measures,
        current_category: current_category_ref,
        current_unit,
        units: _,
        label,
    } = scope;
    let current_category = *current_category_ref;

    let request_url = Url::parse(&uri.to_string()).context("Failed to parse URI")?;
    let project_base_path =
        format!("/{}/{}", project_info.project.owner, project_info.project.repo);
    let canonical_url = request_url.with_path(&format!(
        "/{}/{}/{}",
        project_info.project.owner, project_info.project.repo, report.version
    ));
    let image_url = canonical_url
        .with_path(&format!("{}.png", canonical_url.path()))
        .query_param("mode", Some("report"));

    let versions = project_info
        .report_versions
        .iter()
        .map(|version| {
            let version_url = request_url.with_path(&format!(
                "/{}/{}/{}/{}",
                project_info.project.owner, project_info.project.repo, version, report.commit.sha
            ));
            ReportTemplateVersion { id: version, path: version_url.path_and_query().to_string() }
        })
        .collect::<Vec<_>>();

    let default_category = project_info.project.default_category();
    let CategorySelection { categories, current_top_index, current_sub_index } =
        build_category_selection(
            &canonical_url,
            &report.report.categories,
            current_category,
            default_category,
        );
    let current_top_category = &categories[current_top_index];
    let current_category_item = current_sub_index
        .map(|i| &current_top_category.subcategories[i])
        .unwrap_or(&current_top_category.category);

    let project_name = if let Some(label) = label {
        Cow::Owned(format!("{} ({})", project_info.project.name(), label))
    } else {
        project_info.project.name()
    };
    let project_short_name = project_info.project.short_name();
    let project_short_name_with_label = if let Some(label) = label {
        Cow::Owned(format!("{project_short_name} ({label})"))
    } else {
        Cow::Borrowed(project_short_name)
    };

    // Load blocking resources first so we don't duplicate them
    let header = ctx.header().await;
    let history_chunks = ctx.chunks("history", Load::Blocking).await;

    let rendered = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { (project_short_name_with_label) " • Progress History" }
                (header)
                (ctx.chunks("main", Load::Deferred).await)
                (ctx.chunks("history", Load::Preload).await)
                link rel="canonical" href=(canonical_url);
                meta name="description" content=(format!("Decompilation progress history for {project_name}"));
                meta property="og:title" content=(format!("{project_short_name_with_label} is {} decompiled", format_percent(measures.matched_code_percent)));
                meta property="og:description" content=(format!("Decompilation progress history for {project_name}"));
                meta property="og:image" content=(image_url);
                meta property="og:url" content=(canonical_url);
            }
            body {
                header {
                    nav {
                        ul {
                            li {
                                a href="/" { strong { "decomp.dev" } }
                            }
                            li {
                                a href="/projects" { "Projects" }
                            }
                            li {
                                a href=(project_base_path) { (project_short_name) }
                            }
                            li {
                                a href=(request_url) { "History" }
                            }
                        }
                        (nav_links())
                    }
                }
                main {
                    h3 { "History for " (project_short_name_with_label) }
                    details.dropdown title="Version" {
                        summary { (report.version) }
                        ul {
                            @for version in &versions {
                                li {
                                    a href=(version.path) { (version.id) }
                                }
                            }
                        }
                    }
                    @if current_unit.is_none() && categories.len() > 1 {
                        details.dropdown title="Category" {
                            summary { (current_top_category.category.name) }
                            ul {
                                @for category in &categories {
                                    li {
                                        a href=(category.category.path) { (category.category.name) }
                                    }
                                }
                            }
                        }
                        @if !current_top_category.subcategories.is_empty() {
                            details.dropdown title="Subcategory" {
                                summary { (current_category_item.name) }
                                ul {
                                    li {
                                        a href=(current_top_category.category.path) { (current_top_category.category.name) }
                                    }
                                    @for sub in &current_top_category.subcategories {
                                        li {
                                            a href=(sub.path) { (sub.name) }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    #chart {}
                    (history_chunks)
                    script nonce=[ctx.nonce.as_deref()] {
                        (PreEscaped(r#"window.historyData="#))
                        (escape_script(&serde_json::to_string(&result)?))
                        (PreEscaped(r#";renderChart("chart",window.historyData)"#))
                    }
                    hr;
                    div role="group" {
                        a role="button" href=(project_base_path) { "Back to report" }
                    }
                }
            }
            (ctx.footer(current_user.as_ref()))
        }
    };
    Ok((ctx, rendered).into_response())
}
