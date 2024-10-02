mod config;
mod cron;
mod db;
mod github;
mod handlers;
mod models;
mod svg;
mod templates;
mod util;

use std::{
    fs::File,
    io::BufReader,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use axum::{http::header, Router};
use tokio::{net::TcpListener, signal};
use tower::ServiceBuilder;
use tower_http::{
    timeout::TimeoutLayer,
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
    ServiceBuilderExt,
};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};

use crate::{
    config::Config, db::Database, github::GitHub, handlers::build_router, templates::Templates,
};

#[derive(Clone)]
struct AppState {
    config: Config,
    db: Database,
    github: GitHub,
    templates: Templates,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                // Default to info level
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let config: Config = {
        let file = BufReader::new(File::open("config.yml").expect("Failed to open config file"));
        serde_yaml::from_reader(file).expect("Failed to parse config file")
    };
    let db = Database::new(&config.app).await.expect("Failed to open database");
    let github = GitHub::new(&config.app).await.expect("Failed to create GitHub client");
    let templates = templates::create("templates");
    let state = AppState { config, db: db.clone(), github, templates };

    // Refresh before starting the server
    // cron::refresh_projects(&mut state).await.expect("Failed to refresh projects");

    // Start the task scheduler
    let mut scheduler = cron::create(state.clone()).await.expect("Failed to create scheduler");

    // Run our service
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, state.config.server.port));
    tracing::info!("Listening on {}", addr);
    axum::serve(
        TcpListener::bind(addr).await.expect("bind error"),
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .expect("server error");

    scheduler.shutdown().await.expect("Failed to shut down scheduler");
    db.close().await;
    tracing::info!("Shut down gracefully");
}

fn app(state: AppState) -> Router {
    let sensitive_headers: Arc<[_]> = vec![header::AUTHORIZATION, header::COOKIE].into();
    let middleware = ServiceBuilder::new()
        .sensitive_request_headers(sensitive_headers.clone())
        .sensitive_response_headers(sensitive_headers)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(tracing::Level::INFO))
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
        )
        .layer(TimeoutLayer::new(Duration::from_secs(10)))
        .compression();
    build_router().layer(middleware).with_state(state)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
