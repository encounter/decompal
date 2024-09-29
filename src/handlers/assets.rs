use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

use super::AppError;
use crate::util::join_normalized;

pub async fn get_asset(Path(filename): Path<String>) -> Result<Response, AppError> {
    let path = join_normalized("assets", &filename);
    let Some(ext) = path.extension() else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let content_type = if let Some(format) = image::ImageFormat::from_extension(ext) {
        format.to_mime_type()
    } else {
        match ext.to_str() {
            Some("svg") => mime::IMAGE_SVG.as_ref(),
            _ => return Err(AppError::Status(StatusCode::NOT_FOUND)),
        }
    };
    let output = tokio::fs::read(path).await?;
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            #[cfg(not(debug_assertions))]
            (header::CACHE_CONTROL, "public, max-age=3600"),
            #[cfg(debug_assertions)]
            (header::CACHE_CONTROL, "no-cache"),
        ],
        output,
    )
        .into_response())
}
