//! Race 视图 — 模型 token 用量水平条形图 SVG 渲染。
//!
//! 独立于 TUI，直接输出 SVG 文档。
//! 条形图按 total tokens 降序排列，最多显示前 `MAX_VISIBLE` 个模型。

use super::aggregate;
use super::format::{format_share, format_tokens};
use super::layout::{RACE_HEIGHT, RACE_WIDTH, ov_text, race_document};
use super::palette::SVG_COLORS;
use super::types::{Period, UsageRecord};

// ── Layout constants ────────────────────────────────────────

/// 条形高度。
pub(super) const BAR_HEIGHT: u32 = 28;

/// 条形间距。
pub(super) const BAR_GAP: u32 = 8;

/// 模型名称区右边界 / 条形区左边界。
pub(super) const BAR_LEFT_PAD: u32 = 170;

/// 条形区右侧留白（value 外部标签 + share 百分比）。
pub(super) const BAR_RIGHT_PAD: u32 = 140;

/// 条形图区域起始 y（标题+副标题+分隔线之后）。
pub(super) const TOP_OFFSET: u32 = 60;

/// 最大显示条目数。
pub(super) const MAX_VISIBLE: usize = 15;

/// Value 标签放在 bar 内部的最小 bar 宽度阈值（px）。
pub(super) const INSIDE_LABEL_MIN_W: u32 = 80;

// ── Main renderer ────────────────────────────────────────────

/// 渲染 Race 视图为完整 SVG 文档。
///
/// 接收原始 UsageRecord，内部过滤+聚合+排序，最多显示前 `MAX_VISIBLE` 个模型。
/// 条形宽度 = (tokens / max_tokens) × available_width，渐变填充左→右。
/// Value 标签自适应位置：宽条内部白色，窄条外部深色。
pub(super) fn race_chart(records: &[UsageRecord], today: &str, period: Period) -> String {
    // ── 过滤 + 聚合 ──
    let filtered: Vec<&UsageRecord> = records
        .iter()
        .filter(|r| period.includes(&r.date, today))
        .collect();
    let totals = aggregate::totals_by_model(&filtered);
    let mut entries: Vec<(String, u64)> = totals
        .iter()
        .map(|(m, u)| (m.clone(), u.total_tokens()))
        .filter(|(_, t)| *t > 0)
        .collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    if entries.is_empty() {
        return empty_race(today, period);
    }

    let max_value = entries.iter().map(|(_, v)| *v).max().unwrap_or(1);
    let visible: Vec<(String, u64, String)> = entries
        .iter()
        .take(MAX_VISIBLE)
        .enumerate()
        .map(|(i, (m, v))| (m.clone(), *v, SVG_COLORS[i % SVG_COLORS.len()].to_string()))
        .collect();

    let total_all: u64 = entries.iter().map(|(_, v)| *v).sum();
    let period_label = period.label(today);

    // ── 文档骨架 ──
    let (prefix, suffix) = race_document(
        &format!("Model Tokens · {}", period_label),
        &format!("{} · Top {} models", period_label, visible.len()),
    );

    let mut body = String::new();
    let available_width = RACE_WIDTH - BAR_LEFT_PAD - BAR_RIGHT_PAD;

    // ── 渐变定义（集中在一个 <defs> 块） ──
    body.push_str(&gradient_defs(&visible));

    // ── 网格线 + 刻度标签 ──
    let ticks = nice_ticks(max_value, 5);
    let bars_bottom = TOP_OFFSET + visible.len() as u32 * (BAR_HEIGHT + BAR_GAP);
    body.push_str(&render_grid(
        &ticks,
        max_value,
        available_width,
        bars_bottom,
    ));

    // ── 条形 + 标签 ──
    for (i, (model, value, _color)) in visible.iter().enumerate() {
        let y = TOP_OFFSET + i as u32 * (BAR_HEIGHT + BAR_GAP);
        body.push_str(&render_bar_row(
            i,
            model,
            *value,
            y,
            max_value,
            available_width,
            total_all,
        ));
    }

    format!("{prefix}{body}{suffix}")
}

// ── Empty state ──────────────────────────────────────────────

