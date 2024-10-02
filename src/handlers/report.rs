use std::{borrow::Cow, iter, time::Instant};

use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
    Json,
};
use image::ImageFormat;
use mime::Mime;
use objdiff_core::bindings::report::{Measures, ReportCategory, ReportUnit};
use serde::{Deserialize, Serialize};
use url::Url;

use super::{badge, parse_accept, treemap, AppError, FullUri, Protobuf, PROTOBUF};
use crate::{
    models::{Project, ProjectInfo, ReportFile},
    templates::render,
    util::UrlExt,
    AppState,
};

#[derive(Deserialize)]
pub struct ReportParams {
    owner: String,
    repo: String,
    version: Option<String>,
    commit: Option<String>,
}

const DEFAULT_IMAGE_WIDTH: u32 = 950;
const DEFAULT_IMAGE_HEIGHT: u32 = 475;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportQuery {
    mode: Option<String>,
    category: Option<String>,
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
struct ReportTemplateContext<'a> {
    project: &'a Project,
    project_name: &'a str,
    project_short_name: &'a str,
    project_url: &'a str,
    project_path: &'a str,
    commit: &'a str,
    version: &'a str,
    measures: TemplateMeasures,
    units: &'a [ReportTemplateUnit<'a>],
    versions: &'a [ReportTemplateVersion<'a>],
    prev_commit_path: Option<&'a str>,
    next_commit_path: Option<&'a str>,
    latest_commit_path: Option<&'a str>,
    categories: &'a [ReportCategoryItem<'a>],
    current_category: &'a ReportCategoryItem<'a>,
    canonical_path: &'a str,
    canonical_url: &'a str,
    image_url: &'a str,
    current_unit: Option<&'a str>,
    units_path: &'a str,
    commit_message: Option<&'a str>,
    commit_url: &'a str,
    source_file_url: Option<&'a str>,
}

#[derive(Serialize)]
pub struct ReportTemplateUnit<'a> {
    name: &'a str,
    total_code: u64,
    fuzzy_match_percent: f32,
    color: String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

#[derive(Serialize, Clone)]
struct ReportCategoryItem<'a> {
    id: &'a str,
    name: &'a str,
    path: String,
}

#[derive(Serialize, Clone)]
struct ReportTemplateVersion<'a> {
    id: &'a str,
    path: String,
}

/// Duplicate of Measures to avoid omitting empty fields
#[derive(Serialize)]
struct TemplateMeasures {
    fuzzy_match_percent: f32,
    total_code: u64,
    matched_code: u64,
    matched_code_percent: f32,
    total_data: u64,
    matched_data: u64,
    matched_data_percent: f32,
    total_functions: u32,
    matched_functions: u32,
    matched_functions_percent: f32,
    complete_code: u64,
    complete_code_percent: f32,
    complete_data: u64,
    complete_data_percent: f32,
    total_units: u32,
    complete_units: u32,
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

fn extract_extension(params: ReportParams) -> (ReportParams, Option<String>) {
    if let Some(commit) = params.commit.as_deref() {
        if let Some((commit, ext)) = commit.rsplit_once('.') {
            return (
                ReportParams { commit: Some(commit.to_string()), ..params },
                Some(ext.to_string()),
            );
        }
    } else if let Some(version) = params.version.as_deref() {
        if let Some((version, ext)) = version.rsplit_once('.') {
            return (
                ReportParams { version: Some(version.to_string()), ..params },
                Some(ext.to_string()),
            );
        }
    } else if let Some((repo, ext)) = params.repo.rsplit_once('.') {
        return (ReportParams { repo: repo.to_string(), ..params }, Some(ext.to_string()));
    }
    (params, None)
}

pub async fn get_report(
    Path(params): Path<ReportParams>,
    Query(query): Query<ReportQuery>,
    headers: HeaderMap,
    FullUri(uri): FullUri,
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let start = Instant::now();
    let (params, ext) = extract_extension(params);
    let acceptable = parse_accept(&headers, ext.as_deref());
    if acceptable.is_empty() {
        return Err(AppError::Status(StatusCode::NOT_ACCEPTABLE));
    }

    let mut commit = params.commit.as_deref();
    if matches!(commit, Some(c) if c.eq_ignore_ascii_case("latest")) {
        commit = None;
    }
    let Some(project_info) = state.db.get_project_info(&params.owner, &params.repo, commit).await?
    else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let version = if let Some(version) = &params.version {
        if version.eq_ignore_ascii_case("default") {
            project_info.default_version().ok_or(AppError::Status(StatusCode::NOT_FOUND))?
        } else {
            version.as_str()
        }
    } else if let Some(default_version) = project_info.default_version() {
        default_version
    } else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let Some(report) =
        state.db.get_report(&params.owner, &params.repo, &project_info.commit.sha, version).await?
    else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };

