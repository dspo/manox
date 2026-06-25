//! SVG rendering for stats views — Overview and Race.
//!
//! Generates static SVG snapshots matching the TUI layout structure.
//! Color mapping via [`color_to_hex`] converts ratatui `Color` → CSS hex.
//!
//! Overview SVG (1200×900): header bar → period switch → step chart →
//! model table → footer. Race SVG (1200×600): horizontal bar chart.

use anyhow::Result;
use std::collections::HashMap;

use ratatui::style::Color;

use super::aggregate::{top_models_covering, totals_by_agent_model, totals_by_model};
use super::date::{date_offset, days_diff, previous_month_days};
use super::format::{format_share, format_tokens, short_date};
use super::types::{Period, RaceWindow, UsageRecord, UsageTotals};
use super::{MATRIX_AGENTS, PALETTE, StatsView};

// ═══════════════════════════════════════════════════════════════════════════
// Public dispatch
// ═══════════════════════════════════════════════════════════════════════════

/// Render the specified stats view as an SVG string.
///
/// Dispatches to the appropriate view renderer based on `view`.
/// Returns the complete SVG document as a string.
pub(crate) fn render_to_string(
    records: &[UsageRecord],
    today: &str,
    period: Period,
    view: StatsView,
) -> Result<String> {
    match view {
        StatsView::Overview => render_overview_svg_to_string(records, today, period),
        StatsView::Race => render_race_svg_to_string(records, today, period),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Overview SVG rendering
// ═══════════════════════════════════════════════════════════════════════════

/// Overview canvas dimensions.
const OV_WIDTH: u32 = 1200;
const OV_HEIGHT: u32 = 900;

/// Layout Y regions (pixels).
const OV_HEADER_Y: u32 = 0;
const OV_HEADER_H: u32 = 44;
const OV_PERIOD_Y: u32 = 44;
const OV_PERIOD_H: u32 = 28;
const OV_CHART_Y: u32 = 72;
const OV_CHART_H: u32 = 358;
const OV_TABLE_Y: u32 = 430;
const OV_TABLE_H: u32 = 430;
const OV_FOOTER_Y: u32 = 860;
const OV_FOOTER_H: u32 = 40;

/// Chart inner layout (pixels).
const OV_CHART_TITLE_H: u32 = 24;
const OV_CHART_LABEL_W: u32 = 72;
const OV_CHART_BOTTOM_MARGIN: u32 = 26;
const OV_CHART_RIGHT_MARGIN: u32 = 16;
const OV_Y_TICK_COUNT: usize = 10;
const OV_X_TICK_MIN_COUNT: usize = 6;

/// Table layout (pixels).
const OV_TABLE_ROW_H: u32 = 24;
const OV_TABLE_HEADER_H: u32 = 48;
const OV_TABLE_LEFT_PAD: u32 = 12;
const OV_TABLE_COL_GAP: u32 = 12;
const OV_MODEL_COL_W: u32 = 200;
const OV_SHARE_COL_W: u32 = 58;
const OV_TOTAL_COL_W: u32 = 96;
const OV_AGENT_COL_W: u32 = 80;

/// Theme colors (light/white background for print-friendly output).
const OV_BG: &str = "#ffffff";
const OV_HEADER_BG: &str = "#f0f0f5";
const OV_PERIOD_BG: &str = "#f8f8fc";
const OV_BORDER: &str = "#c0c0c8";
const OV_AXIS: &str = "#808090";
const OV_DIM: &str = "#808090";
const OV_GRID: &str = "#e0e0e8";
const OV_TEXT: &str = "#1a1a2e";
const OV_ACTIVE_TAB_BG: &str = "#00b8b8";
const OV_ACTIVE_PERIOD: &str = "#1a1a2e";
const OV_INACTIVE_PERIOD: &str = "#a0a0a8";
const OV_STRIPED_BG: &str = "#f0f0f5";
const OV_FOOTER_BG: &str = "#f8f8fc";

/// Monospace font stack.
const OV_FONT: &str = "Menlo, 'Courier New', monospace";

/// Render the Overview view SVG as a string (for CLI --output pipeline).
fn render_overview_svg_to_string(
    records: &[UsageRecord],
    today: &str,
    period: Period,
) -> Result<String> {
    Ok(build_overview_svg(records, today, period))
}

/// Build the complete Overview SVG document.
fn build_overview_svg(records: &[UsageRecord], today: &str, period: Period) -> String {
    let filtered: Vec<&UsageRecord> = records
        .iter()
        .filter(|r| period.includes(&r.date, today))
        .collect();

    let mut s = String::with_capacity(32_768);

    // SVG header
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" \
         width=\"{OV_WIDTH}\" height=\"{OV_HEIGHT}\" \
         viewBox=\"0 0 {OV_WIDTH} {OV_HEIGHT}\">\n"
    ));
    s.push_str(&format!("<style>text{{font-family:{OV_FONT};}}</style>\n"));

    // Background
    s.push_str(&ov_rect(0, 0, OV_WIDTH, OV_HEIGHT, OV_BG, None));

    if filtered.is_empty() {
        ov_header(&mut s);
        ov_period_switch(&mut s, today, period);
        let cx = OV_WIDTH / 2;
        let cy = OV_HEIGHT / 2;
        s.push_str(&format!(
            "  <text x=\"{cx}\" y=\"{cy}\" font-size=\"18\" \
             fill=\"{OV_DIM}\" text-anchor=\"middle\">No data in selected period.</text>\n"
        ));
        ov_footer(&mut s, period);
        s.push_str("</svg>\n");
        return s;
    }

    // Components
    ov_header(&mut s);
    ov_period_switch(&mut s, today, period);
    ov_step_chart(&mut s, &filtered, today, period);
    ov_model_table(&mut s, &filtered, today, period);
    ov_footer(&mut s, period);

    s.push_str("</svg>\n");
    s
}

// ── Overview: Header ────────────────────────────────────────────────────

fn ov_header(s: &mut String) {
    // Background bar
    s.push_str(&ov_rect(
        0,
        OV_HEADER_Y,
        OV_WIDTH,
        OV_HEADER_H,
        OV_HEADER_BG,
        None,
    ));

    // Title text
    let title_y = OV_HEADER_Y + 28;
    s.push_str(&format!(
        "  <text x=\"16\" y=\"{title_y}\" font-size=\"16\" \
         font-weight=\"bold\" fill=\"{OV_TEXT}\">cx stats · Token Usage Dashboard</text>\n"
    ));

    // Tab indicators — Overview (active, cyan bg) and Race (inactive)
    let ov_tab_x = OV_WIDTH - 220;
    let race_tab_x = OV_WIDTH - 110;
    let tab_y = OV_HEADER_Y + 8;
    let tab_h: u32 = 28;
    let tab_text_y = OV_HEADER_Y + 28;

    // Active Overview tab
    s.push_str(&ov_rect(ov_tab_x, tab_y, 90, tab_h, OV_ACTIVE_TAB_BG, None));
    s.push_str(&format!(
        "  <text x=\"{}\" y=\"{tab_text_y}\" font-size=\"13\" \
         font-weight=\"bold\" fill=\"#000000\" text-anchor=\"middle\">Overview</text>\n",
        ov_tab_x + 45
    ));

    // Inactive Race tab
    s.push_str(&format!(
        "  <text x=\"{}\" y=\"{tab_text_y}\" font-size=\"13\" \
         font-weight=\"bold\" fill=\"{OV_INACTIVE_PERIOD}\" text-anchor=\"middle\">Race</text>\n",
        race_tab_x + 45
    ));

    // Bottom border
    let border_y = OV_HEADER_Y + OV_HEADER_H;
    s.push_str(&ov_line(0, border_y, OV_WIDTH, border_y, OV_BORDER, 1.0));
}