/// 无数据时渲染居中提示。
pub(super) fn empty_race(today: &str, period: Period) -> String {
    let period_label = period.label(today);
    let (prefix, suffix) = race_document(&format!("Model Tokens · {}", period_label), "no data");
    let msg = "暂无数据";
    let centered = ov_text(RACE_WIDTH / 2, RACE_HEIGHT / 2, msg, "subtitle", "middle");
    format!("{prefix}{centered}{suffix}")
}

// ── Gradient defs ────────────────────────────────────────────

/// 为所有可见 bar 集中生成 linearGradient 定义。
///
/// 渐变方向：左→右（x1=0 → x2=1），起始色全透明度，终止色 0.7 透明度。
fn gradient_defs(visible: &[(String, u64, String)]) -> String {
    let mut svg = String::from("<defs>\n");
    for (i, (_, _, color)) in visible.iter().enumerate() {
        svg.push_str(&format!(
            "  <linearGradient id=\"bar-grad-{}\" x1=\"0\" y1=\"0\" x2=\"1\" y2=\"0\">\n",
            i,
        ));
        svg.push_str(&format!(
            "    <stop offset=\"0%\" stop-color=\"{}\" stop-opacity=\"1\"/>\n",
            color,
        ));
        svg.push_str(&format!(
            "    <stop offset=\"100%\" stop-color=\"{}\" stop-opacity=\"0.7\"/>\n",
            color,
        ));
        svg.push_str("  </linearGradient>\n");
    }
    svg.push_str("</defs>\n");
    svg
}

// ── Grid lines ───────────────────────────────────────────────

/// 渲染垂直虚线网格 + 刻度值标签。
///
/// 使用 CSS `.grid-line` class（含 stroke-dasharray: 4,4），
/// 不再使用 ov_line（会画实线）。
fn render_grid(ticks: &[u64], max_value: u64, available_width: u32, bars_bottom: u32) -> String {
    if max_value == 0 {
        return String::new();
    }
    let mut svg = String::new();
    for &tick in ticks {
        if tick == 0 {
            continue;
        }
        let x_px =
            BAR_LEFT_PAD + (tick as f64 / max_value as f64 * available_width as f64).round() as u32;
        // 虚线网格线（CSS class 提供 stroke-dasharray）
        svg.push_str(&format!(
            "<line x1=\"{x}\" y1=\"{TOP_OFFSET}\" x2=\"{x}\" y2=\"{b}\" class=\"grid-line\"/>\n",
            x = x_px,
            b = bars_bottom,
        ));
        // 刻度值标签
        svg.push_str(&ov_text(
            x_px,
            bars_bottom + 14,
            &format_tokens(tick),
            "axis-label",
            "middle",
        ));
    }
    svg
}

// ── Bar row ──────────────────────────────────────────────────

/// 渲染单行：模型名称 + 渐变条形 + value 标签 + share 百分比。
fn render_bar_row(
    idx: usize,
    model: &str,
    value: u64,
    y: u32,
    max_value: u64,
    available_width: u32,
    total_all: u64,
) -> String {
    let mut svg = String::new();
    let text_y = y + BAR_HEIGHT / 2 + 4; // ≈ baseline shift

    // ── 模型名称（左对齐，CSS class legend-label） ──
    let display_name = truncate_model(model, 22);
    svg.push_str(&ov_text(
        8,
        text_y,
        &xml_escape(&display_name),
        "legend-label",
        "start",
    ));

    // ── 条形 ──
    let bar_width = if max_value > 0 {
        (value as f64 / max_value as f64 * available_width as f64).round() as u32
    } else {
        0
    };
    // 微量值保留 2px 最小宽度
    let effective_w = std::cmp::max(bar_width, if value > 0 { 2 } else { 0 });

    svg.push_str(&format!(
        "<rect x=\"{BAR_LEFT_PAD}\" y=\"{y}\" width=\"{w}\" height=\"{BAR_HEIGHT}\" rx=\"4\" fill=\"url(#bar-grad-{idx})\"/>\n",
        w = effective_w,
    ));

    // ── Value 标签：宽条内部白色 / 窄条外部深色 ──
    let val_text = format_tokens(value);
    if effective_w >= INSIDE_LABEL_MIN_W {
        // CSS .data-value 的 fill 会被 inline style 覆盖（优先级：inline > CSS > 属性）
        let text_x = BAR_LEFT_PAD + effective_w - 8;
        svg.push_str(&format!(
            "<text x=\"{text_x}\" y=\"{text_y}\" class=\"data-value\" text-anchor=\"end\" style=\"fill:#ffffff\" font-weight=\"600\">{val_text}</text>\n",
        ));
    } else {
        let text_x = BAR_LEFT_PAD + effective_w + 8;
        svg.push_str(&ov_text(text_x, text_y, &val_text, "data-value", "start"));
    }

    // ── Share 百分比（右对齐） ──
    let pct = if total_all > 0 {
        value as f64 / total_all as f64 * 100.0
    } else {
        0.0
    };
    svg.push_str(&ov_text(
        RACE_WIDTH - 8,
        text_y,
        &format_share(pct),
        "axis-label",
        "end",
    ));

    svg
}