    let scope = apply_scope(&report, &project_info, &query)?;
    match query.mode.as_deref().unwrap_or("report").to_ascii_lowercase().as_str() {
        "shield" => mode_shield(&scope, query, &acceptable),
        "report" => mode_report(&scope, &state, uri, query, start, &acceptable).await,
        _ => Err(AppError::Status(StatusCode::BAD_REQUEST)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn mode_report(
    scope: &Scope<'_>,
    state: &AppState,
    uri: Uri,
    query: ReportQuery,
    start: Instant,
    acceptable: &[Mime],
) -> Result<Response, AppError> {
    for mime in acceptable {
        if (mime.type_() == mime::STAR && mime.subtype() == mime::STAR)
            || (mime.type_() == mime::TEXT && mime.subtype() == mime::HTML)
        {
            let mut rendered = render_template(scope, state, uri).await?;
            let elapsed = start.elapsed();
            rendered = rendered.replace("[[time]]", &format!("{}ms", elapsed.as_millis()));
            return Ok(Html(rendered).into_response());
        } else if mime.type_() == mime::APPLICATION && mime.subtype() == mime::JSON {
            return Ok(Json(scope.report.report.clone()).into_response());
        } else if mime.type_() == mime::APPLICATION && mime.subtype() == PROTOBUF {
            return Ok(Protobuf(scope.report.report.clone()).into_response());
        } else if mime.type_() == mime::IMAGE && mime.subtype() == mime::SVG {
            let (w, h) = query.size();
            let svg = treemap::render_svg(&scope.units, w, h, state)?;
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
            let data = treemap::render_image(&scope.units, w, h, state, format)?;
            return Ok(([(header::CONTENT_TYPE, format.to_mime_type())], data).into_response());
        }
    }
    Err(AppError::Status(StatusCode::NOT_ACCEPTABLE))
}

fn mode_shield(
    Scope { report, measures, label, .. }: &Scope<'_>,
    query: ReportQuery,
    acceptable: &[Mime],
) -> Result<Response, AppError> {
    let label = label.unwrap_or_else(|| report.project.short_name());
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
    report: &'a ReportFile,
    project_info: &'a ProjectInfo,
    measures: &'a Measures,
    current_category: Option<&'a ReportCategory>,
    current_unit: Option<&'a ReportUnit>,
    units: Vec<ReportTemplateUnit<'a>>,
    label: Option<&'a str>,
}

fn apply_scope<'a>(
    report: &'a ReportFile,
    project_info: &'a ProjectInfo,
    query: &ReportQuery,
) -> Result<Scope<'a>> {
    let mut measures = report.report.measures.as_ref().unwrap_or(&EMPTY_MEASURES);
    let mut current_category = None;
    let mut category_id_filter = None;
    if let Some(category) =
        query.category.as_ref().and_then(|id| report.report.categories.iter().find(|c| c.id == *id))
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
        current_unit = Some(unit);
    }
    let (w, h) = query.size();
    let mut units =
        if let Some(unit) = current_unit {
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
                        color: treemap::unit_color(f.fuzzy_match_percent),
                        x: 0.0,
                        y: 0.0,
                        w: 0.0,
                        h: 0.0,
                    })
                })
                .collect::<Vec<_>>()
        } else {
            report
                .report
                .units
                .iter()
                .filter_map(|unit| {
                    if let Some(category_id) = &category_id_filter {
                        if !unit.metadata.as_ref().map_or(false, |m| {
                            m.progress_categories.iter().any(|c| c == category_id)
                        }) {
                            return None;
                        }
                    }
                    let measures = unit.measures.as_ref()?;
                    if measures.total_code == 0 {
                        return None;
                    }
                    Some(ReportTemplateUnit {
                        name: &unit.name,
                        total_code: measures.total_code,
                        fuzzy_match_percent: measures.fuzzy_match_percent,
                        color: treemap::unit_color(measures.fuzzy_match_percent),
                        x: 0.0,
                        y: 0.0,
                        w: 0.0,
                        h: 0.0,
                    })
                })
                .collect::<Vec<_>>()
        };
    treemap::layout_units(
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
        .or_else(|| current_category.as_ref().map(|c| c.name.as_str()));
    Ok(Scope { report, project_info, measures, current_category, current_unit, units, label })
}