// ── Overview: Period switch ─────────────────────────────────────────────

fn ov_period_switch(s: &mut String, today: &str, period: Period) {
    // Background
    s.push_str(&ov_rect(
        0,
        OV_PERIOD_Y,
        OV_WIDTH,
        OV_PERIOD_H,
        OV_PERIOD_BG,
        None,
    ));

    let periods: [(Period, &str); 5] = [
        (Period::Today, "1"),
        (Period::Lastday, "2"),
        (Period::Last7, "3"),
        (Period::Last30, "4"),
        (Period::All, "5"),
    ];

    let text_y = OV_PERIOD_Y + 19;
    let underline_y = OV_PERIOD_Y + 22;
    let mut x: u32 = 24;

    for (i, (p, num)) in periods.iter().enumerate() {
        if i > 0 {
            s.push_str(&format!(
                "  <text x=\"{x}\" y=\"{text_y}\" font-size=\"14\" \
                 fill=\"{OV_DIM}\">·</text>\n"
            ));
            x += 16;
        }

        let is_active = *p == period;
        let label = p.label(today);
        let num_text = format!("[{num}] ");
        let (num_fill, label_fill) = if is_active {
            (OV_ACTIVE_PERIOD, OV_ACTIVE_PERIOD)
        } else {
            (OV_DIM, OV_INACTIVE_PERIOD)
        };
        let label_weight = if is_active { "bold" } else { "normal" };

        s.push_str(&format!(
            "  <text x=\"{x}\" y=\"{text_y}\" font-size=\"13\" \
             fill=\"{num_fill}\" font-weight=\"{label_weight}\">{esc_num}</text>\n",
            esc_num = xml_escape(&num_text),
        ));
        x += est_text_width(&num_text, 13);

        s.push_str(&format!(
            "  <text x=\"{x}\" y=\"{text_y}\" font-size=\"13\" \
             fill=\"{label_fill}\" font-weight=\"{label_weight}\">{esc_label}</text>\n",
            esc_label = xml_escape(&label),
        ));

        if is_active {
            let label_w = est_text_width(&label, 13);
            s.push_str(&ov_line(
                x,
                underline_y,
                x + label_w,
                underline_y,
                OV_ACTIVE_PERIOD,
                1.0,
            ));
            x += label_w + 20;
        } else {
            x += est_text_width(&label, 13) + 20;
        }
    }

    // Bottom border
    let border_y = OV_PERIOD_Y + OV_PERIOD_H;
    s.push_str(&ov_line(0, border_y, OV_WIDTH, border_y, OV_BORDER, 0.5));
}

// ── Overview: Step chart ────────────────────────────────────────────────

fn ov_step_chart(s: &mut String, filtered: &[&UsageRecord], today: &str, period: Period) {
    let totals = totals_by_model(filtered);
    let top_models = top_models_covering(&totals, 0.80);

    let Some((min_date, max_date)) = ov_chart_date_range(period, today, filtered) else {
        let text_y = OV_CHART_Y + 30;
        s.push_str(&format!(
            "  <text x=\"60\" y=\"{text_y}\" font-size=\"14\" \
             fill=\"{OV_DIM}\">No chart data for this period.</text>\n"
        ));
        return;
    };

    let day_count = (days_diff(&min_date, &max_date).unwrap_or(0).max(0) + 1) as usize;
    let day_count = day_count.max(1);

    // Compute per-model daily series
    let mut series_data: HashMap<String, Vec<f64>> = HashMap::new();
    for m in &top_models {
        series_data.insert(m.clone(), vec![0.0; day_count]);
    }
    for r in filtered {
        let idx = days_diff(&min_date, &r.date).unwrap_or(0).max(0) as usize;
        if idx >= day_count {
            continue;
        }
        let mut t = UsageTotals::default();
        t.add_record(r);
        let tokens = t.total_tokens() as f64;
        if let Some(v) = series_data.get_mut(&r.model) {
            v[idx] += tokens;
        }
    }

    let mut max_y: f64 = 1.0;
    let mut chart_series: Vec<(String, Vec<f64>, String)> = Vec::new();
    for (idx, model) in top_models.iter().enumerate() {
        let color = color_to_hex(&PALETTE[idx % PALETTE.len()]);
        let values = series_data.get(model).cloned().unwrap_or_default();
        for &y in &values {
            if y > max_y {
                max_y = y;
            }
        }
        chart_series.push((model.clone(), values, color));
    }

    // Chart title
    let title_y = OV_CHART_Y + 18;
    s.push_str(&format!(
        "  <text x=\"16\" y=\"{title_y}\" font-size=\"15\" \
         font-weight=\"bold\" fill=\"{OV_TEXT}\">Tokens per Day</text>\n"
    ));

    // Chart area coordinates
    let chart_top = OV_CHART_Y + OV_CHART_TITLE_H;
    let chart_bottom = OV_CHART_Y + OV_CHART_H - OV_CHART_BOTTOM_MARGIN;
    let chart_left = OV_CHART_LABEL_W + 8;
    let chart_right = OV_WIDTH - OV_CHART_RIGHT_MARGIN;

    // Reserve legend space
    let legend_w = ov_legend_width(&chart_series);
    let legend_gap: u32 = 20;
    let plot_right = chart_right.saturating_sub(legend_w + legend_gap);
    let plot_width = plot_right - chart_left;
    let plot_height = chart_bottom - chart_top;

    if plot_width < 60 || plot_height < 30 {
        return;
    }

    let max_bound = (max_y * 1.05).max(1.0);

    // Y-axis ticks and grid
    let y_ticks = ov_y_tick_values(max_y, OV_Y_TICK_COUNT);
    for value in &y_ticks {
        let label = format_tokens(value.round() as u64);
        let ratio = (*value / max_bound).clamp(0.0, 1.0);
        let y_px = chart_bottom as f64 - ratio * plot_height as f64;
        let y_px_u = y_px.round() as u32;
        // Y label (right-aligned at label column end)
        s.push_str(&format!(
            "  <text x=\"{OV_CHART_LABEL_W}\" y=\"{}\" font-size=\"11\" \
             fill=\"{OV_AXIS}\" text-anchor=\"end\">{esc_label}</text>\n",
            y_px_u + 4,
            esc_label = xml_escape(&label),
        ));
        // Horizontal grid line
        s.push_str(&ov_line(
            chart_left, y_px_u, plot_right, y_px_u, OV_GRID, 0.5,
        ));
    }

    // Y-axis vertical line
    s.push_str(&ov_line(
        chart_left,
        chart_top,
        chart_left,
        chart_bottom,
        OV_AXIS,
        1.0,
    ));

    // X-axis ticks
    let x_ticks = ov_x_tick_indices(day_count);
    for idx in x_ticks {
        let date_str = if idx + 1 == day_count {
            max_date.clone()
        } else {
            date_offset(&min_date, idx as i64).unwrap_or_default()
        };
        let label = short_date(&date_str);
        let x_px = chart_left as f64 + idx as f64 / day_count.max(1) as f64 * plot_width as f64;
        let x_px_u = x_px.round() as u32;
        let x_label_y = chart_bottom + 16;

        s.push_str(&format!(
            "  <text x=\"{x_px_u}\" y=\"{x_label_y}\" font-size=\"11\" \
             fill=\"{OV_AXIS}\" text-anchor=\"middle\">{esc_label}</text>\n",
            esc_label = xml_escape(&label),
        ));
        // Tick mark
        s.push_str(&ov_line(
            x_px_u,
            chart_bottom,
            x_px_u,
            chart_bottom + 5,
            OV_AXIS,
            0.5,
        ));
    }

    // Step series as polylines
    for (_model, values, color) in &chart_series {
        let points = ov_step_polyline_points(
            values,
            day_count,
            max_bound,
            chart_left,
            plot_right,
            chart_top,
            chart_bottom,
            plot_width,
            plot_height,
        );
        s.push_str(&format!(
            "  <polyline points=\"{pts}\" stroke=\"{color}\" \
             stroke-width=\"1.5\" fill=\"none\" stroke-linejoin=\"round\"/>\n",
            pts = points,
        ));
    }

    // Legend (right side of chart)
    let legend_x = plot_right + legend_gap;
    for (i, (model, _, color)) in chart_series.iter().enumerate() {
        let ly = chart_top + i as u32 * 18;
        // Color dot
        s.push_str(&ov_rect(legend_x, ly + 2, 10, 10, color, None));
        // Model name
        s.push_str(&format!(
            "  <text x=\"{}\" y=\"{}\" font-size=\"12\" fill=\"{color}\">{esc_model}</text>\n",
            legend_x + 14,
            ly + 12,
            esc_model = xml_escape(model),
        ));
    }
}

