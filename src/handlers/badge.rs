use anyhow::{anyhow, Result};
use image::ImageFormat;
use objdiff_core::bindings::report::{Measures, ReportCategory};
use serde::{Deserialize, Serialize};

use crate::{models::ReportFile, svg};

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ShieldParams {
    label: Option<String>,
    label_color: Option<String>,
    color: Option<String>,
    style: Option<String>,
    measure: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ShieldResponse {
    schema_version: u32,
    label: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    style: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label_color: Option<String>,
}

pub fn render(
    report: &ReportFile,
    measures: &Measures,
    current_category: Option<&ReportCategory>,
    params: &ShieldParams,
) -> Result<ShieldResponse> {
    let label = if let Some(label) = params.label.clone() {
        label
    } else if let Some(category) = current_category {
        category.name.clone()
    } else {
        report.project.short_name().to_string()
    };
    let message = if let Some(measure) = &params.measure {
        match measure.as_str() {
            "code" => format!("{:.2}%", measures.matched_code_percent),
            "data" => format!("{:.2}%", measures.matched_data_percent),
            "functions" => format!("{:.2}%", measures.matched_functions_percent),
            "complete_code" => format!("{:.2}%", measures.complete_code_percent),
            "complete_data" => format!("{:.2}%", measures.complete_data_percent),
            _ => return Err(anyhow!("Unknown measure")),
        }
    } else {
        format!("{:.2}%", measures.matched_code_percent)
    };
    Ok(ShieldResponse {
        schema_version: 1,
        label,
        message,
        color: Some(params.color.clone().unwrap_or_else(|| "informational".to_string())),
        style: params.style.clone(),
        label_color: params.label_color.clone(),
    })
}

pub fn render_svg(
    report: &ReportFile,
    measures: &Measures,
    current_category: Option<&ReportCategory>,
    params: &ShieldParams,
) -> Result<String> {
    let response = render(report, measures, current_category, params)?;
    let mut builder = badge_maker::BadgeBuilder::new();
    builder.label(&response.label).message(&response.message);
    if let Some(color) = &response.color {
        builder.color_parse(color);
    }
    if let Some(style) = &response.style {
        builder.style_parse(style);
    }
    if let Some(label_color) = &response.label_color {
        builder.label_color_parse(label_color);
    }
    let badge = builder.build()?;
    Ok(badge.svg())
}

pub fn render_image(
    report: &ReportFile,
    measures: &Measures,
    current_category: Option<&ReportCategory>,
    params: &ShieldParams,
    format: ImageFormat,
) -> Result<Vec<u8>> {
    let svg = render_svg(report, measures, current_category, params)?;
    svg::render_image(&svg, format)
}
