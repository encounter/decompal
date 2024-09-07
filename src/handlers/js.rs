use std::{ffi::OsStr, path::PathBuf};

use anyhow::{anyhow, Result};
use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use oxc::{
    allocator::Allocator,
    codegen::{CodeGenerator, CodegenReturn},
    minifier::{CompressOptions, Minifier, MinifierOptions},
    parser::Parser,
    semantic::SemanticBuilder,
    span::SourceType,
    transformer::{EnvOptions, Targets, TransformOptions, Transformer},
};

use crate::handlers::AppError;

pub async fn get_js(Path(filename): Path<String>) -> Result<Response, AppError> {
    let mut path = PathBuf::from(format!("js/{filename}"));
    #[derive(Debug, Copy, Clone, Eq, PartialEq)]
    enum ResponseType {
        Js,
        SourceMap,
    }
    let response_type;
    if path.extension() == Some(OsStr::new("js")) {
        response_type = ResponseType::Js;
    } else if path.extension() == Some(OsStr::new("map")) {
        path = path.with_extension("");
        if path.extension() != Some(OsStr::new("js")) {
            return Err(AppError::Status(StatusCode::NOT_FOUND));
        }
        response_type = ResponseType::SourceMap;
    } else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    }
    path = path.with_extension("");
    let minify = path.extension() == Some(OsStr::new("min"));
    path = path.with_extension("ts");
    let source_text =
        std::fs::read_to_string(&path).map_err(|_| AppError::Status(StatusCode::NOT_FOUND))?;
    let ret = transform(&path, &source_text, minify, response_type == ResponseType::SourceMap)?;
    let (data, content_type) = match response_type {
        ResponseType::Js => (
            format!("{}\n//# sourceMappingURL={}.map", ret.source_text, filename),
            mime::APPLICATION_JAVASCRIPT_UTF_8.as_ref(),
        ),
        ResponseType::SourceMap => {
            (ret.source_map.unwrap().to_json_string(), mime::APPLICATION_JSON.as_ref())
        }
    };
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            #[cfg(not(debug_assertions))]
            (header::CACHE_CONTROL, "public, max-age=3600"),
            #[cfg(debug_assertions)]
            (header::CACHE_CONTROL, "no-cache"),
        ],
        data,
    )
        .into_response())
}

fn transform(
    path: &std::path::Path,
    source_text: &str,
    minify: bool,
    source_map: bool,
) -> Result<CodegenReturn> {
    let source_type = SourceType::from_path(path)?;
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, &source_text, source_type).parse();
    let program = allocator.alloc(parsed.program);

    let (symbols, scopes) = SemanticBuilder::new(&source_text, source_type)
        .build(&program)
        .semantic
        .into_symbol_table_and_scope_tree();

    let transform_options = TransformOptions::from_preset_env(&EnvOptions {
        targets: Targets::from_query("es2020"),
        ..EnvOptions::default()
    })
    .map_err(|v| anyhow!("{}", v.first().unwrap()))?;

    let _ = Transformer::new(
        &allocator,
        path,
        source_type,
        &source_text,
        parsed.trivias.clone(),
        transform_options,
    )
    .build_with_symbols_and_scopes(symbols, scopes, program);

    let mangler = if minify {
        Minifier::new(MinifierOptions {
            mangle: minify,
            compress: CompressOptions {
                drop_debugger: false,
                drop_console: false,
                ..CompressOptions::all_true()
            },
        })
        .build(&allocator, program)
        .mangler
    } else {
        None
    };

    let mut codegen = CodeGenerator::new()
        .with_options(oxc::codegen::CodegenOptions { minify, ..Default::default() })
        .with_mangler(mangler);
    if source_map {
        let name = path.file_name().unwrap().to_string_lossy();
        codegen = codegen.enable_source_map(&name, source_text);
    }
    Ok(codegen.build(program))
}