/// Compute step-chart polyline points for a series.
///
/// Produces a step function: value persists for a day, then transitions
/// vertically to the next value at the day boundary.
fn ov_step_polyline_points(
    values: &[f64],
    day_count: usize,
    max_bound: f64,
    chart_left: u32,
    _plot_right: u32,
    _chart_top: u32,
    chart_bottom: u32,
    plot_width: u32,
    plot_height: u32,
) -> String {
    if values.is_empty() || day_count == 0 {
        return String::new();
    }

    let mut pts = Vec::with_capacity(values.len() * 3 + 2);
    let mut prev_y_px: f64 = 0.0;

    for (idx, &value) in values.iter().enumerate() {
        let x_start = chart_left as f64 + idx as f64 / day_count as f64 * plot_width as f64;
        let x_end = chart_left as f64 + (idx + 1) as f64 / day_count as f64 * plot_width as f64;
        let ratio = (value / max_bound).clamp(0.0, 1.0);
        let y_px = chart_bottom as f64 - ratio * plot_height as f64;

        if idx == 0 {
            pts.push(format!("{:.1},{:.1}", x_start, y_px));
            pts.push(format!("{:.1},{:.1}", x_end, y_px));
            prev_y_px = y_px;
        } else {
            // Step: horizontal at prev value to day start, then vertical step
            pts.push(format!("{:.1},{:.1}", x_start, prev_y_px));
            pts.push(format!("{:.1},{:.1}", x_start, y_px));
            pts.push(format!("{:.1},{:.1}", x_end, y_px));
            prev_y_px = y_px;
        }
    }

    pts.join(" ")
}

// ── Overview: Model table ───────────────────────────────────────────────

