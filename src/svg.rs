use std::{
    io::Cursor,
    sync::{Arc, OnceLock},
};

use anyhow::{anyhow, Context, Result};
use image::{
    buffer::ConvertBuffer, error::UnsupportedErrorKind, ExtendedColorType, ImageError, ImageFormat,
    RgbImage, RgbaImage,
};
use resvg::{
    tiny_skia::{PixmapMut, Transform},
    usvg::{fontdb, Options, Tree},
};
use tracing::info;

static FONT_DB: OnceLock<Arc<fontdb::Database>> = OnceLock::new();

pub fn render_image(svg: &str, format: ImageFormat) -> Result<Vec<u8>> {
    let fontdb = FONT_DB
        .get_or_init(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            info!("Loaded {} system fonts", db.len());
            Arc::new(db)
        })
        .clone();
    let opt = Options { fontdb, ..Default::default() };
    let tree = Tree::from_str(&svg, &opt).context("Failed to parse SVG")?;
    let rect = tree.size().to_int_size();
    let w = rect.width().clamp(1, 2048);
    let h = rect.height().clamp(1, 2048);
    let mut image = RgbaImage::new(w, h);
    let mut pixmap = PixmapMut::from_bytes(image.as_mut(), w, h)
        .ok_or_else(|| anyhow!("Failed to create pixmap"))?;
    resvg::render(&tree, Transform::identity(), &mut pixmap);
    let mut bytes = Vec::new();
    match image.write_to(&mut Cursor::new(&mut bytes), format) {
        Ok(()) => {}
        Err(ImageError::Unsupported(e))
            if matches!(e.kind(), UnsupportedErrorKind::Color(ExtendedColorType::Rgba8)) =>
        {
            // Convert to RGB and try again
            let image: RgbImage = image.convert();
            image.write_to(&mut Cursor::new(&mut bytes), format)?;
        }
        Err(e) => return Err(e.into()),
    }
    Ok(bytes)
}
