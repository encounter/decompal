use std::{iter, str::FromStr, time::Instant};

use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
    Json,
};
use image::ImageFormat;
use mime::Mime;
use objdiff_core::bindings::report::{Measures, ReportCategory};
use serde::{Deserialize, Serialize};
use url::Url;

use super::{badge, graph, parse_accept, AppError, FullUri, Protobuf, PROTOBUF};
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
pub struct ReportQuery {
    mode: Option<String>,
    category: Option<String>,
    w: Option<u32>,
    h: Option<u32>,
    #[serde(flatten)]
    shield: badge::ShieldParams,
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
    prev_commit: Option<&'a str>,
    next_commit: Option<&'a str>,
    categories: &'a [ReportCategoryItem<'a>],
    current_category: &'a ReportCategoryItem<'a>,
    canonical_path: &'a str,
    canonical_url: &'a str,
    image_url: &'a str,
}

#[derive(Serialize)]
pub struct ReportTemplateUnit<'a> {
    name: &'a str,
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

    if let Some(mode) = query.mode.as_deref() {
        match mode.to_ascii_lowercase().as_str() {
            "shield" => mode_shield(report, query, headers, ext),
            "report" => mode_report(report, project_info, &state, uri, query, headers, start, ext),
            _ => Err(AppError::Status(StatusCode::BAD_REQUEST)),
        }
    } else {
        mode_report(report, project_info, &state, uri, query, headers, start, ext)
    }
}

fn mode_report(
    report: ReportFile,
    project_info: ProjectInfo,
    state: &AppState,
    uri: Uri,
    query: ReportQuery,
    headers: HeaderMap,
    start: Instant,
    ext: Option<String>,
) -> Result<Response, AppError> {
    let (measures, current_category, units) = apply_category(&report, &query)?;
    let acceptable = if let Some(ext) = ext {
        vec![match ext.to_ascii_lowercase().as_str() {
            "json" => mime::APPLICATION_JSON,
            "binpb" | "proto" => Mime::from_str("application/x-protobuf")?,
            "svg" => mime::IMAGE_SVG,
            _ => {
                if let Some(format) = ImageFormat::from_extension(ext) {
                    Mime::from_str(format.to_mime_type())?
                } else {
                    return Err(AppError::Status(StatusCode::NOT_ACCEPTABLE));
                }
            }
        }]
    } else {
        if !headers.contains_key(header::ACCEPT) {
            return Ok(Json(report.report).into_response());
        }
        parse_accept(&headers)
    };
    for mime in acceptable {
        if (mime.type_() == mime::STAR && mime.subtype() == mime::STAR)
            || (mime.type_() == mime::TEXT && mime.subtype() == mime::HTML)
        {
            let mut rendered = render_template(
                &report,
                &project_info,
                measures,
                current_category,
                &units,
                &state,
                uri,
            )?;
            let elapsed = start.elapsed();
            rendered = rendered.replace("[[time]]", &format!("{}ms", elapsed.as_millis()));
            return Ok(Html(rendered).into_response());
        } else if mime.type_() == mime::APPLICATION && mime.subtype() == mime::JSON {
            return Ok(Json(report.report).into_response());
        } else if mime.type_() == mime::APPLICATION && mime.subtype() == PROTOBUF {
            return Ok(Protobuf(report.report).into_response());
        } else if mime.type_() == mime::IMAGE && mime.subtype() == mime::SVG {
            let (w, h) = query.size();
            let svg = graph::render_svg(&units, w, h, &state)?;
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
            let data = graph::render_image(&units, w, h, &state, format)?;
            return Ok(([(header::CONTENT_TYPE, format.to_mime_type())], data).into_response());
        }
    }
    Err(AppError::Status(StatusCode::NOT_ACCEPTABLE))
}