fn ov_model_table(s: &mut String, filtered: &[&UsageRecord], _today: &str, _period: Period) {
    let cells = totals_by_agent_model(filtered);
    let totals = totals_by_model(filtered);

    let total_all: u64 = totals.values().map(|u| u.total_tokens()).sum();
    let mut total_usage = UsageTotals::default();
    for u in totals.values() {
        total_usage.add(u);
    }

    if totals.is_empty() {
        let text_y = OV_TABLE_Y + 30;
        s.push_str(&format!(
            "  <text x=\"{OV_TABLE_LEFT_PAD}\" y=\"{text_y}\" font-size=\"14\" \
             fill=\"{OV_DIM}\">No models to display.</text>\n"
        ));
        return;
    }

    // Sort models by total_tokens descending
    let mut sorted: Vec<(String, UsageTotals)> = totals.into_iter().collect();
    sorted.retain(|(_, u)| u.total_tokens() > 0);
    sorted.sort_by_key(|e| std::cmp::Reverse(e.1.total_tokens()));

    // Agent columns (sorted by usage, hide empty)
    let agent_columns = ov_sorted_agents_by_usage(&cells);

    // Max total for highlight ratio
    let max_total_tokens: u64 = sorted
        .iter()
        .map(|(_, u)| u.total_tokens())
        .max()
        .unwrap_or(0);

    // Table background
    s.push_str(&ov_rect(
        OV_TABLE_LEFT_PAD,
        OV_TABLE_Y,
        OV_WIDTH - 2 * OV_TABLE_LEFT_PAD,
        OV_TABLE_H,
        OV_BG,
        None,
    ));

    // Title
    let model_count = sorted.len();
    let title = format!("Models · {model_count}");
    let title_y = OV_TABLE_Y + 18;
    s.push_str(&format!(
        "  <text x=\"{OV_TABLE_LEFT_PAD}\" y=\"{title_y}\" font-size=\"14\" \
         font-weight=\"bold\" fill=\"{OV_TEXT}\">{esc_title}</text>\n",
        esc_title = xml_escape(&title),
    ));

    // Header row (two lines: label + total value)
    let hdr_label_y = OV_TABLE_Y + OV_TABLE_HEADER_H - 24;
    let hdr_value_y = OV_TABLE_Y + OV_TABLE_HEADER_H - 6;
    let hdr_sep_y = OV_TABLE_Y + OV_TABLE_HEADER_H;
    let mut col_x = OV_TABLE_LEFT_PAD;

    // Model column header
    s.push_str(&format!(
        "  <text x=\"{col_x}\" y=\"{hdr_label_y}\" font-size=\"12\" \
         font-weight=\"bold\" fill=\"{OV_TEXT}\">Model</text>\n"
    ));
    col_x += OV_MODEL_COL_W + OV_TABLE_COL_GAP;

    // Share column header
    s.push_str(&format!(
        "  <text x=\"{col_x}\" y=\"{hdr_label_y}\" font-size=\"12\" \
         font-weight=\"bold\" fill=\"{OV_TEXT}\">Share</text>\n"
    ));
    col_x += OV_SHARE_COL_W + OV_TABLE_COL_GAP;

    // Total column header (label + grand total)
    let total_text = ov_usage_cell_text(&total_usage);
    s.push_str(&format!(
        "  <text x=\"{col_x}\" y=\"{hdr_label_y}\" font-size=\"12\" \
         font-weight=\"bold\" fill=\"{OV_TEXT}\">Total</text>\n"
    ));
    s.push_str(&format!(
        "  <text x=\"{col_x}\" y=\"{hdr_value_y}\" font-size=\"11\" \
         font-weight=\"bold\" fill=\"{OV_DIM}\">{esc_total}</text>\n",
        esc_total = xml_escape(&total_text),
    ));
    col_x += OV_TOTAL_COL_W + OV_TABLE_COL_GAP;

    // Agent column headers
    for (agent, label) in &agent_columns {
        let agent_total = ov_compute_agent_total(&cells, agent);
        let agent_total_text = ov_usage_cell_text(&agent_total);
        s.push_str(&format!(
            "  <text x=\"{col_x}\" y=\"{hdr_label_y}\" font-size=\"12\" \
             font-weight=\"bold\" fill=\"{OV_ACTIVE_TAB_BG}\">{esc_label}</text>\n",
            esc_label = xml_escape(label),
        ));
        s.push_str(&format!(
            "  <text x=\"{col_x}\" y=\"{hdr_value_y}\" font-size=\"11\" \
             font-weight=\"bold\" fill=\"{OV_DIM}\">{esc_agent_total}</text>\n",
            esc_agent_total = xml_escape(&agent_total_text),
        ));
        col_x += OV_AGENT_COL_W + OV_TABLE_COL_GAP;
    }

    // Header separator line
    s.push_str(&ov_line(
        OV_TABLE_LEFT_PAD,
        hdr_sep_y,
        OV_WIDTH - OV_TABLE_LEFT_PAD,
        hdr_sep_y,
        OV_BORDER,
        1.0,
    ));

    // Data rows
    let row_start_y = OV_TABLE_Y + OV_TABLE_HEADER_H;
    let max_rows = (OV_TABLE_H - OV_TABLE_HEADER_H) / OV_TABLE_ROW_H;

    for (idx, (model, usage)) in sorted.iter().enumerate() {
        if idx as u32 >= max_rows {
            break;
        }

        let row_y = row_start_y + idx as u32 * OV_TABLE_ROW_H;
        let text_y = row_y + 16;
        let palette_idx = idx % PALETTE.len();
        let model_color = color_to_hex(&PALETTE[palette_idx]);

        // Striped row background (odd rows)
        if idx % 2 == 1 {
            let row_w = OV_WIDTH - 2 * OV_TABLE_LEFT_PAD;
            s.push_str(&ov_rect(
                OV_TABLE_LEFT_PAD,
                row_y,
                row_w,
                OV_TABLE_ROW_H,
                OV_STRIPED_BG,
                Some(0.30),
            ));
        }

        // Compute share percentage
        let pct = if total_all > 0 {
            usage.total_tokens() as f64 * 100.0 / total_all as f64
        } else {
            0.0
        };

        let mut col_x = OV_TABLE_LEFT_PAD;

        // Model name with color highlight background
        let ratio = if max_total_tokens > 0 {
            usage.total_tokens() as f64 / max_total_tokens as f64
        } else {
            0.0
        };
        let highlight_w = (OV_MODEL_COL_W as f64 * ratio).round() as u32;
        s.push_str(&ov_rect(
            col_x,
            row_y + 4,
            highlight_w.min(OV_MODEL_COL_W),
            OV_TABLE_ROW_H - 8,
            &model_color,
            Some(0.4),
        ));
        s.push_str(&format!(
            "  <text x=\"{}\" y=\"{text_y}\" font-size=\"13\" \
             font-weight=\"bold\" fill=\"#000000\">{esc_model}</text>\n",
            col_x + 4,
            esc_model = xml_escape(model),
        ));
        col_x += OV_MODEL_COL_W + OV_TABLE_COL_GAP;

        // Share
        s.push_str(&format!(
            "  <text x=\"{col_x}\" y=\"{text_y}\" font-size=\"12\" \
             fill=\"{OV_DIM}\">{esc_share}</text>\n",
            esc_share = xml_escape(&format_share(pct)),
        ));
        col_x += OV_SHARE_COL_W + OV_TABLE_COL_GAP;

        // Total
        let usage_text = ov_usage_cell_text(usage);
        s.push_str(&format!(
            "  <text x=\"{col_x}\" y=\"{text_y}\" font-size=\"12\" \
             fill=\"{OV_TEXT}\">{esc_usage}</text>\n",
            esc_usage = xml_escape(&usage_text),
        ));
        col_x += OV_TOTAL_COL_W + OV_TABLE_COL_GAP;

        // Agent columns
        for (agent, _) in &agent_columns {
            let cell_text = match cells.get(&(agent.to_string(), model.clone())) {
                Some(agent_usage) => ov_usage_cell_text(agent_usage),
                None => "—".to_string(),
            };
            let fill = if cell_text == "—" { OV_DIM } else { OV_TEXT };
            s.push_str(&format!(
                "  <text x=\"{col_x}\" y=\"{text_y}\" font-size=\"12\" \
                 fill=\"{fill}\">{esc_cell}</text>\n",
                esc_cell = xml_escape(&cell_text),
            ));
            col_x += OV_AGENT_COL_W + OV_TABLE_COL_GAP;
        }
    }
}

// ── Overview: Footer ────────────────────────────────────────────────────

