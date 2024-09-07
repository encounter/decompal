use std::{borrow::Cow, sync::Arc};

use chrono::{DateTime, Utc};
use objdiff_core::bindings::report::Report;
use serde::Serialize;

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct Project {
    pub id: u64,
    pub owner: String,
    pub repo: String,
    pub name: Option<String>,
    pub short_name: Option<String>,
    pub default_version: Option<String>,
}

impl Project {
    pub fn name(&self) -> Cow<str> {
        if let Some(name) = self.name.as_ref() {
            Cow::Borrowed(name)
        } else {
            Cow::Owned(format!("{}/{}", self.owner, self.repo))
        }
    }

    pub fn short_name(&self) -> &str {
        self.short_name.as_deref().or(self.name.as_deref()).unwrap_or(&self.repo)
    }

    pub fn repo_url(&self) -> String { format!("https://github.com/{}/{}", self.owner, self.repo) }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct ProjectInfo {
    pub project: Project,
    pub commit: Commit,
    pub report_versions: Vec<String>,
    pub prev_commit: Option<String>,
    pub next_commit: Option<String>,
}

impl ProjectInfo {
    pub fn default_version(&self) -> Option<&str> {
        self.project
            .default_version
            .as_ref()
            // Verify that the default version is in the list of report versions
            .and_then(|v| self.report_versions.contains(v).then_some(v.as_str()))
            // Otherwise, return the first version in the list
            .or_else(|| self.report_versions.first().map(String::as_str))
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct Commit {
    pub sha: String,
    pub timestamp: DateTime<Utc>,
}

impl From<&octocrab::models::repos::Commit> for Commit {
    fn from(commit: &octocrab::models::repos::Commit) -> Self {
        Self {
            sha: commit.sha.as_ref().unwrap().clone(),
            timestamp: commit.author.as_ref().unwrap().date.unwrap(),
        }
    }
}

impl From<&octocrab::models::workflows::HeadCommit> for Commit {
    fn from(commit: &octocrab::models::workflows::HeadCommit) -> Self {
        Self { sha: commit.id.clone(), timestamp: commit.timestamp }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReportFile {
    pub project: Project,
    pub commit: Commit,
    pub version: String,
    pub report: Arc<Report>,
}
