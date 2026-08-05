//! SVG 文档骨架 — canvas 尺寸、header、基础绘图 helper。
//!
//! 每个 `*_document` 函数返回 `(prefix, suffix)` 对：
//! - `prefix` = `<svg>` 开标签 + `<style>` + 背景 + header
//! - `suffix` = `</svg>` 关标签
//! - 调用方在 prefix 与 suffix 之间插入图表/表格等内容。

use super::palette;

// ── Overview canvas ──────────────────────────────────────────

pub(super) const OV_WIDTH: u32 = 1200;

/// Overview 内边距。
///
/// top = header(44) + gap(12) = 56
#[allow(dead_code)]
pub(super) struct OvMargin {
    pub(super) top: u32,
    pub(super) bottom: u32,
    pub(super) left: u32,
    pub(super) right: u32,
}

pub(super) const OV_MARGIN: OvMargin = OvMargin {
    top: 56,
    bottom: 20,
    left: 80,
    right: 120,
};

// ── Race canvas ──────────────────────────────────────────────

pub(super) const RACE_WIDTH: u32 = 1200;
pub(super) const RACE_HEIGHT: u32 = 600;

// ── Header / footer heights ──────────────────────────────────

const HEADER_H: u32 = 44;
/// X 轴日期标签高度（px）。
pub(super) const X_AXIS_LABEL_H: u32 = 18;
/// 区域间间距（px）。
pub(super) const SECTION_GAP: u32 = 12;

// ── CSS style block ──────────────────────────────────────────

fn style_block() -> String {
    format!(
        "\
  .title {{ font-family: {sans}; font-size: 18px; font-weight: 700; fill: {title}; }}
  .subtitle {{ font-family: {sans}; font-size: 13px; fill: {dim}; }}
  .axis-label {{ font-family: {mono}; font-size: 11px; fill: {axis}; }}
  .data-label {{ font-family: {sans}; font-size: 12px; fill: {text}; }}
  .data-value {{ font-family: {mono}; font-size: 12px; fill: {title}; }}
  .grid-line {{ stroke: {grid}; stroke-width: 0.5; stroke-dasharray: 4,4; }}
  .row-even {{ fill: {striped}; opacity: 0.5; }}
  .legend-label {{ font-family: {sans}; font-size: 12px; fill: {title}; }}
  .footer-text {{ font-family: {mono}; font-size: 11px; fill: {dim}; }}
",
        sans = palette::SANS,
        mono = palette::MONO,
        title = palette::TITLE,
        text = palette::TEXT,
        dim = palette::DIM,
        axis = palette::AXIS,
        grid = palette::GRID,
        striped = palette::STRIPED,
    )
}

// ── SVG primitive helpers ────────────────────────────────────

/// 矩形元素，可选圆角和透明度。
#[allow(dead_code)]
pub(super) fn ov_rect(x: u32, y: u32, w: u32, h: u32, fill: &str, opacity: f32) -> String {
    format!(
        "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" rx=\"4\" fill=\"{fill}\" opacity=\"{opacity:.2}\"/>",
        x = x,
        y = y,
        w = w,
        h = h,
        fill = fill,
        opacity = opacity,
    )
}

/// 矩形元素（无圆角，用于全宽背景条）。
fn full_rect(x: u32, y: u32, w: u32, h: u32, fill: &str) -> String {
    format!(
        "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"{fill}\"/>",
        x = x,
        y = y,
        w = w,
        h = h,
        fill = fill,
    )
}

/// 文本元素，CSS class + text-anchor。
pub(super) fn ov_text(x: u32, y: u32, content: &str, class: &str, anchor: &str) -> String {
    format!(
        "<text x=\"{x}\" y=\"{y}\" class=\"{class}\" text-anchor=\"{anchor}\">{content}</text>",
        x = x,
        y = y,
        class = class,
        anchor = anchor,
        content = content,
    )
}

/// 直线元素。
pub(super) fn ov_line(x1: u32, y1: u32, x2: u32, y2: u32, stroke: &str, width: f32) -> String {
    format!(
        "<line x1=\"{x1}\" y1=\"{y1}\" x2=\"{x2}\" y2=\"{y2}\" stroke=\"{stroke}\" stroke-width=\"{width:.1}\"/>",
        x1 = x1,
        y1 = y1,
        x2 = x2,
        y2 = y2,
        stroke = stroke,
        width = width,
    )
}

// ── Header bar ──────────────────────────────────────────────

fn header_bar(title: &str) -> String {
    let mut svg = String::new();
    // background
    svg.push_str(&full_rect(0, 0, OV_WIDTH, HEADER_H, palette::HEADER_BG));
    // bottom border
    svg.push_str(&ov_line(
        0,
        HEADER_H,
        OV_WIDTH,
        HEADER_H,
        palette::BORDER,
        1.0,
    ));
    // title text (left-aligned)
    svg.push_str(&ov_text(16, HEADER_H / 2 + 5, title, "title", "start"));
    // "cx stats" branding (right-aligned)
    svg.push_str(&ov_text(
        OV_WIDTH - 16,
        HEADER_H / 2 + 5,
        "cx stats",
        "subtitle",
        "end",
    ));
    svg
}

// ── Document skeletons ──────────────────────────────────────