fn ov_footer(s: &mut String, period: Period) {
    // Background
    s.push_str(&ov_rect(
        0,
        OV_FOOTER_Y,
        OV_WIDTH,
        OV_FOOTER_H,
        OV_FOOTER_BG,
        None,
    ));

    // Top border
    s.push_str(&ov_line(
        0,
        OV_FOOTER_Y,
        OV_WIDTH,
        OV_FOOTER_Y,
        OV_BORDER,
        0.5,
    ));

    let hint = match period {
        Period::Today => "[1] Today  2 Yda  3 7d  4 Mo  5 All",
        Period::Lastday => "1 Today  [2] Yda  3 7d  4 Mo  5 All",
        Period::Last7 => "1 Today  2 Yda  [3] 7d  4 Mo  5 All",
        Period::Last30 => "1 Today  2 Yda  3 7d  [4] Mo  5 All",
        Period::All => "1 Today  2 Yda  3 7d  4 Mo  [5] All",
    };
    let text = format!("{hint}   r cycle period   Tab switch view   Overview   q quit");
    let text_y = OV_FOOTER_Y + 24;
    s.push_str(&format!(
        "  <text x=\"16\" y=\"{text_y}\" font-size=\"11\" \
         fill=\"{OV_DIM}\">{esc_text}</text>\n",
        esc_text = xml_escape(&text),
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
// Overview helpers (replicating view.rs private logic)
// ═══════════════════════════════════════════════════════════════════════════

/// Date range for the chart, replicating view.rs `chart_date_range`.
fn ov_chart_date_range(
    period: Period,
    today: &str,
    records: &[&UsageRecord],
) -> Option<(String, String)> {
    match period {
        Period::Today => Some((today.to_string(), today.to_string())),
        Period::Lastday => {
            let yda = date_offset(today, -1).ok()?;
            Some((yda.clone(), yda))
        }
        Period::Last7 => {
            let start = date_offset(today, -6).ok()?;
            Some((start, today.to_string()))
        }
        Period::Last30 => {
            let days = previous_month_days(today) as i64;
            let start = date_offset(today, -(days - 1)).ok()?;
            Some((start, today.to_string()))
        }
        Period::All => {
            let first = records.first()?;
            let mut min_d = first.date.clone();
            let mut max_d = first.date.clone();
            for r in records.iter().skip(1) {
                if r.date < min_d {
                    min_d = r.date.clone();
                }
                if r.date > max_d {
                    max_d = r.date.clone();
                }
            }
            Some((min_d, max_d))
        }
    }
}

/// Y-axis tick values, replicating view.rs `y_tick_values`.
fn ov_y_tick_values(max_y: f64, tick_count: usize) -> Vec<f64> {
    if tick_count <= 1 {
        return vec![0.0];
    }
    let max_y = max_y.max(0.0);
    (0..tick_count)
        .map(|i| max_y * i as f64 / (tick_count - 1) as f64)
        .collect()
}

/// X-axis tick indices, replicating view.rs `x_tick_indices`.
fn ov_x_tick_indices(day_count: usize) -> Vec<usize> {
    match day_count {
        0 => Vec::new(),
        1..=7 => (0..day_count).collect(),
        _ => {
            let count = OV_X_TICK_MIN_COUNT.min(day_count);
            let last = day_count - 1;
            let mut indices = Vec::with_capacity(count);
            for i in 0..count {
                indices.push((i * last + (count - 1) / 2) / (count - 1));
            }
            indices.dedup();
            indices
        }
    }
}

/// Sorted agents by usage, replicating view.rs `sorted_agents_by_usage`.
fn ov_sorted_agents_by_usage(
    cells: &HashMap<(String, String), UsageTotals>,
) -> Vec<(&'static str, &'static str)> {
    let mut agents: Vec<(usize, &'static str, &'static str, u64)> = MATRIX_AGENTS
        .iter()
        .enumerate()
        .map(|(idx, (agent, label))| {
            let total: u64 = cells
                .iter()
                .filter(|((a, _), _)| a == agent)
                .map(|(_, u)| u.total_tokens())
                .sum();
            (idx, *agent, *label, total)
        })
        .collect();
    agents.retain(|(_, _, _, total)| *total > 0);
    agents.sort_by(|l, r| r.3.cmp(&l.3).then(l.0.cmp(&r.0)));
    agents.into_iter().map(|(_, a, l, _)| (a, l)).collect()
}

/// Usage cell text: ↑in ↓out (matching TUI format).
fn ov_usage_cell_text(usage: &UsageTotals) -> String {
    format!(
        "↑{} ↓{}",
        format_tokens(usage.in_tokens),
        format_tokens(usage.out_tokens)
    )
}

/// Compute an agent's total across all models.
fn ov_compute_agent_total(
    cells: &HashMap<(String, String), UsageTotals>,
    agent: &str,
) -> UsageTotals {
    let mut total = UsageTotals::default();
    for ((a, _), u) in cells {
        if a == agent {
            total.add(u);
        }
    }
    total
}

/// Estimate legend width in pixels.
fn ov_legend_width(series: &[(String, Vec<f64>, String)]) -> u32 {
    series
        .iter()
        .map(|(name, _, _)| est_text_width(name, 12) + 16)
        .max()
        .unwrap_or(80)
}

/// Estimate text width in pixels for monospace font.
/// Menlo/Courier at font-size N: ~0.6N per character.
fn est_text_width(text: &str, font_size: u32) -> u32 {
    (text.chars().count() as u32 * font_size * 6 / 10).max(text.len() as u32 * font_size / 2)
}

// ═══════════════════════════════════════════════════════════════════════════
// SVG element helpers
// ═══════════════════════════════════════════════════════════════════════════

fn ov_rect(x: u32, y: u32, w: u32, h: u32, fill: &str, opacity: Option<f64>) -> String {
    let op_str = match opacity {
        Some(op) => format!(" opacity=\"{op:.2}\""),
        None => String::new(),
    };
    format!("  <rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"{fill}\"{op_str}/>\n")
}

fn ov_line(x1: u32, y1: u32, x2: u32, y2: u32, stroke: &str, width: f64) -> String {
    format!(
        "  <line x1=\"{x1}\" y1=\"{y1}\" x2=\"{x2}\" y2=\"{y2}\" \
         stroke=\"{stroke}\" stroke-width=\"{width:.1}\"/>\n"
    )
}

// ═══════════════════════════════════════════════════════════════════════════
// Race SVG rendering
// ═══════════════════════════════════════════════════════════════════════════

const RACE_VISIBLE_MODELS: usize = 15;
const ROLLING_WINDOW_DAYS: i64 = 7;

/// Race canvas dimensions.
const RACE_WIDTH: u32 = 1200;
const RACE_HEIGHT: u32 = 600;

/// Race margins (px).
const RACE_MARGIN_TOP: u32 = 50;
const RACE_MARGIN_BOTTOM: u32 = 40;
const RACE_MARGIN_LEFT: u32 = 220;
const RACE_MARGIN_RIGHT: u32 = 80;
const RACE_BAR_HEIGHT: u32 = 28;
const RACE_BAR_GAP: u32 = 8;

const RACE_BG: &str = "#ffffff";
const RACE_TEXT: &str = "#1a1a2e";
const RACE_GRID: &str = "#c0c0c8";
const RACE_TITLE: &str = "#1a1a2e";

/// Render the Race view SVG as a string (for CLI --output pipeline).
///
/// Uses `Rolling7` window by default; AllTime period uses `PerDay`.
fn render_race_svg_to_string(
    records: &[UsageRecord],
    today: &str,
    period: Period,
) -> Result<String> {
    let window = match period {
        Period::All => RaceWindow::PerDay,
        _ => RaceWindow::Rolling7,
    };

    let filtered: Vec<&UsageRecord> = records
        .iter()
        .filter(|r| period.includes(&r.date, today))
        .collect();
    if filtered.is_empty() {
        return Ok(build_empty_race_svg_string(today, period, window));
    }

    let color_map = race_color_map(&filtered);
    let totals = race_snapshot_totals(&filtered, window, today);

    let mut entries: Vec<(String, u64, String)> = totals
        .iter()
        .filter(|(_, usage)| usage.total_tokens() > 0)
        .map(|(model, usage)| {
            let color_hex = color_to_hex(color_map.get(model).unwrap_or(&Color::White));
            (model.clone(), usage.total_tokens(), color_hex)
        })
        .collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    entries.truncate(RACE_VISIBLE_MODELS);

    if entries.is_empty() {
        return Ok(build_empty_race_svg_string(today, period, window));
    }

    let max_value = entries.iter().map(|(_, v, _)| *v).max().unwrap_or(1).max(1);
    let date_label = match window {
        RaceWindow::PerDay => format!("as of {}", short_date(today)),
        RaceWindow::Rolling7 => format!("rolling 7d ending {}", short_date(today)),
    };
    let title = format!("Model Tokens Top {} · {}", entries.len(), window.label());
    let subtitle = date_label;

    Ok(build_race_svg(&title, &subtitle, &entries, max_value))
}

/// Build an empty-state race SVG as a string.
fn build_empty_race_svg_string(today: &str, period: Period, window: RaceWindow) -> String {
    let title = format!("Model Tokens · {}", window.label());
    let subtitle = format!("{} · no data", period.label(today));
    let center_x = RACE_WIDTH / 2;
    let center_y = RACE_HEIGHT / 2;
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{RACE_WIDTH}\" height=\"{RACE_HEIGHT}\" viewBox=\"0 0 {RACE_WIDTH} {RACE_HEIGHT}\">\n\
          <rect width=\"{RACE_WIDTH}\" height=\"{RACE_HEIGHT}\" fill=\"{RACE_BG}\"/>\n\
          <text x=\"{center_x}\" y=\"{center_y}\" font-family=\"{OV_FONT}\" font-size=\"16\" fill=\"{RACE_GRID}\" text-anchor=\"middle\" dominant-baseline=\"middle\">{esc}</text>\n\
        </svg>\n",
        esc = xml_escape(&format!("{title} — {subtitle}")),
    )
}

// ── Race data aggregation ────────────────────────────────────────────────

/// Compute per-model cumulative totals for the current race snapshot.
fn race_snapshot_totals(
    filtered: &[&UsageRecord],
    window: RaceWindow,
    today: &str,
) -> HashMap<String, UsageTotals> {
    match window {
        RaceWindow::PerDay => totals_by_model(filtered),
        RaceWindow::Rolling7 => {
            let rolling: Vec<&UsageRecord> = filtered
                .iter()
                .copied()
                .filter(|r| {
                    days_diff(&r.date, today).is_some_and(|d| (0..ROLLING_WINDOW_DAYS).contains(&d))
                })
                .collect();
            totals_by_model(&rolling)
        }
    }
}

/// Assign PALETTE colors to models, ordered by total usage descending.
fn race_color_map(records: &[&UsageRecord]) -> HashMap<String, Color> {
    let totals = totals_by_model(records);
    let mut models: Vec<(String, u64)> = totals
        .into_iter()
        .map(|(model, usage)| (model, usage.total_tokens()))
        .collect();
    models.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    models
        .into_iter()
        .enumerate()
        .map(|(idx, (model, _))| (model, PALETTE[idx % PALETTE.len()]))
        .collect()
}

// ── Race SVG construction ────────────────────────────────────────────────

/// Build the complete SVG string for the race bar chart.
fn build_race_svg(
    title: &str,
    subtitle: &str,
    entries: &[(String, u64, String)],
    max_value: u64,
) -> String {
    let bar_count = entries.len();
    let chart_height = bar_count as u32 * (RACE_BAR_HEIGHT + RACE_BAR_GAP) - RACE_BAR_GAP;
    let actual_height = (RACE_MARGIN_TOP + chart_height + RACE_MARGIN_BOTTOM + 30).max(RACE_HEIGHT);

    let plot_left = RACE_MARGIN_LEFT;
    let plot_right = RACE_WIDTH - RACE_MARGIN_RIGHT;
    let plot_width = plot_right - plot_left;

    // X-axis ticks
    let x_ticks = nice_ticks(max_value, 5);
    let max_x = *x_ticks.last().unwrap_or(&max_value);

    let mut svg = String::with_capacity(8192);

    // SVG header
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{RACE_WIDTH}\" height=\"{actual_height}\" viewBox=\"0 0 {RACE_WIDTH} {actual_height}\">\n"
    ));

    // Background
    svg.push_str(&format!(
        "  <rect width=\"{RACE_WIDTH}\" height=\"{actual_height}\" fill=\"{RACE_BG}\"/>\n"
    ));

    // Title
    svg.push_str(&format!(
        "  <text x=\"{plot_left}\" y=\"28\" font-family=\"{OV_FONT}\" font-size=\"18\" font-weight=\"bold\" fill=\"{RACE_TITLE}\">{esc_title}</text>\n",
        esc_title = xml_escape(title),
    ));

    // Subtitle
    svg.push_str(&format!(
        "  <text x=\"{plot_left}\" y=\"46\" font-family=\"{OV_FONT}\" font-size=\"13\" fill=\"{RACE_GRID}\">{esc_sub}</text>\n",
        esc_sub = xml_escape(subtitle),
    ));

    // Grid lines (vertical)
    let grid_bottom = RACE_MARGIN_TOP + chart_height;
    let x_label_y = grid_bottom + 20;
    for tick_val in &x_ticks {
        let x_px =
            plot_left + (plot_width as f64 * (*tick_val as f64 / max_x as f64)).round() as u32;
        svg.push_str(&format!(
            "  <line x1=\"{x_px}\" y1=\"{RACE_MARGIN_TOP}\" x2=\"{x_px}\" y2=\"{grid_bottom}\" stroke=\"{RACE_GRID}\" stroke-width=\"1\" stroke-dasharray=\"4,4\"/>\n"
        ));
        svg.push_str(&format!(
            "  <text x=\"{x_px}\" y=\"{x_label_y}\" font-family=\"{OV_FONT}\" font-size=\"11\" fill=\"{RACE_GRID}\" text-anchor=\"middle\">{label}</text>\n",
            label = format_tokens(*tick_val),
        ));
    }

    // Bars
    for (i, (model, value, color)) in entries.iter().enumerate() {
        let y = RACE_MARGIN_TOP + i as u32 * (RACE_BAR_HEIGHT + RACE_BAR_GAP);
        let bar_width = if max_x > 0 {
            (plot_width as f64 * (*value as f64 / max_x as f64)).round() as u32
        } else {
            0
        };
        let text_y = y + RACE_BAR_HEIGHT / 2 + 5;
        let model_label_x = plot_left - 8;

        // Bar rectangle with rounded corners
        svg.push_str(&format!(
            "  <rect x=\"{plot_left}\" y=\"{y}\" width=\"{bar_width}\" height=\"{RACE_BAR_HEIGHT}\" rx=\"4\" ry=\"4\" fill=\"{color}\"/>\n"
        ));

        // Model name (left-aligned in left margin)
        svg.push_str(&format!(
            "  <text x=\"{model_label_x}\" y=\"{text_y}\" font-family=\"{OV_FONT}\" font-size=\"14\" fill=\"{RACE_TEXT}\" text-anchor=\"end\">{esc_model}</text>\n",
            esc_model = xml_escape(&truncate_model(model, 30)),
        ));

        // Value label
        let label_x = if bar_width > 50 {
            plot_left + bar_width - 8
        } else {
            plot_left + bar_width + 8
        };
        let label_anchor = if bar_width > 50 { "end" } else { "start" };
        let label_color = if bar_width > 50 { "#ffffff" } else { RACE_TEXT };
        svg.push_str(&format!(
            "  <text x=\"{label_x}\" y=\"{text_y}\" font-family=\"{OV_FONT}\" font-size=\"12\" font-weight=\"bold\" fill=\"{label_color}\" text-anchor=\"{label_anchor}\">{val}</text>\n",
            val = format_tokens(*value),
        ));
    }

    svg.push_str("</svg>\n");
    svg
}

