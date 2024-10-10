use std::{sync::Arc, time::Instant};

use anyhow::{anyhow, Context};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use objdiff_core::bindings::report::Measures;
use serde::{Deserialize, Serialize};
use tokio::{sync::Semaphore, task::JoinSet};

use super::AppError;
use crate::{templates::render, AppState};

#[derive(Serialize)]
struct ProjectsTemplateContext {
    projects: Vec<ProjectInfoContext>,
    sort_options: &'static [SortOption],
    current_sort: SortOption,
}

#[derive(Serialize)]
struct ProjectInfoContext {
    id: u64,
    path: String,
    owner: String,
    repo: String,
    name: String,
    short_name: String,
    commit: String,
    timestamp: DateTime<Utc>,
    measures: Measures,
    platform: Option<String>,
}

#[derive(Deserialize)]
pub struct ProjectsQuery {
    sort: Option<String>,
}

#[derive(Serialize, Copy, Clone)]
struct SortOption {
    key: &'static str,
    name: &'static str,
}

const SORT_OPTIONS: &[SortOption] = &[
    SortOption { key: "updated", name: "Last updated" },
    SortOption { key: "matched_code", name: "Matched Code" },
    SortOption { key: "name", name: "Name" },
];

pub async fn get_projects(
    State(state): State<AppState>,
    Query(query): Query<ProjectsQuery>,
) -> Result<Response, AppError> {
    let start = Instant::now();
    let projects = state.db.get_projects().await?;
    let mut out = projects
        .iter()
        .filter_map(|p| {
            let commit = p.commit.as_ref()?;
            Some(ProjectInfoContext {
                id: p.project.id,
                path: format!("/{}/{}", p.project.owner, p.project.repo),
                owner: p.project.owner.clone(),
                repo: p.project.repo.clone(),
                name: p.project.name().into_owned(),
                short_name: p.project.short_name().to_owned(),
                commit: commit.sha.clone(),
                timestamp: commit.timestamp,
                measures: Default::default(),
                platform: p.project.platform.clone(),
            })
        })
        .collect::<Vec<_>>();

    // Fetch latest report for each
    let sem = Arc::new(Semaphore::new(10));
    let mut join_set = JoinSet::new();
    for info in projects {
        let sem = sem.clone();
        let state = state.clone();
        join_set.spawn(async move {
            let _permit = sem.acquire().await;
            let Some(version) = info.default_version() else {
                return (info, Err(anyhow!("No report version found")));
            };
            let commit = info.commit.as_ref().unwrap();
            let report = state
                .db
                .get_report(&info.project.owner, &info.project.repo, &commit.sha, version)
                .await
                .with_context(|| {
                    format!(
                        "Failed to fetch report for {}/{} sha {} version {}",
                        info.project.owner, info.project.repo, commit.sha, version
                    )
                });
            (info, report)
        });
    }
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((info, Ok(Some(file)))) => {
                if let Some(c) = out.iter_mut().find(|i| i.id == info.project.id) {
                    c.measures = file.report.measures.unwrap_or_default();
                }
            }
            Ok((info, Ok(None))) => {
                tracing::warn!("No report found for {}", info.project.id);
            }
            Ok((info, Err(e))) => {
                tracing::error!("Failed to fetch report for {}: {:?}", info.project.id, e);
            }
            Err(e) => {
                tracing::error!("Failed to fetch report: {:?}", e);
            }
        }
    }

    let current_sort_key = query.sort.as_deref().unwrap_or("updated");
    let current_sort = SORT_OPTIONS
        .iter()
        .find(|s| s.key.eq_ignore_ascii_case(current_sort_key))
        .copied()
        .ok_or(AppError::Status(StatusCode::BAD_REQUEST))?;
    match current_sort.key {
        "name" => out.sort_by(|a, b| a.name.cmp(&b.name)),
        "updated" => out.sort_by(|a, b| b.timestamp.cmp(&a.timestamp)),
        "matched_code" => out.sort_by(|a, b| {
            b.measures
                .matched_code_percent
                .partial_cmp(&a.measures.matched_code_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        _ => return Err(AppError::Status(StatusCode::BAD_REQUEST)),
    }

    let mut rendered = render(&state.templates, "projects.html", ProjectsTemplateContext {
        projects: out,
        sort_options: SORT_OPTIONS,
        current_sort,
    })?;
    let elapsed = start.elapsed();
    rendered = rendered.replace("[[time]]", &format!("{}ms", elapsed.as_millis()));
    Ok(Html(rendered).into_response())
}
