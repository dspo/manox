//! Clipboard image -> provider-ready `MessageContent::Image`.
//!
//! Decodes a `gpui::Image` from the clipboard, resizes the long edge down to
//! `ANTHROPIC_SIZE_LIMIT`, and re-encodes as PNG, looping with a shrinking
//! factor until the encoded size fits `DEFAULT_IMAGE_MAX_BYTES`. CPU-bound --
//! callers must run it on a background executor.

use std::io::Cursor;
use std::sync::Arc;

use base64::Engine as _;

use crate::language_model::MessageContent;

/// Anthropic's published long-edge cap (px). Larger images are downsampled
/// before encoding so the provider doesn't reject or re-encode them.
const ANTHROPIC_SIZE_LIMIT: u32 = 1568;
/// Encoded byte ceiling shipped to the provider.
const DEFAULT_IMAGE_MAX_BYTES: usize = 5 * 1024 * 1024;
/// Hard cap on resize passes so a pathological image can't loop forever.
const MAX_IMAGE_DOWNSCALE_PASSES: usize = 8;
/// Per-pass shrink factor once the encoded size still exceeds the cap.
const DOWNSCALE_FACTOR: f32 = 0.85;
/// Reject clipboard payloads above this size before decoding, capping the
/// transient memory peak of `image::load_from_memory`.
const MAX_INPUT_BYTES: usize = 50 * 1024 * 1024;

/// Map a clipboard image format onto an `image` decoder. SVG yields `None`
/// (vector, not raster-decodable); the caller drops the attachment.
fn decode_format(format: gpui::ImageFormat) -> Option<image::ImageFormat> {
    match format {
        gpui::ImageFormat::Png => Some(image::ImageFormat::Png),
        gpui::ImageFormat::Jpeg => Some(image::ImageFormat::Jpeg),
        gpui::ImageFormat::Webp => Some(image::ImageFormat::WebP),
        gpui::ImageFormat::Gif => Some(image::ImageFormat::Gif),
        gpui::ImageFormat::Bmp => Some(image::ImageFormat::Bmp),
        gpui::ImageFormat::Tiff => Some(image::ImageFormat::Tiff),
        gpui::ImageFormat::Svg | gpui::ImageFormat::Ico | gpui::ImageFormat::Pnm => None,
    }
}

/// Encode an RGBA buffer as PNG bytes.
fn encode_png(rgba: &image::ImageBuffer<image::Rgba<u8>, Vec<u8>>) -> Option<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba.clone())
        .write_to(&mut buf, image::ImageFormat::Png)
        .ok()?;
    Some(buf.into_inner())
}

/// Downsample so the long edge is at most `max`, preserving aspect ratio.
/// No-op when already within the limit.
fn resize_long_edge(
    rgba: image::ImageBuffer<image::Rgba<u8>, Vec<u8>>,
    max: u32,
) -> image::ImageBuffer<image::Rgba<u8>, Vec<u8>> {
    let (w, h) = (rgba.width(), rgba.height());
    let longest = w.max(h);
    if longest <= max {
        return rgba;
    }
    let scale = max as f32 / longest as f32;
    let new_w = ((w as f32 * scale).round() as u32).max(1);
    let new_h = ((h as f32 * scale).round() as u32).max(1);
    image::imageops::resize(&rgba, new_w, new_h, image::imageops::FilterType::Triangle)
}

/// Clipboard `gpui::Image` -> a provider-ready `MessageContent::Image` (base64
/// PNG, long edge <= `ANTHROPIC_SIZE_LIMIT`, encoded <=
/// `DEFAULT_IMAGE_MAX_BYTES`). Returns `None` when the format is undecodable
/// (e.g. SVG) or the image still exceeds the cap after
/// `MAX_IMAGE_DOWNSCALE_PASSES`.
pub fn gpui_image_to_message_content(image: Arc<gpui::Image>) -> Option<MessageContent> {
    if image.bytes.len() > MAX_INPUT_BYTES {
        return None;
    }
    let format = decode_format(image.format)?;
    let loaded = image::load_from_memory_with_format(&image.bytes, format).ok()?;
    let mut rgba = resize_long_edge(loaded.to_rgba8(), ANTHROPIC_SIZE_LIMIT);

    let mut bytes = encode_png(&rgba)?;
    let mut passes = 0;
    while bytes.len() > DEFAULT_IMAGE_MAX_BYTES && passes < MAX_IMAGE_DOWNSCALE_PASSES {
        let (w, h) = (rgba.width(), rgba.height());
        let new_w = ((w as f32 * DOWNSCALE_FACTOR).round() as u32).max(1);
        let new_h = ((h as f32 * DOWNSCALE_FACTOR).round() as u32).max(1);
        rgba = image::imageops::resize(&rgba, new_w, new_h, image::imageops::FilterType::Triangle);
        bytes = encode_png(&rgba)?;
        passes += 1;
    }

    if bytes.len() > DEFAULT_IMAGE_MAX_BYTES {
        return None;
    }

    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(MessageContent::Image {
        data,
        mime_type: "image/png".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_png(w: u32, h: u32) -> Arc<gpui::Image> {
        let img = image::ImageBuffer::from_pixel(w, h, image::Rgba([255, 0, 0, 255]));
        let mut buf = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            buf.into_inner(),
        ))
    }

    #[test]
    fn large_image_is_downscaled() {
        let img = solid_png(4000, 4000);
        let content = gpui_image_to_message_content(img).expect("large png converts");
        let (data, mime_type) = match content {
            MessageContent::Image { data, mime_type } => (data, mime_type),
            _ => unreachable!("expected image content"),
        };
        assert_eq!(mime_type, "image/png");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&data)
            .unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert!(decoded.width() <= ANTHROPIC_SIZE_LIMIT);
        assert!(decoded.height() <= ANTHROPIC_SIZE_LIMIT);
    }

    #[test]
    fn small_image_passes_through_as_png() {
        let img = solid_png(100, 50);
        let content = gpui_image_to_message_content(img).expect("small png converts");
        let (data, mime_type) = match content {
            MessageContent::Image { data, mime_type } => (data, mime_type),
            _ => unreachable!("expected image content"),
        };
        assert_eq!(mime_type, "image/png");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&data)
            .unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(decoded.width(), 100);
        assert_eq!(decoded.height(), 50);
    }

    #[test]
    fn svg_is_dropped() {
        let svg = b"<?xml version=\"1.0\"?><svg></svg>";
        let img = Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Svg,
            svg.to_vec(),
        ));
        assert!(gpui_image_to_message_content(img).is_none());
    }
}