// ═══════════════════════════════════════════════════════════════════════════
// Color mapping
// ═══════════════════════════════════════════════════════════════════════════

/// Map a ratatui `Color` to an SVG/CSS hex color string.
///
/// Covers all named colors used in the PALETTE and TUI rendering,
/// plus the `Rgb(r,g,b)` variant. Returns lowercase hex like `#e0c0d9`.
///
/// Named colors use standard CSS/X11 values where available, ensuring
/// all 8 PALETTE entries are visually distinct on a dark background.
pub(super) fn color_to_hex(color: &Color) -> String {
    match color {
        // PALETTE colors — must be distinct for chart series / model highlights
        Color::Cyan => "#00cdcd".to_string(),
        Color::LightYellow => "#ffffe0".to_string(),
        Color::LightGreen => "#90ee90".to_string(),
        Color::LightMagenta => "#ffbbff".to_string(),
        Color::LightRed => "#ffbbbb".to_string(),
        Color::LightBlue => "#add8e6".to_string(),
        Color::Yellow => "#cdcd00".to_string(),
        Color::Green => "#00cd00".to_string(),

        // TUI structural colors
        Color::White => "#ffffff".to_string(),
        Color::Black => "#000000".to_string(),
        Color::Gray => "#808080".to_string(),
        Color::DarkGray => "#555555".to_string(),
        Color::LightCyan => "#e0ffff".to_string(),

        // Other ANSI colors
        Color::Red => "#ff0000".to_string(),
        Color::Blue => "#0000ff".to_string(),
        Color::Magenta => "#ff00ff".to_string(),

        // Dynamic RGB
        Color::Rgb(r, g, b) => format!("#{:02x}{:02x}{:02x}", r, g, b),

        // Fallbacks
        Color::Reset => "#ffffff".to_string(),
        Color::Indexed(i) => indexed_to_hex(*i),
    }
}

