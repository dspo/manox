//! SVG → PNG/JPG rasterization via resvg + image crate.
//!
//! Entire module is feature-gated: only compiled when `image-output` is enabled.

#[cfg(feature = "image-output")]
use anyhow::{Context, Result};
use std::path::Path;

use super::OutputFormat;

/// Target raster format.
#[cfg(feature = "image-output")]
enum RasterKind {
    Png,
    Jpg,
}

/// Render SVG content to an image file (PNG or JPG).
///
/// - Initializes fontdb with system fonts + fallback families to avoid invisible text
/// - Renders at 2× scale for crisp output
/// - Writes the result to `output_path`
/// - Prints confirmation on success
#[cfg(feature = "image-output")]
pub(super) fn render_to_image(
    svg_content: &str,
    format: OutputFormat,
    output_path: &Path,
) -> Result<()> {
    let kind = match format {
        OutputFormat::Png => RasterKind::Png,
        OutputFormat::Jpg => RasterKind::Jpg,
        OutputFormat::Svg => {
            // SVG should have been written directly in the caller; reaching here is a bug.
            anyhow::bail!("render_to_image called with Svg format — write SVG directly instead");
        }
    };

    // --- Font database: critical for visible text in rendered output ---
    let mut fontdb = resvg::usvg::fontdb::Database::new();
    fontdb.load_system_fonts();
    fontdb.set_sans_serif_family("Helvetica Neue");
    fontdb.set_monospace_family("Menlo");

    // --- Parse SVG tree ---
    let usvg_opts = resvg::usvg::Options {
        fontdb: std::sync::Arc::new(fontdb),
        ..resvg::usvg::Options::default()
    };
    let tree = resvg::usvg::Tree::from_data(svg_content.as_bytes(), &usvg_opts)
        .context("Failed to parse SVG for rasterization")?;

    // --- Determine dimensions (2× for retina-quality) ---
    let base_w = tree.size().width() as u32;
    let base_h = tree.size().height() as u32;
    let scale = 2.0;
    let px_w = (base_w as f32 * scale).round() as u32;
    let px_h = (base_h as f32 * scale).round() as u32;

    // --- Render to pixmap ---
    let mut pixmap = resvg::tiny_skia::Pixmap::new(px_w, px_h)
        .context(format!("Failed to allocate pixmap ({px_w}×{px_h})"))?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // --- Encode & write ---
    match kind {
        RasterKind::Png => {
            let png_data = pixmap.encode_png().context("Failed to encode PNG")?;
            std::fs::write(output_path, png_data).context("Failed to write PNG file")?;
        }
        RasterKind::Jpg => {
            // Convert pixmap RGBA data → image crate DynamicImage, then encode JPEG
            let raw_rgba = pixmap.data().to_vec();
            let img =
                image::ImageBuffer::<image::Rgba<u8>, Vec<u8>>::from_raw(px_w, px_h, raw_rgba)
                    .context("Failed to construct image buffer from pixmap data")?;
            let dyn_img = image::DynamicImage::ImageRgba8(img);

            // JPEG doesn't support alpha; convert to RGB
            let rgb_img = dyn_img.to_rgb8();
            let mut buf = std::io::Cursor::new(Vec::new());
            image::codecs::jpeg::JpegEncoder::new(&mut buf)
                .encode(rgb_img.as_raw(), px_w, px_h, image::ExtendedColorType::Rgb8)
                .context("Failed to encode JPEG")?;
            std::fs::write(output_path, buf.into_inner()).context("Failed to write JPG file")?;
        }
    }

    println!("Written to {}", output_path.display());
    Ok(())
}
