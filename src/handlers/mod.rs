use std::{str::FromStr, sync::Arc};

use axum::{
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use bytes::BytesMut;
use mime::Mime;
use prost::Message;

use crate::AppState;

mod css;
mod graph;
mod project;
mod report;
mod js;

pub fn build_router() -> Router<AppState> {
    Router::new()
        .route("/css/*filename", get(css::get_css))
        .route("/js/*filename", get(js::get_js))
        .route("/", get(project::get_projects))
        .route("/:owner/:repo", get(report::get_report))
        .route("/:owner/:repo/:version", get(report::get_report))
        .route("/:owner/:repo/:version/:commit", get(report::get_report))
}

pub enum AppError {
    Status(StatusCode),
    Internal(anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            Self::Status(status) if status == StatusCode::NOT_FOUND => {
                (status, "Not found").into_response()
            }
            Self::Status(status) => status.into_response(),
            Self::Internal(err) => {
                tracing::error!("{:?}", err);
                (StatusCode::INTERNAL_SERVER_ERROR, format!("Something went wrong: {}", err))
                    .into_response()
            }
        }
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self { Self::Internal(err.into()) }
}

pub fn parse_accept(headers: &HeaderMap) -> Vec<Mime> {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .iter()
        .flat_map(|s| s.split(','))
        .map(|s| s.trim())
        .filter_map(|s| Mime::from_str(s).ok())
        .collect()
}

pub struct Protobuf<T: Message>(pub Arc<T>);

pub const APPLICATION_PROTOBUF: &str = "application/x-protobuf";
pub const PROTOBUF: &str = "x-protobuf";

impl<T: Message> IntoResponse for Protobuf<T> {
    fn into_response(self) -> Response {
        let mut bytes = BytesMut::with_capacity(self.0.encoded_len());
        self.0.encode(&mut bytes).unwrap();
        ([(header::CONTENT_TYPE, APPLICATION_PROTOBUF)], bytes.freeze()).into_response()
    }
}
