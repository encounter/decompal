use std::{sync::Arc, time::Instant};

use anyhow::anyhow;
use axum::{
    extract::State,
    response::{Html, IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use objdiff_core::bindings::report::Measures;
use serde::Serialize;
use tokio::{sync::Semaphore, task::JoinSet};

use super::AppError;
use crate::{templates::render, AppState};

#[derive(Serialize)]
struct ProjectsTemplateContext {
    projects: Vec<ProjectInfoContext>,
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
}

pub async fn get_projects(State(state): State<AppState>) -> Result<Response, AppError> {
    let start = Instant::now();
    let projects = state.db.get_projects().await?;
    let mut out = projects
        .iter()
        .map(|p| ProjectInfoContext {
            id: p.project.id,
            path: format!("/{}/{}", p.project.owner, p.project.repo),
            owner: p.project.owner.clone(),
            repo: p.project.repo.clone(),
            name: p.project.name().into_owned(),
            short_name: p.project.short_name().to_owned(),
            commit: p.commit.sha.clone(),
            timestamp: p.commit.timestamp,
            measures: Default::default(),
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
            let report = state
                .db
                .get_report(&info.project.owner, &info.project.repo, &info.commit.sha, version)
                .await;
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

    let mut rendered =
        render(&state.templates, "projects.html", ProjectsTemplateContext { projects: out })?;
    let elapsed = start.elapsed();
    rendered = rendered.replace("[[time]]", &format!("{}ms", elapsed.as_millis()));
    Ok(Html(rendered).into_response())
}