/// Convert xterm-256 indexed color to hex.
fn indexed_to_hex(i: u8) -> String {
    // Standard 16 ANSI colors (0–15)
    const ANSI16: [&str; 16] = [
        "#000000", "#cd0000", "#00cd00", "#cdcd00", "#0000cd", "#cd00cd", "#00cdcd", "#e5e5e5",
        "#4d4d4d", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    if i < 16 {
        return ANSI16[i as usize].to_string();
    }
    // 6×6×6 color cube (16–231)
    if i < 232 {
        let b = (i - 16) % 6;
        let g = ((i - 16) / 6) % 6;
        let r = ((i - 16) / 36) % 6;
        let ch = |v: u8| match v {
            0 => 0,
            1 => 95,
            2 => 135,
            3 => 175,
            4 => 215,
            5 => 255,
            _ => 255,
        };
        return format!("#{:02x}{:02x}{:02x}", ch(r), ch(g), ch(b));
    }
    // Grayscale ramp (232–255)
    let v = 8 + 10 * (i - 232);
    format!("#{:02x}{:02x}{:02x}", v, v, v)
}

// ═══════════════════════════════════════════════════════════════════════════
// Shared helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Generate nice round tick values for the x-axis.
///
/// Returns 5-6 values from 0 up to a round ceiling of `max_value`.
fn nice_ticks(max_value: u64, target_count: usize) -> Vec<u64> {
    if max_value == 0 {
        return vec![0];
    }
    let raw_step = max_value as f64 / target_count as f64;
    let magnitude = 10_f64.powf(raw_step.log10().floor());
    let residual = raw_step / magnitude;
    let nice_step = match residual {
        r if r <= 1.5 => magnitude,
        r if r <= 3.0 => 2.0 * magnitude,
        r if r <= 7.0 => 5.0 * magnitude,
        _ => 10.0 * magnitude,
    } as u64;

    let mut ticks = Vec::new();
    let mut v = 0u64;
    while v < max_value {
        ticks.push(v);
        v += nice_step;
    }
    ticks.push(v);
    ticks
}

/// Truncate model name for display, appending "…" if truncated.
fn truncate_model(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        name.to_string()
    } else {
        let truncated = &name[..name.floor_char_boundary(max_len - 1)];
        format!("{truncated}…")
    }
}

