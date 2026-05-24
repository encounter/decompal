use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub server: ServerConfig,
    pub db: DbConfig,
    pub github: GitHubConfig,
    pub openai: Option<OpenAiConfig>,
    #[serde(default)]
    pub worker: WorkerConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub port: u16,
    pub jobs_port: Option<u16>,
    #[serde(default)]
    pub dev_mode: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DbConfig {
    pub url: String,
    pub jobs_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitHubConfig {
    pub token: String,
    pub app: Option<GitHubAppConfig>,
    pub oauth: Option<GitHubOAuthConfig>,
    #[serde(default)]
    pub super_admin_ids: Vec<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitHubAppConfig {
    pub id: u64,
    pub webhook_secret: String,
    pub private_key: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitHubOAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenAiConfig {
    pub api_key: String,
}

/// Configuration for job workers.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkerConfig {
    /// Maximum concurrent workflow run jobs.
    pub workflow_run_concurrency: usize,
    /// Maximum concurrent refresh project jobs.
    pub refresh_project_concurrency: usize,
    /// Number of retry attempts for failed jobs.
    pub retry_attempts: usize,
    /// Maximum wall-clock seconds allowed for a single job attempt.
    #[serde(default = "default_job_timeout_secs")]
    pub job_timeout_secs: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            workflow_run_concurrency: 3,
            refresh_project_concurrency: 3,
            retry_attempts: 5,
            job_timeout_secs: default_job_timeout_secs(),
        }
    }
}

fn default_job_timeout_secs() -> u64 { 15 * 60 }
