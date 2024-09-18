use std::ffi::OsStr;

use anyhow::anyhow;
use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

use super::AppError;
use crate::util::join_normalized;

pub async fn get_css(Path(filename): Path<String>) -> Result<Response, AppError> {
    let mut path = join_normalized("css", &filename);
    if path.extension() != Some(OsStr::new("css")) {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    }
    path = path.with_extension("");
    let printer_options = lightningcss::stylesheet::PrinterOptions {
        minify: path.extension() == Some(OsStr::new("min")),
        ..Default::default()
    };
    path = path.with_extension("scss");
    let options = grass::Options::default().load_path("node_modules");
    let mut output = grass::from_path(&path, &options)?;
    // Skip lightningcss entirely if we're not minifying
    if printer_options.minify {
        let options = lightningcss::stylesheet::ParserOptions::default();
        let stylesheet = lightningcss::stylesheet::StyleSheet::parse(&output, options)
            .map_err(|e| anyhow!(e.to_string()))?;
        let result = stylesheet.to_css(printer_options)?;
        drop(stylesheet);
        output = result.code;
    }
    Ok((
        [
            (header::CONTENT_TYPE, mime::TEXT_CSS_UTF_8.as_ref()),
            #[cfg(not(debug_assertions))]
            (header::CACHE_CONTROL, "public, max-age=3600"),
            #[cfg(debug_assertions)]
            (header::CACHE_CONTROL, "no-cache"),
        ],
        output,
    )
        .into_response())
}