async fn render_template(scope: &Scope<'_>, state: &AppState, uri: Uri) -> Result<String> {
    let Scope { report, project_info, measures, current_category, current_unit, units, label } =
        scope;
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

    let request_url = Url::parse(&uri.to_string()).context("Failed to parse URI")?;
    let project_base_path =
        format!("/{}/{}", project_info.project.owner, project_info.project.repo);
    let canonical_url = request_url.with_path(&format!(
        "/{}/{}/{}/{}",
        project_info.project.owner, project_info.project.repo, report.version, report.commit.sha
    ));
    let image_url = canonical_url.with_path(&format!("{}.png", canonical_url.path()));

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

    let all_url = canonical_url.query_param("category", None);
    let all_category =
        ReportCategoryItem { id: "all", name: "All", path: all_url.path_and_query().to_string() };
    let current_category = current_category
        .map(|c| {
            let path =
                canonical_url.query_param("category", Some(&c.id)).path_and_query().to_string();
            ReportCategoryItem { id: &c.id, name: &c.name, path }
        })
        .unwrap_or_else(|| all_category.clone());
    let categories = iter::once(all_category)
        .chain(report.report.categories.iter().map(|c| {
            let path =
                canonical_url.query_param("category", Some(&c.id)).path_and_query().to_string();
            ReportCategoryItem { id: &c.id, name: &c.name, path }
        }))
        .collect::<Vec<_>>();

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
    let commit_message = commit.as_ref().and_then(|c| c.message.lines().next());
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
    let project_short_name = if let Some(label) = label {
        Cow::Owned(format!("{} ({})", project_info.project.short_name(), label))
    } else {
        Cow::Borrowed(project_info.project.short_name())
    };

    render(&state.templates, "report.html", ReportTemplateContext {
        project: &report.project,
        project_name: project_name.as_ref(),
        project_short_name: project_short_name.as_ref(),
        project_url: &report.project.repo_url(),
        project_path: &project_base_path,
        commit: &report.commit.sha,
        version: &report.version,
        measures: TemplateMeasures::from(*measures),
        units,
        versions: &versions,
        prev_commit_path: prev_commit_path.as_deref(),
        next_commit_path: next_commit_path.as_deref(),
        latest_commit_path: latest_commit_path.as_deref(),
        categories: &categories,
        current_category: &current_category,
        canonical_path: canonical_url.path_and_query(),
        canonical_url: canonical_url.as_ref(),
        image_url: image_url.as_ref(),
        current_unit: current_unit.map(|u| u.name.as_str()),
        units_path: &units_path,
        commit_message,
        commit_url: &commit_url,
        source_file_url: source_file_url.as_deref(),
    })
}
