use anyhow::Result;
use image::ImageFormat;
use objdiff_core::bindings::report::{Report, ReportUnit};
use palette::{Mix, Srgb};
use serde::Serialize;
use streemap::Rect;

use crate::{handlers::report::ReportTemplateUnit, svg, templates::render, AppState};

#[derive(Clone)]
pub struct ReportUnitItem<'report> {
    pub unit: &'report ReportUnit,
    pub size: f32,
    pub bounds: Rect<f32>,
}

impl<'report> ReportUnitItem<'report> {
    pub fn with_size(unit: &'report ReportUnit, size: f32) -> ReportUnitItem<'report> {
        ReportUnitItem { unit, size, bounds: Rect::from_size(0.0, 0.0) }
    }

    pub fn unit(&self) -> &'report ReportUnit { self.unit }
}

pub fn layout_units(
    report: &Report,
    w: u32,
    h: u32,
    mut predicate: impl FnMut(&ReportUnit) -> bool,
) -> Vec<ReportUnitItem> {
    let aspect = w as f32 / h as f32;
    let rect = if aspect > 1.0 {
        Rect::from_size(1.0, 1.0 / aspect)
    } else {
        Rect::from_size(aspect, 1.0)
    };
    let mut items = report
        .units
        .iter()
        .filter_map(|unit| {
            if !predicate(unit) {
                return None;
            }
            let total_code = unit.measures.as_ref().unwrap().total_code;
            if total_code == 0 {
                return None;
            }
            Some(ReportUnitItem::with_size(
                unit,
                total_code as f32 / report.measures.as_ref().unwrap().total_code as f32,
            ))
        })
        .collect::<Vec<_>>();
    streemap::ordered_pivot_by_middle(rect, &mut items, |i| i.size, |i, s| i.bounds = s);
    items
}

#[derive(Serialize)]
struct TreemapTemplateContext<'a> {
    units: &'a [ReportTemplateUnit<'a>],
    w: u32,
    h: u32,
}

pub fn render_svg(
    units: &[ReportTemplateUnit],
    w: u32,
    h: u32,
    state: &AppState,
) -> Result<String> {
    render(&state.templates, "treemap.svg", TreemapTemplateContext { units, w, h })
}

pub fn render_image(
    units: &[ReportTemplateUnit],
    w: u32,
    h: u32,
    state: &AppState,
    format: ImageFormat,
) -> Result<Vec<u8>> {
    let svg = render_svg(units, w, h, state)?;
    svg::render_image(&svg, format)
}

fn rgb(r: u8, g: u8, b: u8) -> Srgb {
    Srgb::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
}

pub fn unit_color(fuzzy_match_percent: f32) -> String {
    let red = rgb(42, 49, 64);
    let green = rgb(0, 200, 0);
    let (r, g, b) = red.mix(green, fuzzy_match_percent / 100.0).into_components();
    format!("#{:02x}{:02x}{:02x}", (r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}