// ── Helpers ──────────────────────────────────────────────────

/// 截断过长模型名称，保留前 (max_len−1) 个字符 + `…`。
pub(super) fn truncate_model(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        name.to_string()
    } else {
        let truncated = name.chars().take(max_len - 1).collect::<String>();
        format!("{truncated}…")
    }
}

/// XML 特殊字符转义。
pub(super) fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Nice round tick 值，用于网格线刻度。
///
/// 目标 ~5 条刻度，步长取 1/2/5/10 × 10^n。
pub(super) fn nice_ticks(max_value: u64, target_count: usize) -> Vec<u64> {
    if max_value == 0 {
        return vec![0];
    }
    let raw_step = max_value as f64 / target_count as f64;
    let mag = 10_f64.powf(raw_step.log10().floor());
    let res = raw_step / mag;
    let nice_step: u64 = if res <= 1.5 {
        mag as u64
    } else if res <= 3.0 {
        (2.0 * mag) as u64
    } else if res <= 7.0 {
        (5.0 * mag) as u64
    } else {
        (10.0 * mag) as u64
    };

    let nice_max = ((max_value as f64 / nice_step as f64).ceil() as u64) * nice_step;
    let mut ticks = Vec::new();
    let mut v: u64 = 0;
    while v <= nice_max {
        ticks.push(v);
        v += nice_step;
    }
    ticks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_ticks_basic() {
        let ticks = nice_ticks(100, 5);
        assert_eq!(ticks[0], 0);
        assert!(ticks.len() >= 4 && ticks.len() <= 8);
        let step = ticks[1];
        for &t in &ticks {
            assert_eq!(t % step, 0);
        }
    }

    #[test]
    fn nice_ticks_zero() {
        assert_eq!(nice_ticks(0, 5), vec![0]);
    }

    #[test]
    fn nice_ticks_large() {
        let ticks = nice_ticks(234_000_000, 5);
        assert_eq!(ticks[0], 0);
        assert!(*ticks.last().unwrap() >= 234_000_000);
        let step = ticks[1];
        for &t in &ticks {
            assert_eq!(t % step, 0);
        }
    }

    #[test]
    fn nice_ticks_small() {
        let ticks = nice_ticks(5000, 5);
        assert_eq!(ticks[0], 0);
        assert!(*ticks.last().unwrap() >= 5000);
    }

    #[test]
    fn truncate_model_short() {
        assert_eq!(truncate_model("glm-5", 22), "glm-5");
    }

    #[test]
    fn truncate_model_long() {
        let long = "very-long-model-name-that-exceeds-the-limit";
        let truncated = truncate_model(long, 20);
        assert!(truncated.len() <= 22);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn truncate_model_exact() {
        assert_eq!(
            truncate_model("1234567890123456789012", 22),
            "1234567890123456789012"
        );
    }

    #[test]
    fn xml_escape_special_chars() {
        assert_eq!(
            xml_escape("a&b<c>d'e\"f"),
            "a&amp;b&lt;c&gt;d&apos;e&quot;f"
        );
    }

    #[test]
    fn xml_escape_no_special() {
        assert_eq!(xml_escape("glm-5.2"), "glm-5.2");
    }
}
