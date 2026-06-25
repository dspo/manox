//! PNG/JPG image output — renders SVG content to raster formats.
//!
//! SVG output is just a file write (no resvg/image needed).
//! PNG/JPG: resvg renders SVG → pixel buffer → image crate encodes.
//!
//! This entire module is feature-gated behind `image-output`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::OutputFormat;

/// Render SVG content to an image file in the specified format.
///
/// - SVG → writes the SVG string directly to `output_path`.
/// - PNG → resvg renders SVG to pixels, then `image` crate encodes PNG.
/// - JPG → resvg renders SVG to pixels, then `image` crate encodes JPEG.
pub fn render_to_image(svg_content: &str, format: OutputFormat, output_path: &Path) -> Result<()> {
    match format {
        OutputFormat::Svg => write_svg(svg_content, output_path),
        OutputFormat::Png => render_raster(svg_content, output_path, RasterKind::Png),
        OutputFormat::Jpg => render_raster(svg_content, output_path, RasterKind::Jpg),
    }
}

/// Write SVG content directly to a file.
fn write_svg(svg_content: &str, output_path: &Path) -> Result<()> {
    fs::write(output_path, svg_content)
        .with_context(|| format!("写入 SVG 文件失败: {}", output_path.display()))?;
    Ok(())
}

/// Raster format variant for the image crate encoding step.
enum RasterKind {
    Png,
    Jpg,
}

/// Render SVG content to a raster image (PNG or JPG) via resvg + image crate.
fn render_raster(svg_content: &str, output_path: &Path, kind: RasterKind) -> Result<()> {
    // 1. Build font database — load system fonts so text can render
    let mut fontdb = resvg::usvg::fontdb::Database::new();
    fontdb.load_system_fonts();
    fontdb.set_monospace_family("Courier New");

    // 2. Parse SVG with the font database
    let opt = resvg::usvg::Options {
        fontdb: std::sync::Arc::new(fontdb),
        ..resvg::usvg::Options::default()
    };
    let tree = resvg::usvg::Tree::from_data(svg_content.as_bytes(), &opt)
        .with_context(|| "解析 SVG 内容失败")?;

    // 3. Render to pixel buffer at 2x scale for quality
    let pixmap_size = tree.size();
    let scale = 2.0;
    let width = (pixmap_size.width() * scale) as u32;
    let height = (pixmap_size.height() * scale) as u32;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .with_context(|| format!("创建像素缓冲区失败 ({width}×{height})"))?;

    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // 3. Convert to image crate's DynamicImage via ImageBuffer
    let rgba_image = image::ImageBuffer::<image::Rgba<u8>, Vec<u8>>::from_raw(
        width,
        height,
        pixmap.data().to_vec(),
    )
    .with_context(|| "从 resvg 像素数据创建 ImageBuffer 失败")?;
    let dynamic = image::DynamicImage::ImageRgba8(rgba_image);

    // 4. Encode and write
    let buf = match kind {
        RasterKind::Png => {
            let mut cursor = std::io::Cursor::new(Vec::new());
            dynamic
                .write_with_encoder(image::codecs::png::PngEncoder::new_with_quality(
                    &mut cursor,
                    image::codecs::png::CompressionType::Default,
                    image::codecs::png::FilterType::Sub,
                ))
                .with_context(|| "PNG 编码失败")?;
            cursor.into_inner()
        }
        RasterKind::Jpg => {
            let mut cursor = std::io::Cursor::new(Vec::new());
            // JPEG doesn't support RGBA — convert to RGB first
            let rgb = dynamic.to_rgb8();
            rgb.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut cursor,
                95,
            ))
            .with_context(|| "JPEG 编码失败")?;
            cursor.into_inner()
        }
    };

    fs::write(output_path, buf)
        .with_context(|| format!("写入图片文件失败: {}", output_path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn write_svg_to_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.svg");
        write_svg("<svg>hello</svg>", &path).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "<svg>hello</svg>");
    }

    #[test]
    fn render_to_image_svg_format() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.svg");
        render_to_image("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"><rect width=\"10\" height=\"10\" fill=\"red\"/></svg>", OutputFormat::Svg, &path).unwrap();
        assert!(fs::read_to_string(&path).unwrap().starts_with("<svg"));
    }
}