/// Escape special XML characters in text content.
fn xml_escape(s: &str) -> String {
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

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── color_to_hex ──────────────────────────────────────────────────────

    #[test]
    fn color_to_hex_palette_distinct() {
        // All 8 PALETTE colors must produce distinct hex values
        let hexes: Vec<String> = PALETTE.iter().map(|c| color_to_hex(c)).collect();
        let unique: std::collections::HashSet<_> = hexes.iter().collect();
        assert_eq!(
            hexes.len(),
            unique.len(),
            "PALETTE hex values must be distinct"
        );
    }

    #[test]
    fn color_to_hex_palette_values() {
        assert_eq!(color_to_hex(&Color::Cyan), "#00cdcd");
        assert_eq!(color_to_hex(&Color::LightYellow), "#ffffe0");
        assert_eq!(color_to_hex(&Color::LightGreen), "#90ee90");
        assert_eq!(color_to_hex(&Color::LightMagenta), "#ffbbff");
        assert_eq!(color_to_hex(&Color::LightRed), "#ffbbbb");
        assert_eq!(color_to_hex(&Color::LightBlue), "#add8e6");
        assert_eq!(color_to_hex(&Color::Yellow), "#cdcd00");
        assert_eq!(color_to_hex(&Color::Green), "#00cd00");
    }

    #[test]
    fn color_to_hex_rgb() {
        assert_eq!(color_to_hex(&Color::Rgb(224, 192, 217)), "#e0c0d9");
        assert_eq!(color_to_hex(&Color::Rgb(0, 205, 253)), "#00cdfd");
        assert_eq!(color_to_hex(&Color::Rgb(250, 250, 0)), "#fafa00");
    }

    #[test]
    fn color_to_hex_misc() {
        assert_eq!(color_to_hex(&Color::White), "#ffffff");
        assert_eq!(color_to_hex(&Color::Black), "#000000");
        assert_eq!(color_to_hex(&Color::Gray), "#808080");
        assert_eq!(color_to_hex(&Color::DarkGray), "#555555");
        assert_eq!(color_to_hex(&Color::Reset), "#ffffff");
    }

    #[test]
    fn color_to_hex_striped_row_bg() {
        // The STRIPED_ROW_BG from view.rs: Color::Rgb(238, 242, 247)
        assert_eq!(color_to_hex(&Color::Rgb(238, 242, 247)), "#eef2f7");
    }

    // ── nice_ticks ────────────────────────────────────────────────────────

    #[test]
    fn nice_ticks_basic() {
        let ticks = nice_ticks(100, 5);
        assert_eq!(ticks, vec![0, 20, 40, 60, 80, 100]);
    }

    #[test]
    fn nice_ticks_large() {
        let ticks = nice_ticks(1_500_000, 5);
        // nice_ticks returns step-aligned values; just verify first and last
        assert_eq!(ticks[0], 0);
        assert!(ticks.last().copied().unwrap_or(0) >= 1_500_000);
    }

    #[test]
    fn nice_ticks_zero() {
        assert_eq!(nice_ticks(0, 5), vec![0]);
    }

    // ── xml_escape ────────────────────────────────────────────────────────

    #[test]
    fn xml_escape_special_chars() {
        assert_eq!(
            xml_escape("a&b<c>d'e\"f"),
            "a&amp;b&lt;c&gt;d&apos;e&quot;f"
        );
        assert_eq!(xml_escape("normal text"), "normal text");
    }

    // ── truncate_model ────────────────────────────────────────────────────

    #[test]
    fn truncate_model_short() {
        assert_eq!(truncate_model("claude-opus-4-7", 30), "claude-opus-4-7");
    }

    #[test]
    fn truncate_model_long() {
        let long = "very-long-model-name-that-exceeds-the-limit";
        let truncated = truncate_model(long, 20);
        assert!(truncated.len() <= 22);
        assert!(truncated.ends_with('…'));
    }

    // ── race_color_map ────────────────────────────────────────────────────

    #[test]
    fn race_color_map_ordering() {
        let records = vec![
            UsageRecord {
                agent: "a".to_string(),
                model: "model-b".to_string(),
                date: "2026-06-20".to_string(),
                in_tokens: 100,
                total_tokens: 200,
                out_tokens: 100,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            UsageRecord {
                agent: "a".to_string(),
                model: "model-a".to_string(),
                date: "2026-06-20".to_string(),
                in_tokens: 500,
                total_tokens: 1000,
                out_tokens: 500,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
        ];
        let refs: Vec<&UsageRecord> = records.iter().collect();
        let map = race_color_map(&refs);
        // model-a (highest usage) should get PALETTE[0] = Cyan
        assert_eq!(map.get("model-a"), Some(&PALETTE[0]));
        // model-b should get PALETTE[1] = LightYellow
        assert_eq!(map.get("model-b"), Some(&PALETTE[1]));
    }

    // ── Overview helpers ──────────────────────────────────────────────────

    #[test]
    fn ov_y_tick_values_basic() {
        let ticks = ov_y_tick_values(100.0, 5);
        assert_eq!(ticks, vec![0.0, 25.0, 50.0, 75.0, 100.0]);
    }

    #[test]
    fn ov_y_tick_values_zero() {
        assert_eq!(ov_y_tick_values(0.0, 5), vec![0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn ov_x_tick_indices_short_range() {
        // 1-7 days: every day
        assert_eq!(ov_x_tick_indices(5), vec![0, 1, 2, 3, 4]);
        assert_eq!(ov_x_tick_indices(1), vec![0]);
    }

    #[test]
    fn ov_x_tick_indices_long_range() {
        let indices = ov_x_tick_indices(30);
        assert!(!indices.is_empty());
        assert!(indices[0] == 0);
        assert!(*indices.last().unwrap() < 30);
    }

    #[test]
    fn test_ov_sorted_agents_by_usage() {
        let mut cells = HashMap::new();
        cells.insert(
            ("claude".to_string(), "model-a".to_string()),
            UsageTotals {
                in_tokens: 1000,
                total_tokens: 2000,
                out_tokens: 1000,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
        );
        cells.insert(
            ("codex".to_string(), "model-a".to_string()),
            UsageTotals {
                in_tokens: 50,
                total_tokens: 100,
                out_tokens: 50,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
        );
        let agents = ov_sorted_agents_by_usage(&cells);
        // Claude should come first (higher usage)
        assert_eq!(agents[0].0, "claude");
        assert_eq!(agents[1].0, "codex");
    }

    #[test]
    fn ov_usage_cell_text_format() {
        let usage = UsageTotals {
            in_tokens: 1234,
            total_tokens: 5678,
            out_tokens: 4444,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };
        let text = ov_usage_cell_text(&usage);
        assert!(text.starts_with("↑"));
        assert!(text.contains("↓"));
    }

    // ── Overview SVG smoke test ──────────────────────────────────────────

    #[test]
    fn build_overview_svg_empty_records() {
        let svg = build_overview_svg(&[], "2026-06-24", Period::Today);
        assert!(svg.contains("<svg"));
        assert!(svg.contains("No data"));
        assert!(svg.contains("</svg>"));
    }

    #[test]
    fn build_overview_svg_with_data() {
        let records = vec![
            UsageRecord {
                agent: "claude".to_string(),
                model: "claude-sonnet-4-5".to_string(),
                date: "2026-06-24".to_string(),
                in_tokens: 50000,
                total_tokens: 100000,
                out_tokens: 50000,
                cache_read_input_tokens: 10000,
                cache_creation_input_tokens: 5000,
            },
            UsageRecord {
                agent: "claude".to_string(),
                model: "claude-opus-4-7".to_string(),
                date: "2026-06-23".to_string(),
                in_tokens: 200000,
                total_tokens: 400000,
                out_tokens: 200000,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
        ];
        let svg = build_overview_svg(&records, "2026-06-24", Period::All);
        assert!(svg.contains("<svg"));
        assert!(svg.contains("cx stats"));
        assert!(svg.contains("Tokens per Day"));
        assert!(svg.contains("Models"));
        assert!(svg.contains("claude-sonnet-4-5"));
        assert!(svg.contains("claude-opus-4-7"));
        assert!(svg.contains("</svg>"));
        // Verify it has step chart polyline
        assert!(svg.contains("<polyline"));
        // Verify it has model table rows
        assert!(svg.contains("<text") && svg.contains("↑") && svg.contains("↓"));
    }

    #[test]
    fn build_overview_svg_period_today() {
        let records = vec![UsageRecord {
            agent: "claude".to_string(),
            model: "claude-sonnet-4".to_string(),
            date: "2026-06-24".to_string(),
            in_tokens: 100,
            total_tokens: 200,
            out_tokens: 100,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        }];
        let svg = build_overview_svg(&records, "2026-06-24", Period::Today);
        assert!(svg.contains("<svg"));
        assert!(svg.contains("[1]"));
        assert!(svg.contains("</svg>"));
    }

    // ── Step polyline ─────────────────────────────────────────────────────

    #[test]
    fn ov_step_polyline_basic() {
        let values = vec![10.0, 20.0, 30.0];
        let pts = ov_step_polyline_points(&values, 3, 40.0, 80, 400, 30, 200, 320, 170);
        assert!(!pts.is_empty());
        // Should contain comma-separated coordinate pairs
        assert!(pts.contains(','));
        // Should contain space-separated pairs (polyline format)
        assert!(pts.contains(' '));
    }

    #[test]
    fn ov_step_polyline_single_day() {
        let values = vec![5.0];
        let pts = ov_step_polyline_points(&values, 1, 10.0, 80, 400, 30, 200, 320, 170);
        assert!(!pts.is_empty());
    }

    #[test]
    fn ov_step_polyline_empty() {
        let pts = ov_step_polyline_points(&[], 0, 10.0, 80, 400, 30, 200, 320, 170);
        assert!(pts.is_empty());
    }
}