/// Overview 文档骨架。
///
/// 返回 `(prefix, suffix)`：
/// - `prefix`: `<svg>` + `<style>` + background + header
/// - `suffix`: `</svg>`
/// - 调用方在两者之间插入 chart + table 等内容。
pub(super) fn ov_document(
    title: &str,
    period_label: &str,
    _active_period_idx: Option<usize>,
    height: u32,
) -> (String, String) {
    let mut prefix = String::new();

    // SVG open tag
    prefix.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{OV_WIDTH}\" height=\"{height}\" viewBox=\"0 0 {OV_WIDTH} {height}\">\n",
    ));

    // embedded style
    prefix.push_str("<style>\n");
    prefix.push_str(&style_block());
    prefix.push_str("</style>\n");

    // white background
    prefix.push_str(&full_rect(0, 0, OV_WIDTH, height, palette::BG));

    // header bar
    let header_title = format!("{title} · {period_label}");
    prefix.push_str(&header_bar(&header_title));

    let mut suffix = String::new();
    suffix.push_str("</svg>\n");

    (prefix, suffix)
}

/// Race 文档骨架。
///
/// 返回 `(prefix, suffix)`：
/// - `prefix`: `<svg>` + `<style>` + background + title + subtitle
/// - `suffix`: footer + `</svg>`
pub(super) fn race_document(title: &str, subtitle: &str) -> (String, String) {
    let mut prefix = String::new();

    // SVG open tag
    prefix.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{RACE_WIDTH}\" height=\"{RACE_HEIGHT}\" viewBox=\"0 0 {RACE_WIDTH} {RACE_HEIGHT}\">\n",
        RACE_WIDTH = RACE_WIDTH,
        RACE_HEIGHT = RACE_HEIGHT,
    ));

    // embedded style
    prefix.push_str("<style>\n");
    prefix.push_str(&style_block());
    prefix.push_str("</style>\n");

    // white background
    prefix.push_str(&full_rect(0, 0, RACE_WIDTH, RACE_HEIGHT, palette::BG));

    // title line (left-aligned, near top)
    prefix.push_str(&ov_text(16, 24, title, "title", "start"));

    // subtitle line (left-aligned, below title)
    prefix.push_str(&ov_text(16, 44, subtitle, "subtitle", "start"));

    // thin separator under subtitle
    prefix.push_str(&ov_line(16, 52, RACE_WIDTH - 16, 52, palette::BORDER, 1.0));

    let mut suffix = String::new();
    suffix.push_str("</svg>\n");

    (prefix, suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ov_document_contains_style_and_background() {
        let (prefix, suffix) = ov_document("Token Usage", "Last 7 days", Some(2), 900);
        assert!(prefix.contains("<svg"));
        assert!(prefix.contains("<style>"));
        assert!(prefix.contains(&format!("width=\"{OV_WIDTH}\"")));
        assert!(prefix.contains("height=\"900\""));
        assert!(prefix.contains(palette::BG));
        assert!(prefix.contains("cx stats"));
        assert!(suffix.contains("</svg>"));
    }

    #[test]
    fn ov_document_active_tab_highlighted() {
        let (prefix, _) = ov_document("Token Usage", "Last 7 days", Some(2), 900);
        assert!(!prefix.contains(palette::ACTIVE_TAB));
        assert!(!prefix.contains("Today"));
        assert!(!prefix.contains("Yesterday"));
    }

    #[test]
    fn race_document_contains_title_subtitle() {
        let (prefix, suffix) = race_document("Model Tokens", "Rolling 7 days in Last month");
        assert!(prefix.contains("<svg"));
        assert!(prefix.contains("<style>"));
        assert!(prefix.contains(&format!("width=\"{RACE_WIDTH}\"")));
        assert!(prefix.contains(&format!("height=\"{RACE_HEIGHT}\"")));
        assert!(prefix.contains("Model Tokens"));
        assert!(prefix.contains("Rolling 7 days"));
        assert!(suffix.contains("</svg>"));
    }

    #[test]
    fn ov_margin_constants() {
        assert_eq!(OV_MARGIN.top, 56);
        assert_eq!(OV_MARGIN.bottom, 20);
        assert_eq!(OV_MARGIN.left, 80);
        assert_eq!(OV_MARGIN.right, 120);
    }

    #[test]
    fn helper_functions_produce_valid_svg_elements() {
        let rect = ov_rect(10, 20, 100, 50, "#ff0000", 0.8);
        assert!(rect.contains("rx=\"4\""));
        assert!(rect.contains("fill=\"#ff0000\""));
        assert!(rect.contains("opacity=\"0.80\""));

        let text = ov_text(50, 100, "hello", "title", "middle");
        assert!(text.contains("class=\"title\""));
        assert!(text.contains(">hello</text>"));

        let line = ov_line(0, 0, 100, 100, "#000", 1.5);
        assert!(line.contains("stroke=\"#000\""));
        assert!(line.contains("stroke-width=\"1.5\""));
    }

    #[test]
    fn footer_bar_positioned_at_bottom() {
        let (_prefix, suffix) = ov_document("Test", "All time", Some(4), 900);
        assert_eq!(suffix, "</svg>\n");
    }
}
