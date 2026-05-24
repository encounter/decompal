mod jobs;

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use apalis::{
    layers::retry::{
        HasherRng, RetryPolicy,
        backoff::{ExponentialBackoffMaker, MakeBackoff},
    },
    prelude::*,
};
use apalis_codec::json::JsonCodec;
use apalis_sqlite::{CompactType, SqliteStorage, fetcher::SqliteFetcher};
use decomp_dev_core::config::{Config, DbConfig, WorkerConfig};
use decomp_dev_db::Database;
use decomp_dev_github::GitHub;
pub use jobs::{
    ProcessWorkflowRunJob, RefreshProjectJob, process_refresh_project_job, process_workflow_run_job,
};
use sqlx::{Sqlite, migrate::MigrateDatabase, sqlite::SqlitePool};

/// Shared context available to all job handlers.
#[derive(Clone)]
pub struct JobContext {
    pub config: Arc<Config>,
    pub db: Arc<Database>,
    pub github: Arc<GitHub>,
}

/// Type alias for the default codec used by SqliteStorage.
type DefaultCodec = JsonCodec<CompactType>;

/// Type alias for workflow run storage.
pub type WorkflowRunStorage = SqliteStorage<ProcessWorkflowRunJob, DefaultCodec, SqliteFetcher>;

/// Type alias for refresh project storage.
pub type RefreshProjectStorage = SqliteStorage<RefreshProjectJob, DefaultCodec, SqliteFetcher>;

/// Storage handles for pushing jobs from request handlers.
#[derive(Clone)]
pub struct JobStorage {
    workflow_run: WorkflowRunStorage,
    refresh_project: RefreshProjectStorage,
}

impl JobStorage {
    /// Set up job storage tables and create storage instances.
    pub async fn setup(db: &DbConfig) -> Result<Arc<Self>> {
        if !Sqlite::database_exists(&db.jobs_url).await.unwrap_or(false) {
            tracing::info!(url = %db.jobs_url, "Creating database");
            Sqlite::create_database(&db.jobs_url).await.context("Failed to create database")?;
            tracing::info!("Database created");
        }
        let pool =
            SqlitePool::connect(&db.jobs_url).await.context("Failed to connect to database")?;
        SqliteStorage::setup(&pool).await?;
        Ok(Arc::new(Self {
            workflow_run: create_storage(&pool),
            refresh_project: create_storage(&pool),
        }))
    }

    /// Get a clone of the workflow run storage for pushing jobs.
    pub fn workflow_run(&self) -> WorkflowRunStorage { self.workflow_run.clone() }

    /// Get a clone of the refresh project storage for pushing jobs.
    pub fn refresh_project(&self) -> RefreshProjectStorage { self.refresh_project.clone() }
}

fn create_storage<T>(pool: &SqlitePool) -> SqliteStorage<T, DefaultCodec, SqliteFetcher> {
    let config = apalis_sqlite::Config::new(std::any::type_name::<T>()).with_poll_interval(
        StrategyBuilder::new()
            .apply(
                IntervalStrategy::new(Duration::from_millis(100))
                    .with_backoff(BackoffConfig::new(Duration::from_secs(1))),
            )
            .build(),
    );
    SqliteStorage::new_with_config(pool, &config)
}

/// Create the job monitor with all workers.
pub fn create_monitor(
    storage: Arc<JobStorage>,
    context: JobContext,
    config: &WorkerConfig,
) -> Monitor {
    let &WorkerConfig {
        workflow_run_concurrency,
        refresh_project_concurrency,
        retry_attempts,
        job_timeout_secs,
    } = config;
    let job_timeout = Duration::from_secs(job_timeout_secs);

    let backoff = ExponentialBackoffMaker::new(
        Duration::from_secs(1),
        Duration::from_secs(120),
        1.25,
        HasherRng::default(),
    )
    .unwrap()
    .make_backoff();
    let retry_policy = RetryPolicy::retries(retry_attempts)
        .with_backoff(backoff)
        .retry_if(|e: &BoxDynError| e.downcast_ref::<AbortError>().is_none());

    let storage1 = storage.clone();
    let storage2 = storage;
    let ctx1 = context.clone();
    let ctx2 = context;
    let retry1 = retry_policy.clone();
    let retry2 = retry_policy;

    Monitor::new()
        .register(move |_| {
            WorkerBuilder::new("workflow-run-worker")
                .backend(storage1.workflow_run.clone())
                .timeout(job_timeout)
                .retry(retry1.clone())
                .enable_tracing()
                .catch_panic()
                .on_event(|_c, e| {
                    if let Some(err) = e.as_error() {
                        tracing::error!("Error processing refresh project job: {err:?}");
                    }
                })
                .concurrency(workflow_run_concurrency)
                .parallelize(tokio::spawn)
                .data(ctx1.clone())
                .build(process_workflow_run_job)
        })
        .register(move |_| {
            WorkerBuilder::new("refresh-project-worker")
                .backend(storage2.refresh_project.clone())
                .timeout(job_timeout)
                .retry(retry2.clone())
                .enable_tracing()
                .catch_panic()
                .on_event(|_c, e| {
                    if let Some(err) = e.as_error() {
                        tracing::error!("Error processing refresh project job: {err:?}");
                    }
                })
                .concurrency(refresh_project_concurrency)
                .parallelize(tokio::spawn)
                .data(ctx2.clone())
                .build(process_refresh_project_job)
        })
        .shutdown_timeout(Duration::from_secs(30))
}
