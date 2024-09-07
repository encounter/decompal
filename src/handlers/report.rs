use std::{iter, str::FromStr, time::Instant};

use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    Json,
};
use image::ImageFormat;
use mime::Mime;
use objdiff_core::bindings::report::Measures;
use serde::{Deserialize, Serialize};

use super::{graph::layout_units, parse_accept, AppError, Protobuf, PROTOBUF};
use crate::{
    handlers::graph::{render_image, render_svg, unit_color},
    models::{Project, ProjectInfo, ReportFile},
    templates::render,
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
    category: Option<String>,
    w: Option<u32>,
    h: Option<u32>,
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
    versions: &'a [String],
    prev_commit: Option<&'a str>,
    next_commit: Option<&'a str>,
    categories: &'a [ReportCategoryItem<'a>],
    current_category: &'a ReportCategoryItem<'a>,
    units_json: String,
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

#[derive(Serialize, Copy, Clone)]
struct ReportCategoryItem<'a> {
    id: &'a str,
    name: &'a str,
}

impl Default for ReportCategoryItem<'static> {
    fn default() -> Self { Self { id: "all", name: "All" } }
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
    return (params, None);
}

pub async fn get_report(
    Path(params): Path<ReportParams>,
    Query(query): Query<ReportQuery>,
    headers: HeaderMap,
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

    let (w, h) = query.size();
    let (measures, current_category, units) = apply_category(&report, &query)?;

    let acceptable = if let Some(ext) = ext {
        vec![match ext.to_ascii_lowercase().as_str() {
            "json" => mime::APPLICATION_JSON,
            "binpb" | "proto" => Mime::from_str("application/x-protobuf").unwrap(),
            "svg" => mime::IMAGE_SVG,
            _ => {
                if let Some(format) = ImageFormat::from_extension(ext) {
                    Mime::from_str(format.to_mime_type()).unwrap()
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
                &current_category,
                &units,
                &state,
            )?;
            let elapsed = start.elapsed();
            rendered = rendered.replace("[[time]]", &format!("{}ms", elapsed.as_millis()));
            return Ok(Html(rendered).into_response());
        } else if mime.type_() == mime::APPLICATION && mime.subtype() == mime::JSON {
            return Ok(Json(report.report).into_response());
        } else if mime.type_() == mime::APPLICATION && mime.subtype() == PROTOBUF {
            return Ok(Protobuf(report.report).into_response());
        } else if mime.type_() == mime::IMAGE && mime.subtype() == mime::SVG {
            let svg = render_svg(&units, w, h, &state)?;
            return Ok(([(header::CONTENT_TYPE, mime::IMAGE_SVG.as_ref())], svg).into_response());
        } else if mime.type_() == mime::IMAGE {
            let format = if mime.subtype() == mime::STAR {
                // Default to PNG
                ImageFormat::Png
            } else {
                ImageFormat::from_mime_type(mime.essence_str())
                    .ok_or_else(|| AppError::Status(StatusCode::NOT_ACCEPTABLE))?
            };
            let data = render_image(&units, w, h, &state, format)?;
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
) -> Result<(&'a Measures, ReportCategoryItem<'a>, Vec<ReportTemplateUnit<'a>>)> {
    let mut measures = report.report.measures.as_ref().unwrap_or(&EMPTY_MEASURES);
    let mut current_category = ReportCategoryItem::default();
    let mut category_id_filter = None;
    if let Some(category) =
        query.category.as_ref().and_then(|id| report.report.categories.iter().find(|c| c.id == *id))
    {
        measures = category.measures.as_ref().unwrap_or(&EMPTY_MEASURES);
        current_category = ReportCategoryItem { id: &category.id, name: &category.name };
        category_id_filter = Some(category.id.clone());
    }
    let (w, h) = query.size();
    let aspect = w as f32 / h as f32;
    let units = layout_units(&report.report, w, h, |item| {
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
            color: unit_color(match_percent),
            x: bounds.x * 100.0,
            y: bounds.y * 100.0,
            w: bounds.w * 100.0,
            h: bounds.h * 100.0,
        }
    })
    .collect();
    Ok((measures, current_category, units))
}

fn render_template(
    report: &ReportFile,
    project_info: &ProjectInfo,
    measures: &Measures,
    current_category: &ReportCategoryItem,
    units: &[ReportTemplateUnit],
    state: &AppState,
) -> Result<String> {
    let categories = iter::once(ReportCategoryItem::default())
        .chain(
            report
                .report
                .categories
                .iter()
                .map(|c| ReportCategoryItem { id: &c.id, name: &c.name }),
        )
        .collect::<Vec<_>>();
    let units_json = serde_json::to_string(&units).context("Failed to serialize units")?;
    render(&state.templates, "report.html", ReportTemplateContext {
        project: &report.project,
        project_name: &report.project.name(),
        project_short_name: &report.project.short_name(),
        project_url: &report.project.repo_url(),
        project_path: &format!("/{}/{}", report.project.owner, report.project.repo),
        commit: &report.commit.sha,
        version: &report.version,
        measures: TemplateMeasures::from(measures),
        units,
        versions: &project_info.report_versions,
        prev_commit: project_info.prev_commit.as_deref(),
        next_commit: project_info.next_commit.as_deref(),
        categories: &categories,
        current_category,
        units_json,
    })
}