fn mode_shield(
    report: ReportFile,
    query: ReportQuery,
    headers: HeaderMap,
    ext: Option<String>,
) -> Result<Response, AppError> {
    let (measures, current_category, _units) = apply_category(&report, &query)?;
    let acceptable = if let Some(ext) = ext {
        vec![match ext.to_ascii_lowercase().as_str() {
            "json" => mime::APPLICATION_JSON,
            "svg" => mime::IMAGE_SVG,
            _ => {
                if let Some(format) = ImageFormat::from_extension(ext) {
                    Mime::from_str(format.to_mime_type())?
                } else {
                    return Err(AppError::Status(StatusCode::NOT_ACCEPTABLE));
                }
            }
        }]
    } else {
        if !headers.contains_key(header::ACCEPT) {
            return Ok(Json(report.report).into_response());
        }
        parse_accept(&headers)
    };
    for mime in acceptable {
        if (mime.type_() == mime::STAR && mime.subtype() == mime::STAR)
            || (mime.type_() == mime::IMAGE && mime.subtype() == mime::SVG)
            || (mime.type_() == mime::TEXT && mime.subtype() == mime::HTML)
        {
            let data = badge::render_svg(&report, measures, current_category, &query.shield)?;
            return Ok(([(header::CONTENT_TYPE, mime::IMAGE_SVG.as_ref())], data).into_response());
        } else if mime.type_() == mime::APPLICATION && mime.subtype() == mime::JSON {
            let data = badge::render(&report, measures, current_category, &query.shield)?;
            return Ok(Json(data).into_response());
        } else if mime.type_() == mime::IMAGE {
            let format = if mime.subtype() == mime::STAR {
                // Default to PNG
                ImageFormat::Png
            } else {
                ImageFormat::from_mime_type(mime.essence_str())
                    .ok_or_else(|| AppError::Status(StatusCode::NOT_ACCEPTABLE))?
            };
            let data =
                badge::render_image(&report, measures, current_category, &query.shield, format)?;
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
};

fn apply_category<'a>(
    report: &'a ReportFile,
    query: &ReportQuery,
) -> Result<(&'a Measures, Option<&'a ReportCategory>, Vec<ReportTemplateUnit<'a>>)> {
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
    let (w, h) = query.size();
    let aspect = w as f32 / h as f32;
    let units = graph::layout_units(&report.report, w, h, |item| {
        if let Some(category_id) = &category_id_filter {
            item.metadata
                .as_ref()
                .map_or(false, |m| m.progress_categories.iter().any(|c| c == category_id))
        } else {
            true
        }
    })
    .into_iter()
    .map(|item| {
        let unit = item.unit();
        let match_percent =
            unit.measures.as_ref().map(|m| m.fuzzy_match_percent).unwrap_or_default();
        let mut bounds = item.bounds;
        if aspect > 1.0 {
            bounds.y *= aspect;
            bounds.h *= aspect;
        } else {
            bounds.x /= aspect;
            bounds.w /= aspect;
        }
        ReportTemplateUnit {
            name: &unit.name,
            fuzzy_match_percent: match_percent,
            color: graph::unit_color(match_percent),
            x: bounds.x,
            y: bounds.y,
            w: bounds.w,
            h: bounds.h,
        }
    })
    .collect();
    Ok((measures, current_category, units))
}

fn render_template(
    report: &ReportFile,
    project_info: &ProjectInfo,
    measures: &Measures,
    current_category: Option<&ReportCategory>,
    units: &[ReportTemplateUnit],
    state: &AppState,
    uri: Uri,
) -> Result<String> {
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

    let all_url = canonical_url.set_query("category", None);
    let all_category =
        ReportCategoryItem { id: "all", name: "All", path: all_url.path_and_query().to_string() };
    let current_category = current_category
        .map(|c| {
            let path =
                canonical_url.set_query("category", Some(&c.id)).path_and_query().to_string();
            ReportCategoryItem { id: &c.id, name: &c.name, path }
        })
        .unwrap_or_else(|| all_category.clone());
    let categories = iter::once(all_category)
        .chain(report.report.categories.iter().map(|c| {
            let path =
                canonical_url.set_query("category", Some(&c.id)).path_and_query().to_string();
            ReportCategoryItem { id: &c.id, name: &c.name, path }
        }))
        .collect::<Vec<_>>();

    render(&state.templates, "report.html", ReportTemplateContext {
        project: &report.project,
        project_name: &report.project.name(),
        project_short_name: report.project.short_name(),
        project_url: &report.project.repo_url(),
        project_path: &project_base_path,
        commit: &report.commit.sha,
        version: &report.version,
        measures: TemplateMeasures::from(measures),
        units,
        versions: &versions,
        prev_commit: project_info.prev_commit.as_deref(),
        next_commit: project_info.next_commit.as_deref(),
        categories: &categories,
        current_category: &current_category,
        canonical_path: canonical_url.path_and_query(),
        canonical_url: canonical_url.as_ref(),
        image_url: image_url.as_ref(),
    })
}
