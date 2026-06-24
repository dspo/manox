//! TUI 渲染：header / footer / Models。

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::collections::{HashMap, HashSet};

use super::aggregate::{top_models_covering, totals_by_agent_model, totals_by_model};
use super::date::{date_offset, days_diff};
use super::format::{format_tokens, short_date};
use super::tui::{ChartTab, StatsApp};
use super::types::{Period, RaceInterval, RaceWindow, UsageRecord, UsageTotals};
use super::{MATRIX_AGENTS, PALETTE};

type ChartSeries = (String, Vec<f64>, Color);
type ChartOccupancy = HashSet<(u16, u16)>;
/// `(day_idx, 该日累计 (agent, model) -> 用量)`：用于 race 动画的插值快照。
type RaceSnapshot = (usize, HashMap<(String, String), UsageTotals>);

const MODEL_MIN_WIDTH: u16 = 26;
const SHARE_WIDTH: u16 = 6;
const TABLE_COLUMN_SPACING: u16 = 1;
const STRIPED_ROW_BG: Color = Color::Rgb(238, 242, 247);
const STEP_CHART_MAX_WIDTH: u16 = 78;
const STEP_CHART_HEIGHT: u16 = 17;
const Y_TICK_COUNT: usize = 10;
const X_TICK_MIN_COUNT: usize = 6;
const RACE_VISIBLE_MODELS: usize = 15;
const RACE_TWEEN_STEPS: usize = 12;
const RACE_FINAL_HOLD_TICKS: usize = RACE_TWEEN_STEPS * 3;
const RACE_FINAL_DISSOLVE_TICKS: usize = RACE_TWEEN_STEPS * 2;
const RACE_INITIAL_COALESCE_TICKS: usize = RACE_TWEEN_STEPS * 2;
const RACE_TRANSITION_SEED: u32 = 0x1234_5678;


#[derive(Debug, Clone)]
struct RaceEntry {
    model: String,
    value: u64,
    usage: UsageTotals,
    color: Color,
}

#[derive(Debug, Clone)]
struct RaceFrame {
    date: String,
    entries: Vec<RaceEntry>,
    cells: HashMap<(String, String), UsageTotals>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum RacePhase {
    Playing {
        previous_idx: usize,
        current_idx: usize,
        tween: f64,
    },
    HoldingLast {
        idx: usize,
    },
    DissolvingLast {
        idx: usize,
        progress: f64,
    },
    CoalescingFirst {
        idx: usize,
        progress: f64,
    },
}

pub(super) fn draw(f: &mut ratatui::Frame, app: &mut StatsApp) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, chunks[0], app);
    draw_models_view(f, chunks[1], app);
    draw_footer(f, chunks[2], app);
}

fn draw_header(f: &mut ratatui::Frame, area: Rect, app: &StatsApp) {
    let mut spans = vec![
        Span::styled(
            " cx stats ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("· Token Usage Dashboard   "),
    ];
    for tab in [ChartTab::Overview, ChartTab::Race] {
        let style = if app.chart_tab == tab {
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightCyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(format!(" {} ", tab.label()), style));
        spans.push(Span::raw("  "));
    }
    let title = Line::from(spans);

    let block = Block::default().borders(Borders::BOTTOM);
    let p = Paragraph::new(title).block(block);
    f.render_widget(p, area);
}

fn draw_footer(f: &mut ratatui::Frame, area: Rect, app: &StatsApp) {
    let period_hint = match app.period {
        Period::Today => "[1] Today  2 Yda  3 7d  4 Mo  5 All",
        Period::Lastday => "1 Today  [2] Yda  3 7d  4 Mo  5 All",
        Period::Last7 => "1 Today  2 Yda  [3] 7d  4 Mo  5 All",
        Period::Last30 => "1 Today  2 Yda  3 7d  [4] Mo  5 All",
        Period::All => "1 Today  2 Yda  3 7d  4 Mo  [5] All",
    };
    let view_hint = match app.chart_tab {
        ChartTab::Overview => "Overview".to_string(),
        ChartTab::Race => {
            let interval_label = app.race_interval.label(&app.today);
            let window_label = app.race_window.label();
            format!("Race · {interval_label} · {window_label}")
        }
    };
    let keys_hint = match app.chart_tab {
        ChartTab::Overview => "r cycle period",
        ChartTab::Race => "r cycle interval   d cycle window",
    };
    let text = format!(
        "{period_hint}   {keys_hint}   Tab switch view   ↑↓/j/k scroll   {view_hint}   q quit"
    );
    let p = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(p, area);
}

fn draw_models_view(f: &mut ratatui::Frame, area: Rect, app: &mut StatsApp) {
    match app.chart_tab {
        ChartTab::Overview => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Length(STEP_CHART_HEIGHT),
                    Constraint::Min(0),
                ])
                .split(area);
            draw_period_switch(f, chunks[0], app);
            draw_tokens_per_day_chart(f, chunks[1], app);
            draw_overview_model_list(f, chunks[2], app);
        }
        ChartTab::Race => {
            let filtered_records: Vec<UsageRecord> = if matches!(
                app.race_interval,
                RaceInterval::AllTime
            ) {
                app.records.clone()
            } else {
                app.records
                    .iter()
                    .filter(|r| app.race_interval.includes(&r.date, &app.today))
                    .cloned()
                    .collect()
            };
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(STEP_CHART_HEIGHT), Constraint::Min(0)])
                .split(area);
            let frames = race_frames(&filtered_records, app.race_window);
            let interval_label = app.race_interval.label(&app.today);
            let window_label = app.race_window.label();
            let title_suffix = format!("{interval_label} · {window_label}");
            draw_bar_chart_race(f, chunks[0], app, &frames, &title_suffix);
            draw_dynamic_model_list(f, chunks[1], app, &frames, app.race_window);
            apply_race_transition(
                f.buffer_mut(),
                area,
                race_phase(app.race_tick, frames.len()),
            );
        }
    }
}

fn draw_tokens_per_day_chart(f: &mut ratatui::Frame, area: Rect, app: &StatsApp) {
    let records = app.period_records();
    if records.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "  No data in selected period.",
            Style::default().fg(Color::DarkGray),
        )))
        .block(Block::default().title(" Tokens per Day "));
        f.render_widget(p, area);
        return;
    }

    let totals = totals_by_model(&records);
    let top_models: Vec<String> = top_models_covering(&totals, 0.80);

    let Some((min_date, max_date)) = chart_date_range(app.period, &app.today, &records) else {
        return;
    };

    let day_count = (days_diff(&min_date, &max_date).unwrap_or(0).max(0) + 1) as usize;
    let day_count = day_count.max(1);

    // 每个模型每天的 token 数，使用与模型排行一致的 input + output 口径。
    let mut series: HashMap<String, Vec<f64>> = HashMap::new();
    for m in &top_models {
        series.insert(m.clone(), vec![0.0; day_count]);
    }
    for r in &records {
        let idx = days_diff(&min_date, &r.date).unwrap_or(0).max(0) as usize;
        if idx >= day_count {
            continue;
        }
        let mut totals = UsageTotals::default();
        totals.add_record(r);
        let tokens = totals.total_tokens() as f64;
        if let Some(v) = series.get_mut(&r.model) {
            v[idx] += tokens;
        }
    }

    let mut max_y: f64 = 1.0;
    let mut chart_series: Vec<ChartSeries> = Vec::new();
    for (idx, model) in top_models.iter().enumerate() {
        let color = PALETTE[idx % PALETTE.len()];
        let values = series.get(model).cloned().unwrap_or_default();
        for &y in &values {
            if y > max_y {
                max_y = y;
            }
        }
        chart_series.push((model.clone(), values, color));
    }

    draw_step_chart(
        f,
        area,
        &min_date,
        &max_date,
        day_count,
        max_y,
        &chart_series,
    );
}

fn draw_bar_chart_race(
    f: &mut ratatui::Frame,
    area: Rect,
    app: &StatsApp,
    frames: &[RaceFrame],
    title_suffix: &str,
) {
    let chart_area = fixed_chart_area(area);
    let title = format!("Model Tokens Top 15 · {title_suffix}");
    if chart_area.width < 32 || chart_area.height < 6 {
        f.render_widget(Paragraph::new(title), chart_area);
        return;
    }

    if frames.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "  No data for bar chart race.",
            Style::default().fg(Color::DarkGray),
        )))
        .block(Block::default().title(format!(" {title} ")));
        f.render_widget(p, chart_area);
        return;
    }

    let Some((previous, current, tween)) = current_race_frame(app, frames) else {
        return;
    };
    let max_value = race_max_value(frames);
    draw_race_frame(f, chart_area, previous, current, tween, max_value, title_suffix);
}

fn draw_race_frame(
    f: &mut ratatui::Frame,
    chart_area: Rect,
    previous: &RaceFrame,
    current: &RaceFrame,
    tween: f64,
    max_value: u64,
    title_suffix: &str,
) {
    let s = smoothstep(tween);
    let prev_in: u64 = previous.cells.values().map(|u| u.in_tokens).sum();
    let prev_out: u64 = previous.cells.values().map(|u| u.out_tokens).sum();
    let curr_in: u64 = current.cells.values().map(|u| u.in_tokens).sum();
    let curr_out: u64 = current.cells.values().map(|u| u.out_tokens).sum();
    let total_in = interpolate_u64(prev_in, curr_in, s);
    let total_out = interpolate_u64(prev_out, curr_out, s);
    let title = Line::from(vec![
        Span::styled(
            format!(" Model Tokens Top 15 · {title_suffix} "),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            short_date(&current.date),
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " ↑{} ↓{}",
                format_tokens(total_in),
                format_tokens(total_out)
            ),
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(
        Paragraph::new(title),
        Rect::new(chart_area.x, chart_area.y, chart_area.width, 1),
    );

    if current.entries.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  Waiting for the first model token usage...",
                Style::default().fg(Color::DarkGray),
            ))),
            Rect::new(
                chart_area.x,
                chart_area.y + 2,
                chart_area.width,
                chart_area.height.saturating_sub(2),
            ),
        );
        return;
    }

    let row_count = RACE_VISIBLE_MODELS
        .min(current.entries.len())
        .min(chart_area.height.saturating_sub(2) as usize);
    if row_count == 0 {
        return;
    }

    let model_width = current
        .entries
        .iter()
        .take(row_count)
        .map(|entry| text_width(&entry.model))
        .max()
        .unwrap_or(10)
        .clamp(10, 26);
    let value_width = current
        .entries
        .iter()
        .take(row_count)
        .map(|entry| text_width(&usage_cell_text(&entry.usage)))
        .max()
        .unwrap_or(4)
        .max(4);

    let bar_left = chart_area.x + model_width + 2;
    let bar_right = chart_area
        .right()
        .saturating_sub(value_width)
        .saturating_sub(3);
    if bar_left >= bar_right {
        return;
    }

    let plot_top = chart_area.y + 2;
    let plot_bottom = plot_top + row_count as u16 - 1;
    let previous_ranks = race_rank_map(previous);
    let previous_usages = race_usage_map(previous);
    let eased = smoothstep(tween);
    let bar_width = bar_right.saturating_sub(bar_left) + 1;
    let mut occupied_rows = HashSet::new();

    for (rank, entry) in current.entries.iter().take(row_count).enumerate() {
        let previous_rank = previous_ranks
            .get(&entry.model)
            .copied()
            .unwrap_or(row_count)
            .min(row_count);
        let interpolated_rank = previous_rank as f64 + (rank as f64 - previous_rank as f64) * eased;
        let candidate_row = plot_top + interpolated_rank.round() as u16;
        let Some(row) = nearest_free_row(candidate_row, plot_top, plot_bottom, &occupied_rows)
        else {
            continue;
        };
        occupied_rows.insert(row);

        let previous_usage = previous_usages
            .get(&entry.model)
            .copied()
            .unwrap_or_default();
        let usage = interpolate_usage_totals(previous_usage, entry.usage, eased);
        let total_tokens = usage.total_tokens();
        let bar_len = ((total_tokens as f64 / max_value.max(1) as f64) * f64::from(bar_width))
            .round()
            .max(if total_tokens > 0 { 1.0 } else { 0.0 }) as u16;
        let label = truncate_text(&entry.model, model_width);
        let value_label = usage_cell_text(&usage);
        let style = Style::default().fg(entry.color);
        let buf = f.buffer_mut();
        buf.set_string(chart_area.x, row, label, style.add_modifier(Modifier::BOLD));
        if bar_len > 0 {
            buf.set_string(
                bar_left,
                row,
                "█".repeat(bar_len.min(bar_width) as usize),
                style,
            );
        }
        buf.set_string(
            bar_right + 2,
            row,
            value_label,
            Style::default().fg(Color::DarkGray),
        );
    }
}

fn draw_step_chart(
    f: &mut ratatui::Frame,
    area: Rect,
    min_date: &str,
    max_date: &str,
    day_count: usize,
    max_y: f64,
    series: &[ChartSeries],
) {
    let chart_area = fixed_chart_area(area);
    if chart_area.width < 24 || chart_area.height < 6 {
        f.render_widget(Paragraph::new("Tokens per Day"), chart_area);
        return;
    }

    let y_ticks = y_tick_values(max_y, Y_TICK_COUNT);
    let y_tick_labels: Vec<String> = y_ticks
        .iter()
        .map(|value| format_tokens(value.round() as u64))
        .collect();
    let label_width = y_tick_labels
        .iter()
        .map(|label| label.chars().count() as u16)
        .max()
        .unwrap_or(1)
        .max(4);

    let title = Line::from(Span::styled(
        " Tokens per Day ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    f.render_widget(
        Paragraph::new(title),
        Rect::new(chart_area.x, chart_area.y, chart_area.width, 1),
    );

    let axis_x = chart_area.x + label_width;
    let plot_left = axis_x + 1;
    let available_plot_right = chart_area.right().saturating_sub(1);
    let legend_width = chart_legend_max_width(series)
        .min(available_plot_right.saturating_sub(plot_left) + 1)
        .max(1);
    let legend_gap = 2;
    let plot_right =
        plot_right_before_legend(plot_left, available_plot_right, legend_width, legend_gap);
    let plot_top = chart_area.y + 2;
    let plot_bottom = chart_area.bottom().saturating_sub(2);
    if plot_left >= plot_right || plot_top >= plot_bottom {
        return;
    }

    let max_bound = (max_y * 1.05).max(1.0);
    let axis_style = Style::default().fg(Color::DarkGray);

    let plot_area = Rect::new(
        plot_left,
        plot_top,
        plot_right.saturating_sub(plot_left) + 1,
        plot_bottom.saturating_sub(plot_top) + 1,
    );

    let buf = f.buffer_mut();
    let mut used_y_tick_rows = HashSet::new();
    for (value, label) in y_ticks.iter().zip(y_tick_labels.iter()) {
        let y = value_row(*value, max_bound, plot_top, plot_bottom);
        if used_y_tick_rows.insert(y) {
            right_aligned_label(buf, chart_area.x, y, label_width, label, axis_style);
        }
    }

    for y in plot_top..=plot_bottom {
        buf.set_string(axis_x, y, "│", axis_style);
    }

    let mut occupied = ChartOccupancy::new();
    for (_, values, color) in series {
        draw_rounded_step_series(
            buf,
            plot_area,
            day_count,
            max_bound,
            values,
            *color,
            &mut occupied,
        );
    }

    let x_label_y = plot_bottom + 1;
    draw_x_tick_labels(
        buf, min_date, max_date, day_count, plot_left, plot_right, x_label_y, axis_style,
    );

    let legend_x = available_plot_right
        .saturating_add(1)
        .saturating_sub(legend_width);
    let legend_height = (series.len() as u16).min(plot_bottom.saturating_sub(plot_top) + 1);
    let legend_area = Rect::new(legend_x, plot_top, legend_width, legend_height);
    draw_chart_legend(f, legend_area, series);
}

fn fixed_chart_area(area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y,
        area.width.min(STEP_CHART_MAX_WIDTH),
        area.height.min(STEP_CHART_HEIGHT),
    )
}

fn chart_date_range(
    period: Period,
    today: &str,
    records: &[&UsageRecord],
) -> Option<(String, String)> {
    match period {
        Period::Today => Some((today.to_string(), today.to_string())),
        Period::Lastday => Some((date_offset(today, -1).ok()?, date_offset(today, -1).ok()?)),
        Period::Last7 => Some((date_offset(today, -6).ok()?, today.to_string())),
        Period::Last30 => {
            let days = super::date::previous_month_days(today) as i64;
            Some((date_offset(today, -(days - 1)).ok()?, today.to_string()))
        }
        Period::All => {
            let first = records.first()?;
            let mut min_date = first.date.clone();
            let mut max_date = first.date.clone();
            for record in records.iter().skip(1) {
                if record.date.as_str() < min_date.as_str() {
                    min_date = record.date.clone();
                }
                if record.date.as_str() > max_date.as_str() {
                    max_date = record.date.clone();
                }
            }
            Some((min_date, max_date))
        }
    }
}

/// 7 天滚动窗口长度（含当日）。第一个能形成完整窗口的 frame 日期为
/// `min_date + 6`，此时累计 7 天数据。
const ROLLING_WINDOW_DAYS: i64 = 7;

fn race_frames(records: &[UsageRecord], window: RaceWindow) -> Vec<RaceFrame> {
    let Some((min_date, max_date)) = record_date_range(records) else {
        return Vec::new();
    };
    let day_count = (days_diff(&min_date, &max_date).unwrap_or(0).max(0) + 1) as usize;
    if day_count == 0 {
        return Vec::new();
    }
    let color_map = race_color_map(records);
    let mut deltas_by_date: HashMap<String, HashMap<(String, String), UsageTotals>> =
        HashMap::new();
    for record in records {
        deltas_by_date
            .entry(record.date.clone())
            .or_default()
            .entry((record.agent.clone(), record.model.clone()))
            .or_default()
            .add_record(record);
    }

    let snapshots = match window {
        RaceWindow::PerDay => all_time_snapshots(&min_date, &max_date, day_count, &deltas_by_date),
        RaceWindow::Rolling7 => rolling_snapshots(&min_date, &max_date, day_count, &deltas_by_date),
    };

    let start_idx = match window {
        RaceWindow::PerDay => 0,
        RaceWindow::Rolling7 => (ROLLING_WINDOW_DAYS - 1) as usize,
    };

    let mut frames = Vec::with_capacity(day_count.saturating_sub(start_idx));
    for day_idx in start_idx..day_count {
        let date = date_for_day(&min_date, &max_date, day_idx, day_count);
        let cells = interpolated_race_cells(day_idx, &snapshots);
        let totals = totals_by_model_from_cells(&cells);
        let entries = race_entries(&totals, &color_map);
        frames.push(RaceFrame {
            date,
            entries,
            cells,
        });
    }
    frames
}

/// All-Time race：自 day0 起累计，每发生记录的天生成一个 snapshot。
/// 间隔天在 [`interpolated_race_cells`] 中沿用前一个 snapshot 的值（零增长）。
fn all_time_snapshots(
    min_date: &str,
    max_date: &str,
    day_count: usize,
    deltas_by_date: &HashMap<String, HashMap<(String, String), UsageTotals>>,
) -> Vec<RaceSnapshot> {
    let mut cumulative: HashMap<(String, String), UsageTotals> = HashMap::new();
    let mut snapshots: Vec<RaceSnapshot> = Vec::new();
    for day_idx in 0..day_count {
        let date = date_for_day(min_date, max_date, day_idx, day_count);
        if let Some(deltas) = deltas_by_date.get(&date) {
            for (key, usage) in deltas {
                cumulative.entry(key.clone()).or_default().add(usage);
            }
            snapshots.push((day_idx, cumulative.clone()));
        }
    }
    snapshots
}

/// 滚动 7 天 race：每个 day 生成一个 snapshot，cells 为
/// 最近 7 天（含当日）的累计值。Rolling7 每天都有 snapshot，无需插值。
fn rolling_snapshots(
    min_date: &str,
    max_date: &str,
    day_count: usize,
    deltas_by_date: &HashMap<String, HashMap<(String, String), UsageTotals>>,
) -> Vec<RaceSnapshot> {
    let mut snapshots: Vec<RaceSnapshot> = Vec::with_capacity(day_count);
    for day_idx in 0..day_count {
        let mut cells: HashMap<(String, String), UsageTotals> = HashMap::new();
        for offset in 0..ROLLING_WINDOW_DAYS {
            let lookback_idx = day_idx as i64 - offset;
            if lookback_idx < 0 {
                break;
            }
            let lookback_date = date_for_day(min_date, max_date, lookback_idx as usize, day_count);
            if let Some(deltas) = deltas_by_date.get(&lookback_date) {
                for (key, usage) in deltas {
                    cells.entry(key.clone()).or_default().add(usage);
                }
            }
        }
        snapshots.push((day_idx, cells));
    }
    snapshots
}

fn date_for_day(min_date: &str, max_date: &str, day_idx: usize, day_count: usize) -> String {
    if day_idx + 1 == day_count {
        max_date.to_string()
    } else {
        date_offset(min_date, day_idx as i64).unwrap_or_else(|_| min_date.to_string())
    }
}

/// 返回指定天的 cell 值：有 snapshot 则直接使用，空白天沿用前一个 snapshot（零增长）。
/// 不做日期间线性插值，避免在没有真实数据的空白日产生虚假增长。
fn interpolated_race_cells(
    day_idx: usize,
    snapshots: &[RaceSnapshot],
) -> HashMap<(String, String), UsageTotals> {
    let Some((first_idx, first_values)) = snapshots.first() else {
        return HashMap::new();
    };
    if day_idx <= *first_idx {
        return first_values.clone();
    }

    for window in snapshots.windows(2) {
        let (previous_idx, previous_values) = &window[0];
        let (next_idx, next_values) = &window[1];
        if day_idx == *previous_idx {
            return previous_values.clone();
        }
        if day_idx == *next_idx {
            return next_values.clone();
        }
        if day_idx > *previous_idx && day_idx < *next_idx {
            // 空白天：使用前一个 snapshot 的值，表示零增长而非虚假增长
            return previous_values.clone();
        }
    }

    // day_idx 超出所有 snapshot 范围：沿用最后一个 snapshot
    snapshots
        .last()
        .map(|(_, values)| values.clone())
        .unwrap_or_default()
}

fn interpolate_usage_cells(
    previous: &HashMap<(String, String), UsageTotals>,
    next: &HashMap<(String, String), UsageTotals>,
    tween: f64,
) -> HashMap<(String, String), UsageTotals> {
    let keys: HashSet<&(String, String)> = previous.keys().chain(next.keys()).collect();
    keys.into_iter()
        .map(|key| {
            let from = previous.get(key).copied().unwrap_or_default();
            let to = next.get(key).copied().unwrap_or_default();
            (key.clone(), interpolate_usage_totals(from, to, tween))
        })
        .collect()
}

fn interpolate_usage_totals(from: UsageTotals, to: UsageTotals, tween: f64) -> UsageTotals {
    UsageTotals {
        in_tokens: interpolate_u64(from.in_tokens, to.in_tokens, tween),
        total_tokens: interpolate_u64(from.total_tokens, to.total_tokens, tween),
        out_tokens: interpolate_u64(from.out_tokens, to.out_tokens, tween),
        cache_read_input_tokens: interpolate_u64(
            from.cache_read_input_tokens,
            to.cache_read_input_tokens,
            tween,
        ),
        cache_creation_input_tokens: interpolate_u64(
            from.cache_creation_input_tokens,
            to.cache_creation_input_tokens,
            tween,
        ),
    }
}

fn totals_by_model_from_cells(
    cells: &HashMap<(String, String), UsageTotals>,
) -> HashMap<String, UsageTotals> {
    let mut totals: HashMap<String, UsageTotals> = HashMap::new();
    for ((_, model), usage) in cells {
        totals.entry(model.clone()).or_default().add(usage);
    }
    totals
}

fn race_entries(
    totals: &HashMap<String, UsageTotals>,
    color_map: &HashMap<String, Color>,
) -> Vec<RaceEntry> {
    let mut entries: Vec<RaceEntry> = totals
        .iter()
        .filter(|(_, usage)| usage.total_tokens() > 0)
        .map(|(model, usage)| RaceEntry {
            color: color_map.get(model).copied().unwrap_or(Color::White),
            model: model.clone(),
            value: usage.total_tokens(),
            usage: *usage,
        })
        .collect();
    entries.sort_by(|left, right| {
        right
            .value
            .cmp(&left.value)
            .then_with(|| left.model.cmp(&right.model))
    });
    entries.truncate(RACE_VISIBLE_MODELS);
    entries
}

fn race_max_value(frames: &[RaceFrame]) -> u64 {
    frames
        .iter()
        .flat_map(|frame| frame.entries.iter().map(|entry| entry.value))
        .max()
        .unwrap_or(1)
        .max(1)
}

fn record_date_range(records: &[UsageRecord]) -> Option<(String, String)> {
    let first = records.first()?;
    let mut min_date = first.date.clone();
    let mut max_date = first.date.clone();
    for record in records.iter().skip(1) {
        if record.date.as_str() < min_date.as_str() {
            min_date = record.date.clone();
        }
        if record.date.as_str() > max_date.as_str() {
            max_date = record.date.clone();
        }
    }
    Some((min_date, max_date))
}

fn race_color_map(records: &[UsageRecord]) -> HashMap<String, Color> {
    let record_refs = records.iter().collect::<Vec<_>>();
    let totals = totals_by_model(&record_refs);
    let mut models: Vec<(String, u64)> = totals
        .into_iter()
        .map(|(model, usage)| (model, usage.total_tokens()))
        .collect();
    models.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    models
        .into_iter()
        .enumerate()
        .map(|(idx, (model, _))| (model, PALETTE[idx % PALETTE.len()]))
        .collect()
}

#[cfg(test)]
fn race_frame_index(tick: usize, frame_count: usize) -> usize {
    if frame_count == 0 {
        0
    } else {
        match race_phase(tick, frame_count) {
            RacePhase::Playing { current_idx, .. } => current_idx,
            RacePhase::HoldingLast { idx }
            | RacePhase::DissolvingLast { idx, .. }
            | RacePhase::CoalescingFirst { idx, .. } => idx,
        }
    }
}

fn race_cycle_tick(tick: usize, frame_count: usize) -> usize {
    if frame_count == 0 {
        return 0;
    }
    let frame_ticks = frame_count.saturating_mul(RACE_TWEEN_STEPS);
    let cycle_ticks = frame_ticks
        .saturating_add(RACE_FINAL_HOLD_TICKS)
        .saturating_add(RACE_FINAL_DISSOLVE_TICKS)
        .saturating_add(RACE_INITIAL_COALESCE_TICKS);
    tick % cycle_ticks
}

fn current_race_frame<'a>(
    app: &StatsApp,
    frames: &'a [RaceFrame],
) -> Option<(&'a RaceFrame, &'a RaceFrame, f64)> {
    if frames.is_empty() {
        return None;
    }
    match race_phase(app.race_tick, frames.len()) {
        RacePhase::Playing {
            previous_idx,
            current_idx,
            tween,
        } => Some((&frames[previous_idx], &frames[current_idx], tween)),
        RacePhase::HoldingLast { idx }
        | RacePhase::DissolvingLast { idx, .. }
        | RacePhase::CoalescingFirst { idx, .. } => Some((&frames[idx], &frames[idx], 1.0)),
    }
}

#[cfg(test)]
fn race_tween(tick: usize, frame_count: usize) -> f64 {
    match race_phase(tick, frame_count) {
        RacePhase::Playing { tween, .. } => tween,
        RacePhase::HoldingLast { .. }
        | RacePhase::DissolvingLast { .. }
        | RacePhase::CoalescingFirst { .. } => 1.0,
    }
}

fn race_phase(tick: usize, frame_count: usize) -> RacePhase {
    if frame_count == 0 {
        return RacePhase::HoldingLast { idx: 0 };
    }
    let cycle_tick = race_cycle_tick(tick, frame_count);
    let frame_ticks = frame_count.saturating_mul(RACE_TWEEN_STEPS);
    if cycle_tick < frame_ticks {
        let current_idx = cycle_tick / RACE_TWEEN_STEPS;
        return RacePhase::Playing {
            previous_idx: current_idx.saturating_sub(1),
            current_idx,
            tween: (cycle_tick % RACE_TWEEN_STEPS) as f64 / RACE_TWEEN_STEPS as f64,
        };
    }

    let last_idx = frame_count - 1;
    let hold_end = frame_ticks.saturating_add(RACE_FINAL_HOLD_TICKS);
    if cycle_tick < hold_end {
        return RacePhase::HoldingLast { idx: last_idx };
    }

    let dissolve_end = hold_end.saturating_add(RACE_FINAL_DISSOLVE_TICKS);
    if cycle_tick < dissolve_end {
        let progress =
            ((cycle_tick - hold_end + 1) as f64 / RACE_FINAL_DISSOLVE_TICKS as f64).clamp(0.0, 1.0);
        return RacePhase::DissolvingLast {
            idx: last_idx,
            progress: smoothstep(progress),
        };
    }

    let progress = ((cycle_tick - dissolve_end + 1) as f64 / RACE_INITIAL_COALESCE_TICKS as f64)
        .clamp(0.0, 1.0);
    RacePhase::CoalescingFirst {
        idx: 0,
        progress: smoothstep(progress),
    }
}

fn apply_race_transition(buf: &mut Buffer, area: Rect, phase: RacePhase) {
    match phase {
        RacePhase::DissolvingLast { progress, .. } => {
            apply_transition_mask(buf, area, progress, TransitionMask::Dissolve);
        }
        RacePhase::CoalescingFirst { progress, .. } => {
            apply_transition_mask(buf, area, progress, TransitionMask::Coalesce);
        }
        RacePhase::Playing { .. } | RacePhase::HoldingLast { .. } => {}
    }
}

fn apply_transition_mask(buf: &mut Buffer, area: Rect, progress: f64, mask: TransitionMask) {
    let mut rng = TransitionRng::new(RACE_TRANSITION_SEED);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let threshold = rng.gen_f32() as f64;
            let clear = match mask {
                TransitionMask::Dissolve => progress >= threshold,
                TransitionMask::Coalesce => progress < threshold,
            };
            if clear {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(' ');
                    cell.set_style(Style::default());
                }
            }
        }
    }
}

enum TransitionMask {
    Dissolve,
    Coalesce,
}

// SplitMix32 matches tachyonfx's SimpleRng so the transition mask behaves like the referenced effect.
#[derive(Clone, Copy)]
struct TransitionRng {
    state: u32,
}

impl TransitionRng {
    fn new(seed: u32) -> Self {
        Self { state: seed }
    }

    fn next_u32(&mut self) -> u32 {
        self.state = self.state.wrapping_add(0x9E3779B9);
        let mut z = self.state;
        z = (z ^ (z >> 15)).wrapping_mul(0x85EBCA6B);
        z = (z ^ (z >> 13)).wrapping_mul(0xC2B2AE35);
        z ^ (z >> 16)
    }

    fn gen_f32(&mut self) -> f32 {
        const EXPONENT: u32 = 0x3f80_0000;
        let mantissa = self.next_u32() >> 9;
        f32::from_bits(EXPONENT | mantissa) - 1.0
    }
}

fn smoothstep(value: f64) -> f64 {
    let value = value.clamp(0.0, 1.0);
    value * value * (3.0 - 2.0 * value)
}

fn interpolate_u64(from: u64, to: u64, tween: f64) -> u64 {
    (from as f64 + (to as f64 - from as f64) * tween)
        .round()
        .max(0.0) as u64
}

fn race_rank_map(frame: &RaceFrame) -> HashMap<String, usize> {
    frame
        .entries
        .iter()
        .enumerate()
        .map(|(rank, entry)| (entry.model.clone(), rank))
        .collect()
}

fn race_usage_map(frame: &RaceFrame) -> HashMap<String, UsageTotals> {
    frame
        .entries
        .iter()
        .map(|entry| (entry.model.clone(), entry.usage))
        .collect()
}

fn nearest_free_row(
    candidate: u16,
    top: u16,
    bottom: u16,
    occupied_rows: &HashSet<u16>,
) -> Option<u16> {
    if top > bottom {
        return None;
    }
    let candidate = candidate.clamp(top, bottom);
    if !occupied_rows.contains(&candidate) {
        return Some(candidate);
    }

    let max_distance = bottom.saturating_sub(top);
    for distance in 1..=max_distance {
        if let Some(row) = candidate.checked_sub(distance) {
            if row >= top && !occupied_rows.contains(&row) {
                return Some(row);
            }
        }
        let row = candidate.saturating_add(distance);
        if row <= bottom && !occupied_rows.contains(&row) {
            return Some(row);
        }
    }
    None
}

fn right_aligned_label(
    buf: &mut ratatui::buffer::Buffer,
    x: u16,
    y: u16,
    width: u16,
    label: &str,
    style: Style,
) {
    let label_width = label.chars().count() as u16;
    let label_x = x + width.saturating_sub(label_width);
    buf.set_string(label_x, y, label, style);
}

fn y_tick_values(max_y: f64, tick_count: usize) -> Vec<f64> {
    if tick_count <= 1 {
        return vec![0.0];
    }

    let max_y = max_y.max(0.0);
    (0..tick_count)
        .map(|idx| max_y * idx as f64 / (tick_count - 1) as f64)
        .collect()
}

fn x_tick_indices(day_count: usize) -> Vec<usize> {
    match day_count {
        0 => Vec::new(),
        1..=7 => (0..day_count).collect(),
        _ => {
            let tick_count = X_TICK_MIN_COUNT.min(day_count);
            let last = day_count - 1;
            let mut indices = Vec::with_capacity(tick_count);
            for idx in 0..tick_count {
                let numerator = idx * last + (tick_count - 1) / 2;
                indices.push(numerator / (tick_count - 1));
            }
            indices.dedup();
            indices
        }
    }
}

fn draw_x_tick_labels(
    buf: &mut Buffer,
    min_date: &str,
    max_date: &str,
    day_count: usize,
    plot_left: u16,
    plot_right: u16,
    y: u16,
    style: Style,
) {
    let mut occupied_columns = HashSet::new();
    for idx in x_tick_indices(day_count) {
        let date = if idx + 1 == day_count {
            max_date.to_string()
        } else {
            date_offset(min_date, idx as i64).unwrap_or_else(|_| String::new())
        };
        let label = short_date(&date);
        let label_width = label.chars().count() as u16;
        let tick_x = chart_x_boundary(idx, day_count.max(1), plot_left, plot_right);
        let label_x = tick_x
            .saturating_sub(label_width / 2)
            .max(plot_left)
            .min(plot_right.saturating_sub(label_width.saturating_sub(1)));
        let label_end = label_x.saturating_add(label_width.saturating_sub(1));
        if (label_x..=label_end).any(|x| occupied_columns.contains(&x)) {
            continue;
        }

        for x in label_x..=label_end {
            occupied_columns.insert(x);
        }
        buf.set_string(label_x, y, label, style);
    }
}

fn value_row(value: f64, max_bound: f64, plot_top: u16, plot_bottom: u16) -> u16 {
    let height = plot_bottom.saturating_sub(plot_top);
    let ratio = (value / max_bound).clamp(0.0, 1.0);
    plot_bottom.saturating_sub((ratio * f64::from(height)).round() as u16)
}

fn draw_rounded_step_series(
    buf: &mut Buffer,
    plot_area: Rect,
    day_count: usize,
    max_bound: f64,
    values: &[f64],
    color: Color,
    occupied: &mut ChartOccupancy,
) {
    if values.is_empty() || day_count == 0 || plot_area.width == 0 || plot_area.height == 0 {
        return;
    }

    let plot_left = plot_area.x;
    let plot_right = plot_area.right().saturating_sub(1);
    let plot_top = plot_area.y;
    let plot_bottom = plot_area.bottom().saturating_sub(1);
    let style = Style::default().fg(color);
    let rows: Vec<u16> = values
        .iter()
        .map(|value| value_row(*value, max_bound, plot_top, plot_bottom))
        .collect();

    for idx in 0..values.len() {
        let x0 = chart_x_boundary(idx, day_count, plot_left, plot_right);
        let x1 = chart_x_boundary(idx + 1, day_count, plot_left, plot_right);
        let y = rows[idx];
        let next_y = rows.get(idx + 1);
        let changes_next = next_y.is_some_and(|next| *next != y);
        let end = if changes_next {
            x1.saturating_sub(1)
        } else {
            x1
        };
        draw_horizontal(buf, x0, end, y, style, occupied);

        if let Some(&next_y) = next_y {
            if next_y != y {
                draw_rounded_transition(buf, x1, y, next_y, style, occupied);
            }
        }
    }
}

fn plot_right_before_legend(
    plot_left: u16,
    available_plot_right: u16,
    legend_width: u16,
    legend_gap: u16,
) -> u16 {
    if plot_left >= available_plot_right {
        return plot_left;
    }

    let reserved = legend_width.saturating_add(legend_gap);
    let reserved_right = available_plot_right.saturating_sub(reserved);
    reserved_right.max(plot_left.saturating_add(1))
}

fn chart_x_boundary(idx: usize, day_count: usize, plot_left: u16, plot_right: u16) -> u16 {
    if day_count == 0 || plot_left >= plot_right {
        return plot_left;
    }

    let width = usize::from(plot_right - plot_left);
    let offset = (idx.min(day_count) * width + day_count / 2) / day_count;
    plot_left + offset.min(width) as u16
}

fn draw_horizontal(
    buf: &mut Buffer,
    start: u16,
    end: u16,
    y: u16,
    style: Style,
    occupied: &mut ChartOccupancy,
) {
    if start > end {
        return;
    }

    for x in start..=end {
        set_chart_symbol(buf, x, y, "─", style, occupied);
    }
}

fn draw_rounded_transition(
    buf: &mut Buffer,
    x: u16,
    from_y: u16,
    to_y: u16,
    style: Style,
    occupied: &mut ChartOccupancy,
) {
    let (from_corner, to_corner) = rounded_transition_corners(from_y, to_y);
    set_chart_symbol(buf, x, from_y, from_corner, style, occupied);
    set_chart_symbol(buf, x, to_y, to_corner, style, occupied);

    let start = from_y.min(to_y).saturating_add(1);
    let end = from_y.max(to_y).saturating_sub(1);
    for y in start..=end {
        set_chart_symbol(buf, x, y, "│", style, occupied);
    }
}

fn rounded_transition_corners(from_y: u16, to_y: u16) -> (&'static str, &'static str) {
    if to_y < from_y {
        ("╯", "╭")
    } else {
        ("╮", "╰")
    }
}

fn set_chart_symbol(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    symbol: &str,
    style: Style,
    occupied: &mut ChartOccupancy,
) {
    if !occupied.insert((x, y)) {
        return;
    }

    let Some(cell) = buf.cell_mut((x, y)) else {
        occupied.remove(&(x, y));
        return;
    };
    cell.set_symbol(symbol).set_style(style);
}

fn draw_chart_legend(f: &mut ratatui::Frame, area: Rect, datasets: &[ChartSeries]) {
    let lines: Vec<Line> = datasets
        .iter()
        .map(|(name, _, color)| {
            Line::from(vec![
                Span::styled("● ", Style::default().fg(*color)),
                Span::styled(name.clone(), Style::default().fg(*color)),
            ])
        })
        .collect();

    f.render_widget(Paragraph::new(lines), area);
}

fn chart_legend_max_width(datasets: &[ChartSeries]) -> u16 {
    datasets
        .iter()
        .map(|(name, _, _)| name.chars().count() + 2)
        .max()
        .unwrap_or(1)
        .min(u16::MAX as usize) as u16
}

fn draw_period_switch(f: &mut ratatui::Frame, area: Rect, app: &StatsApp) {
    let mut spans: Vec<Span> = Vec::new();
    for (i, p) in [
        Period::Today,
        Period::Lastday,
        Period::Last7,
        Period::Last30,
        Period::All,
    ]
    .iter()
    .enumerate()
    {
        if i > 0 {
            spans.push(Span::raw(" · "));
        }
        let style = if app.period == *p {
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(p.label(&app.today), style));
    }
    let p = Paragraph::new(Line::from(spans));
    f.render_widget(p, area);
}

fn draw_overview_model_list(f: &mut ratatui::Frame, area: Rect, app: &mut StatsApp) {
    let records = app.period_records();
    let cells = totals_by_agent_model(&records);
    let totals = totals_by_model(&records);
    let model_count = totals.values().filter(|u| u.total_tokens() > 0).count();
    let visible = area.height.saturating_sub(3) as usize;
    let max_scroll = model_count.saturating_sub(visible.max(1));
    if app.models_scroll > max_scroll {
        app.models_scroll = max_scroll;
    }
    let shown = model_count.saturating_sub(app.models_scroll).min(visible);
    let title = format!("Models · {} of {}", shown, model_count);
    draw_model_table(f, area, app, &title, cells, totals, None, true);
}

fn draw_dynamic_model_list(
    f: &mut ratatui::Frame,
    area: Rect,
    app: &mut StatsApp,
    frames: &[RaceFrame],
    window: RaceWindow,
) {
    let Some((previous, current, tween)) = current_race_frame(app, frames) else {
        draw_model_table(
            f,
            area,
            app,
            "Model Tokens Top 0",
            HashMap::new(),
            HashMap::new(),
            None,
            true,
        );
        return;
    };
    let displayed_cells =
        interpolate_usage_cells(&previous.cells, &current.cells, smoothstep(tween));
    let displayed_totals = totals_by_model_from_cells(&displayed_cells);
    let color_map = race_color_map(&app.records);

    let model_count = displayed_totals
        .values()
        .filter(|u| u.total_tokens() > 0)
        .count();
    let total_in: u64 = displayed_totals.values().map(|u| u.in_tokens).sum();
    let total_out: u64 = displayed_totals.values().map(|u| u.out_tokens).sum();
    let interval_label = app.race_interval.label(&app.today);
    let window_label = window.label();
    let date_short = short_date(&current.date);
    let total_in_fmt = format_tokens(total_in);
    let total_out_fmt = format_tokens(total_out);
    let title = format!(
        "Model Tokens Top {model_count} · {interval_label} · {window_label} {date_short} ↑{total_in_fmt} ↓{total_out_fmt}"
    );
    draw_model_table(
        f,
        area,
        app,
        &title,
        displayed_cells,
        displayed_totals,
        Some(&color_map),
        true,
    );
}

fn draw_model_table(
    f: &mut ratatui::Frame,
    area: Rect,
    app: &mut StatsApp,
    title: &str,
    cells: HashMap<(String, String), UsageTotals>,
    totals: HashMap<String, UsageTotals>,
    color_map: Option<&HashMap<String, Color>>,
    hide_empty_agents: bool,
) {
    let total_all: u64 = totals.values().map(|usage| usage.total_tokens()).sum();
    let mut total_usage = UsageTotals::default();
    for u in totals.values() {
        total_usage.add(u);
    }

    if totals.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "  No models to display.",
            Style::default().fg(Color::DarkGray),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {title} ")),
        );
        f.render_widget(p, area);
        return;
    }

    let mut sorted: Vec<(String, UsageTotals)> = totals.into_iter().collect();
    sorted.retain(|(_, usage)| usage.total_tokens() > 0);
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1.total_tokens()));
    let agent_columns = sorted_agents_by_usage(&cells, hide_empty_agents);

    // 窄终端下优先保证高用量（左侧）agent 列完整显示，低用量（右侧）列
    // 整列隐藏；拉宽终端后右侧列逐步「弹出」——不再等比压缩所有列。
    let inner_width = area.width.saturating_sub(2);
    let total_width_val = usage_column_width("Total", sorted.iter().map(|(_, usage)| usage));
    let fixed_columns_width = MODEL_MIN_WIDTH + SHARE_WIDTH + total_width_val;
    let ideal_agent_widths: Vec<u16> = agent_columns
        .iter()
        .map(|(agent, label)| {
            let agent = (*agent).to_string();
            let usages = sorted
                .iter()
                .filter_map(|(model, _)| cells.get(&(agent.clone(), model.clone())));
            usage_column_width(label, usages)
        })
        .collect();
    let (visible_agents, visible_agent_widths) = prioritize_agent_columns(
        &agent_columns,
        &ideal_agent_widths,
        inner_width,
        fixed_columns_width,
        TABLE_COLUMN_SPACING,
    );

    let agent_totals: HashMap<&str, UsageTotals> = visible_agents
        .iter()
        .map(|(agent, _)| {
            let mut total = UsageTotals::default();
            for ((a, _), u) in &cells {
                if a == agent {
                    total.add(u);
                }
            }
            (*agent, total)
        })
        .collect();

    let visible = area.height.saturating_sub(3) as usize;
    let max_scroll = sorted.len().saturating_sub(visible.max(1));
    if app.models_scroll > max_scroll {
        app.models_scroll = max_scroll;
    }

    let constraints = model_table_widths_with_agent_widths(
        &sorted,
        &visible_agent_widths,
    );
    let layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints.clone())
        .split(area);
    let model_col_width = layout[0].width.saturating_sub(2) as usize;

    // Overview 的 Model 列底纹按 Total tokens 归一化：统一除以最大模型的
    // Total tokens，使最大者填满整列、其余按比例缩短，让份额差异视觉上更突出。
    // 仅影响底纹绘制，Share 列仍显示真实占比 (pct)。
    let max_total_tokens: u64 = sorted
        .iter()
        .map(|(_, usage)| usage.total_tokens())
        .max()
        .unwrap_or(0);

    let rows: Vec<Row> = sorted
        .iter()
        .enumerate()
        .skip(app.models_scroll)
        .take(visible)
        .map(|(idx, (model, usage))| {
            let pct = if total_all > 0 {
                usage.total_tokens() as f64 * 100.0 / total_all as f64
            } else {
                0.0
            };
            let model_cell = if color_map.is_some() {
                let dot_color = color_map
                    .and_then(|colors| colors.get(model).copied())
                    .unwrap_or(PALETTE[idx % PALETTE.len()]);
                Cell::from(Span::styled(
                    model.clone(),
                    Style::default().fg(dot_color).add_modifier(Modifier::BOLD),
                ))
            } else {
                let ratio = if max_total_tokens > 0 {
                    usage.total_tokens() as f64 / max_total_tokens as f64
                } else {
                    0.0
                };
                let share_width = (model_col_width as f64 * ratio).round() as usize;
                let model_len = model.chars().count();
                let share_chars = share_width.min(model_len);

                let model_color = PALETTE[idx % PALETTE.len()];
                let mut chars = model.chars();
                let model_prefix: String = chars.by_ref().take(share_chars).collect();
                let model_suffix: String = chars.collect();

                let mut model_spans = Vec::new();
                if !model_prefix.is_empty() {
                    model_spans.push(Span::styled(
                        model_prefix,
                        Style::default()
                            .fg(Color::Black)
                            .bg(model_color)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                if !model_suffix.is_empty() {
                    model_spans.push(Span::styled(
                        model_suffix,
                        Style::default()
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                if share_width > model_len {
                    model_spans.push(Span::styled(
                        " ".repeat(share_width - model_len),
                        Style::default().bg(model_color),
                    ));
                }
                Cell::from(Line::from(model_spans))
            };

            let mut row_cells = vec![
                model_cell,
                Cell::from(Span::styled(
                    format!("{:.1}%", pct),
                    Style::default().fg(Color::DarkGray),
                )),
                usage_cell(usage),
            ];
            for (agent, _) in &visible_agents {
                let cell = match cells.get(&(agent.to_string(), model.clone())) {
                    Some(usage) => usage_cell(usage),
                    None => Cell::from(Span::styled("—", Style::default().fg(Color::DarkGray))),
                };
                row_cells.push(cell);
            }
            let row_style = if idx % 2 == 0 {
                Style::default()
            } else {
                Style::default().bg(STRIPED_ROW_BG)
            };
            Row::new(row_cells).style(row_style)
        })
        .collect();

    let total_usage_text = usage_cell_text(&total_usage);
    let agent_header_texts: Vec<String> = visible_agents
        .iter()
        .map(|(agent, _)| {
            let total = agent_totals.get(agent).copied().unwrap_or_default();
            usage_cell_text(&total)
        })
        .collect();

    let header_cells: Vec<Cell> =
        [
            Cell::from(
                Text::from(vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        "Model",
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )),
                ])
                .centered(),
            ),
            Cell::from(
                Text::from(vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        "Share",
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )),
                ])
                .centered(),
            ),
            Cell::from(
                Text::from(vec![
                    Line::from(Span::styled(
                        "Total",
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(Span::styled(
                        total_usage_text,
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD),
                    )),
                ])
                .centered(),
            ),
        ]
        .into_iter()
        .chain(visible_agents.iter().zip(agent_header_texts.iter()).map(
            |((_, label), usage_text)| {
                Cell::from(
                    Text::from(vec![
                        Line::from(Span::styled(
                            *label,
                            Style::default()
                                .fg(Color::LightCyan)
                                .add_modifier(Modifier::BOLD),
                        )),
                        Line::from(Span::styled(
                            usage_text.clone(),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::BOLD),
                        )),
                    ])
                    .centered(),
                )
            },
        ))
        .collect();
    let header = Row::new(header_cells).height(2);

    let title_text = format!(" {title} ");
    let table = Table::new(rows, constraints)
        .header(header)
        .column_spacing(TABLE_COLUMN_SPACING)
        .block(Block::default().borders(Borders::ALL).title(title_text));
    f.render_widget(table, area);
}

fn usage_cell(usage: &UsageTotals) -> Cell<'static> {
    Cell::from(usage_cell_text(usage))
}

fn usage_cell_text(usage: &UsageTotals) -> String {
    format!(
        "↑{} ↓{}",
        format_tokens(usage.in_tokens),
        format_tokens(usage.out_tokens)
    )
}

fn sorted_agents_by_usage(
    cells: &HashMap<(String, String), UsageTotals>,
    hide_empty_agents: bool,
) -> Vec<(&'static str, &'static str)> {
    let mut agents: Vec<(usize, &'static str, &'static str, u64)> = MATRIX_AGENTS
        .iter()
        .enumerate()
        .map(|(idx, (agent, label))| {
            let total = cells
                .iter()
                .filter(|((cell_agent, _), _)| cell_agent == agent)
                .map(|(_, usage)| usage.total_tokens())
                .sum();
            (idx, *agent, *label, total)
        })
        .collect();

    if hide_empty_agents {
        agents.retain(|(_, _, _, total)| *total > 0);
    }
    agents.sort_by(|left, right| right.3.cmp(&left.3).then(left.0.cmp(&right.0)));
    agents
        .into_iter()
        .map(|(_, agent, label, _)| (agent, label))
        .collect()
}

fn text_width(text: &str) -> u16 {
    text.chars().count().min(u16::MAX as usize) as u16
}

fn truncate_text(text: &str, max_width: u16) -> String {
    let max_width = max_width as usize;
    if text.chars().count() <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let mut output: String = text.chars().take(max_width - 1).collect();
    output.push('…');
    output
}

fn usage_column_width<'a, I>(header: &str, usages: I) -> u16
where
    I: IntoIterator<Item = &'a UsageTotals>,
{
    usages
        .into_iter()
        .map(|usage| text_width(&usage_cell_text(usage)))
        .fold(text_width(header), u16::max)
}

/// Convenience wrapper: compute visible agents from cells data and build constraints.
/// Used only in tests; production code calls `prioritize_agent_columns` + `model_table_widths_with_agent_widths` directly.
#[cfg(test)]
fn model_table_widths(
    area_width: u16,
    sorted: &[(String, UsageTotals)],
    cells: &HashMap<(String, String), UsageTotals>,
    agent_columns: &[(&'static str, &'static str)],
) -> Vec<Constraint> {
    let total_width = usage_column_width("Total", sorted.iter().map(|(_, usage)| usage));
    let model_width = MODEL_MIN_WIDTH;
    let fixed_columns_width = model_width + SHARE_WIDTH + total_width;
    let inner_width = area_width.saturating_sub(2);
    let ideal_agent_widths: Vec<u16> = agent_columns
        .iter()
        .map(|(agent, label)| {
            let agent = (*agent).to_string();
            let usages = sorted
                .iter()
                .filter_map(|(model, _)| cells.get(&(agent.clone(), model.clone())));
            usage_column_width(label, usages)
        })
        .collect();
    let (_visible_agents, visible_agent_widths) = prioritize_agent_columns(
        agent_columns,
        &ideal_agent_widths,
        inner_width,
        fixed_columns_width,
        TABLE_COLUMN_SPACING,
    );
    model_table_widths_with_agent_widths(sorted, &visible_agent_widths)
}

/// Build table constraints from pre-computed visible agent widths.
fn model_table_widths_with_agent_widths(
    sorted: &[(String, UsageTotals)],
    agent_widths: &[u16],
) -> Vec<Constraint> {
    let total_width = usage_column_width("Total", sorted.iter().map(|(_, usage)| usage));
    let model_width = MODEL_MIN_WIDTH;

    let mut widths = vec![
        Constraint::Min(model_width),
        Constraint::Length(SHARE_WIDTH),
        Constraint::Length(total_width),
    ];
    widths.extend(agent_widths.iter().map(|w| Constraint::Length(*w)));
    widths
}

/// Greedily allocate space for agent columns, prioritizing leftmost (highest-usage) agents.
/// Each agent receives its ideal width; agents that cannot fit are dropped entirely —
/// they reappear when the terminal is widened.
fn prioritize_agent_columns(
    agent_columns: &[(&'static str, &'static str)],
    ideal_widths: &[u16],
    inner_width: u16,
    fixed_columns_width: u16, // model + share + total
    spacing: u16,
) -> (Vec<(&'static str, &'static str)>, Vec<u16>) {
    // 3 fixed columns require 2 spacing gaps; each additional agent column adds
    // 1 more spacing gap + its own width.
    let base_used = fixed_columns_width + spacing * 2;
    let mut used = base_used;
    let mut visible_agents = Vec::new();
    let mut visible_widths = Vec::new();

    for ((agent, label), ideal) in agent_columns.iter().zip(ideal_widths.iter()) {
        let needed = *ideal + spacing;
        if used + needed <= inner_width {
            visible_agents.push((*agent, *label));
            visible_widths.push(*ideal);
            used += needed;
        } else {
            break; // can't fit this agent — stop trying (lower-usage agents are narrower anyway)
        }
    }

    (visible_agents, visible_widths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constraint_length(constraint: Constraint) -> u16 {
        match constraint {
            Constraint::Length(width) | Constraint::Min(width) => width,
            other => panic!("expected length/min constraint, got {other:?}"),
        }
    }

    fn usage(in_tokens: u64, out_tokens: u64) -> UsageTotals {
        UsageTotals {
            in_tokens,
            out_tokens,
            ..UsageTotals::default()
        }
    }

    fn record(model: &str, date: &str, in_tokens: u64, out_tokens: u64) -> UsageRecord {
        agent_record("claude", model, date, in_tokens, out_tokens)
    }

    fn agent_record(
        agent: &str,
        model: &str,
        date: &str,
        in_tokens: u64,
        out_tokens: u64,
    ) -> UsageRecord {
        UsageRecord {
            agent: agent.to_string(),
            model: model.to_string(),
            date: date.to_string(),
            in_tokens,
            total_tokens: in_tokens + out_tokens,
            out_tokens,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        }
    }

    #[test]
    fn chart_area_is_capped_for_large_terminals() {
        let area = fixed_chart_area(Rect::new(2, 3, 120, 30));

        assert_eq!(
            area,
            Rect::new(2, 3, STEP_CHART_MAX_WIDTH, STEP_CHART_HEIGHT)
        );
    }

    #[test]
    fn relative_chart_ranges_use_full_period_windows() {
        let records = vec![UsageRecord {
            agent: "claude".to_string(),
            model: "qwen3.7-max".to_string(),
            date: "2026-05-27".to_string(),
            in_tokens: 1,
            total_tokens: 1,
            out_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        }];
        let record_refs = records.iter().collect::<Vec<_>>();

        assert_eq!(
            chart_date_range(Period::Last7, "2026-05-29", &record_refs),
            Some(("2026-05-23".to_string(), "2026-05-29".to_string()))
        );
        assert_eq!(
            chart_date_range(Period::Last30, "2026-05-29", &record_refs),
            Some(("2026-04-30".to_string(), "2026-05-29".to_string()))
        );
    }

    #[test]
    fn all_time_chart_range_uses_data_extent() {
        let records = vec![
            UsageRecord {
                agent: "claude".to_string(),
                model: "qwen3.7-max".to_string(),
                date: "2026-05-27".to_string(),
                in_tokens: 1,
                total_tokens: 1,
                out_tokens: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            UsageRecord {
                agent: "claude".to_string(),
                model: "qwen3.7-max".to_string(),
                date: "2026-05-12".to_string(),
                in_tokens: 1,
                total_tokens: 1,
                out_tokens: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
        ];
        let record_refs = records.iter().collect::<Vec<_>>();

        assert_eq!(
            chart_date_range(Period::All, "2026-05-29", &record_refs),
            Some(("2026-05-12".to_string(), "2026-05-27".to_string()))
        );
    }

    #[test]
    fn chart_x_boundaries_include_plot_edges() {
        assert_eq!(chart_x_boundary(0, 7, 10, 30), 10);
        assert_eq!(chart_x_boundary(7, 7, 10, 30), 30);
        assert_eq!(chart_x_boundary(99, 7, 10, 30), 30);
    }

    #[test]
    fn plot_right_reserves_space_for_in_chart_legend() {
        assert_eq!(plot_right_before_legend(10, 90, 18, 2), 70);
    }

    #[test]
    fn plot_right_keeps_minimal_plot_when_legend_is_wide() {
        assert_eq!(plot_right_before_legend(10, 20, 18, 2), 11);
    }

    #[test]
    fn y_tick_values_include_zero_and_max() {
        let ticks = y_tick_values(90.0, Y_TICK_COUNT);

        assert_eq!(ticks.len(), Y_TICK_COUNT);
        assert_eq!(ticks.first(), Some(&0.0));
        assert_eq!(ticks.last(), Some(&90.0));
    }

    #[test]
    fn x_tick_indices_draw_every_day_for_short_ranges() {
        assert_eq!(x_tick_indices(7), vec![0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn x_tick_indices_draw_at_least_six_ticks_for_long_ranges() {
        let ticks = x_tick_indices(30);

        assert_eq!(ticks.len(), X_TICK_MIN_COUNT);
        assert_eq!(ticks.first(), Some(&0));
        assert_eq!(ticks.last(), Some(&29));
    }

    #[test]
    fn rounded_transitions_use_directional_corners() {
        assert_eq!(rounded_transition_corners(8, 3), ("╯", "╭"));
        assert_eq!(rounded_transition_corners(3, 8), ("╮", "╰"));
    }

    #[test]
    fn rounded_step_series_draws_box_drawing_glyphs() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 5));
        let mut occupied = ChartOccupancy::new();

        draw_rounded_step_series(
            &mut buf,
            Rect::new(0, 0, 12, 5),
            3,
            3.0,
            &[1.0, 3.0, 2.0],
            Color::Red,
            &mut occupied,
        );

        let symbols: String = buf.content().iter().map(|cell| cell.symbol()).collect();
        assert!(symbols.contains('─'));
        assert!(symbols.contains('╯'));
        assert!(symbols.contains('╭'));
    }

    #[test]
    fn later_series_does_not_create_offset_artifacts() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 5));
        let mut occupied = ChartOccupancy::new();
        let plot_area = Rect::new(0, 0, 12, 5);

        draw_rounded_step_series(
            &mut buf,
            plot_area,
            3,
            3.0,
            &[2.0, 2.0, 2.0],
            Color::Red,
            &mut occupied,
        );
        draw_rounded_step_series(
            &mut buf,
            plot_area,
            3,
            3.0,
            &[2.0, 2.0, 2.0],
            Color::Green,
            &mut occupied,
        );

        let red_row = value_row(2.0, 3.0, plot_area.y, plot_area.bottom() - 1);
        assert!((plot_area.x..plot_area.right()).all(|x| {
            let cell = buf.cell((x, red_row)).expect("cell should be in bounds");
            cell.symbol() == "─" && cell.fg == Color::Red
        }));
        assert!(
            !buf.content()
                .iter()
                .any(|cell| cell.symbol() == "─" && cell.fg == Color::Green)
        );
    }

    #[test]
    fn later_series_still_draws_non_conflicting_values() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 5));
        let mut occupied = ChartOccupancy::new();
        let plot_area = Rect::new(0, 0, 12, 5);

        draw_rounded_step_series(
            &mut buf,
            plot_area,
            3,
            3.0,
            &[2.0, 2.0, 2.0],
            Color::Red,
            &mut occupied,
        );
        draw_rounded_step_series(
            &mut buf,
            plot_area,
            3,
            3.0,
            &[1.0, 1.0, 1.0],
            Color::Green,
            &mut occupied,
        );

        let green_row = value_row(1.0, 3.0, plot_area.y, plot_area.bottom() - 1);
        assert!((plot_area.x..plot_area.right()).all(|x| {
            let cell = buf.cell((x, green_row)).expect("cell should be in bounds");
            cell.symbol() == "─" && cell.fg == Color::Green
        }));
    }

    #[test]
    fn chart_legend_width_uses_longest_item() {
        let datasets = vec![
            ("alpha".to_string(), Vec::new(), Color::Red),
            ("beta".to_string(), Vec::new(), Color::Green),
        ];

        assert_eq!(chart_legend_max_width(&datasets), 7);
    }

    #[test]
    fn race_frames_use_previous_values_for_empty_days() {
        // day0: alpha 100+20, day1: 无数据, day2: beta 200+0
        let records = vec![
            record("alpha", "2026-05-27", 100, 20),
            record("beta", "2026-05-29", 200, 0),
        ];

        let frames = race_frames(&records, RaceWindow::PerDay);

        assert_eq!(frames.len(), 3);
        // day0: alpha 的真实数据
        assert_eq!(frames[0].date, "2026-05-27");
        assert_eq!(frames[0].entries[0].model, "alpha");
        assert_eq!(frames[0].entries[0].value, 120);
        assert_eq!(usage_cell_text(&frames[0].entries[0].usage), "↑100 ↓20");
        // day1: 空白天 → 沿用前一个 snapshot（alpha），零增长，不出现 beta
        assert_eq!(frames[1].date, "2026-05-28");
        assert_eq!(frames[1].entries.len(), 1);
        assert_eq!(frames[1].entries[0].model, "alpha");
        assert_eq!(frames[1].entries[0].value, 120);
        // day2: beta 的真实数据 + alpha 累计
        assert_eq!(frames[2].date, "2026-05-29");
        assert_eq!(frames[2].entries[0].model, "beta");
        assert_eq!(frames[2].entries[0].value, 200);
        assert_eq!(frames[2].entries[1].model, "alpha");
        assert_eq!(frames[2].entries[1].value, 120);
    }

    #[test]
    fn race_frames_consecutive_empty_days_hold_previous_values() {
        // May 27: alpha 100+20, May 28-30: 无数据, May 31: beta 200+0
        let records = vec![
            record("alpha", "2026-05-27", 100, 20),
            record("beta", "2026-05-31", 200, 0),
        ];
        let frames = race_frames(&records, RaceWindow::PerDay);

        assert_eq!(frames.len(), 5);
        // day0: alpha 真实数据
        assert_eq!(frames[0].entries[0].model, "alpha");
        assert_eq!(frames[0].entries[0].value, 120);
        // day1-3: 连续空白天，始终沿用 alpha 前值，不出现 beta
        for i in 1..4 {
            assert_eq!(frames[i].entries.len(), 1);
            assert_eq!(frames[i].entries[0].model, "alpha");
            assert_eq!(frames[i].entries[0].value, 120);
        }
        // day4: beta 真实数据 + alpha 累计
        assert_eq!(frames[4].entries[0].model, "beta");
        assert_eq!(frames[4].entries[0].value, 200);
        assert_eq!(frames[4].entries[1].model, "alpha");
        assert_eq!(frames[4].entries[1].value, 120);
    }

    #[test]
    fn race_frames_keep_only_top_fifteen_models() {
        let records: Vec<UsageRecord> = (0..18)
            .map(|idx| record(&format!("model-{idx:02}"), "2026-05-29", idx + 1, 0))
            .collect();

        let frames = race_frames(&records, RaceWindow::PerDay);
        let models: Vec<&str> = frames[0]
            .entries
            .iter()
            .map(|entry| entry.model.as_str())
            .collect();

        assert_eq!(models.len(), RACE_VISIBLE_MODELS);
        assert_eq!(models.first(), Some(&"model-17"));
        assert_eq!(models.last(), Some(&"model-03"));
    }

    #[test]
    fn race_max_value_uses_global_final_scale() {
        let records = vec![
            record("alpha", "2026-05-27", 100, 0),
            record("beta", "2026-05-28", 1000, 0),
        ];

        let frames = race_frames(&records, RaceWindow::PerDay);

        assert_eq!(frames[0].entries[0].value, 100);
        assert_eq!(race_max_value(&frames), 1000);
    }

    #[test]
    fn rolling_race_sums_seven_day_window_for_each_frame() {
        // 7 天连续数据，每天用量不同。第一个 frame 应是 day 6（7 天窗口的起点），
        // 之后每帧滚动一格。
        let records: Vec<UsageRecord> = (0..10)
            .map(|day| {
                let day_str = format!("2026-05-{:02}", 10 + day);
                record("alpha", &day_str, 100 * (day as u64 + 1), 0)
            })
            .collect();

        let frames = race_frames(&records, RaceWindow::Rolling7);

        // day0..day6（7 天）作为首个 frame，day1..day7（次 7 天）作为第二帧。
        assert_eq!(frames.len(), 4); // day6, day7, day8, day9
        assert_eq!(frames[0].date, "2026-05-16");
        // 首个窗口：[day0, day6] = 100+200+300+400+500+600+700 = 2800
        assert_eq!(frames[0].entries[0].model, "alpha");
        assert_eq!(
            frames[0].entries[0].value,
            100 + 200 + 300 + 400 + 500 + 600 + 700
        );
        // 第二个窗口：[day1, day7] = 200+...+800 = 3500
        assert_eq!(frames[1].date, "2026-05-17");
        assert_eq!(
            frames[1].entries[0].value,
            200 + 300 + 400 + 500 + 600 + 700 + 800
        );
        // 末个窗口：[day3, day9] = 400+500+...+1000 = 4900
        assert_eq!(frames[3].date, "2026-05-19");
        assert_eq!(
            frames[3].entries[0].value,
            400 + 500 + 600 + 700 + 800 + 900 + 1000
        );
    }

    #[test]
    fn rolling_race_drops_models_with_empty_window() {
        // `alpha` 仅有 day0 的数据；`beta` 覆盖 day0..day9。
        // 第一个滚动 7 天窗口为 [05-10, 05-16]，包含 alpha 的全部数据；
        // 后续窗口均不包含 05-10，故 alpha 在末个 frame 中应被过滤。
        let records = vec![
            record("alpha", "2026-05-10", 100, 0),
            record("beta", "2026-05-10", 10, 0),
            record("beta", "2026-05-11", 10, 0),
            record("beta", "2026-05-12", 10, 0),
            record("beta", "2026-05-13", 10, 0),
            record("beta", "2026-05-14", 10, 0),
            record("beta", "2026-05-15", 10, 0),
            record("beta", "2026-05-16", 10, 0),
            record("beta", "2026-05-17", 10, 0),
            record("beta", "2026-05-18", 10, 0),
            record("beta", "2026-05-19", 10, 0),
        ];

        let frames = race_frames(&records, RaceWindow::Rolling7);
        // 末个 frame (2026-05-19) 窗口 [05-13, 05-19]：alpha 在窗口外，值为 0。
        let last = frames.last().expect("rolling race has frames");
        let last_models: Vec<&str> = last
            .entries
            .iter()
            .map(|entry| entry.model.as_str())
            .collect();
        assert!(!last_models.contains(&"alpha"));
        assert!(last_models.contains(&"beta"));
        // 末个窗口 beta 累计 = 7 × 10 = 70
        assert_eq!(last.entries[0].value, 70);
    }

    #[test]
    fn rolling_race_uses_global_max_across_frames() {
        // 7 天后某模型冲高，应作为 `race_max_value` 的全局基准。
        let records: Vec<UsageRecord> = (0..10)
            .map(|day| {
                let day_str = format!("2026-05-{:02}", 10 + day);
                let value = if day == 9 { 5_000 } else { 10 };
                record("alpha", &day_str, value, 0)
            })
            .collect();

        let frames = race_frames(&records, RaceWindow::Rolling7);
        // 末个窗口的 alpha 累计应 = 6*10 + 5000
        assert_eq!(
            frames.last().unwrap().entries[0].value,
            9 * 10 + 5_000 - 3 * 10
        );
        assert_eq!(
            race_max_value(&frames),
            frames.last().unwrap().entries[0].value
        );
    }

    #[test]
    fn race_frames_keep_agent_cells_for_dynamic_table() {
        let records = vec![
            agent_record("claude", "alpha", "2026-05-27", 100, 0),
            agent_record("codex", "beta", "2026-05-28", 300, 0),
        ];

        let frames = race_frames(&records, RaceWindow::PerDay);

        assert_eq!(
            frames[0]
                .cells
                .get(&("claude".to_string(), "alpha".to_string()))
                .map(|usage| usage.total_tokens()),
            Some(100)
        );
        assert_eq!(
            sorted_agents_by_usage(&frames[0].cells, true),
            vec![("claude", "Claude Code")]
        );
        assert_eq!(
            sorted_agents_by_usage(&frames[1].cells, true)[0],
            ("codex", "Codex")
        );
    }

    #[test]
    fn race_frame_index_advances_by_tween_steps() {
        let frame_ticks = RACE_TWEEN_STEPS * 3;
        let cycle_ticks = frame_ticks
            + RACE_FINAL_HOLD_TICKS
            + RACE_FINAL_DISSOLVE_TICKS
            + RACE_INITIAL_COALESCE_TICKS;

        assert_eq!(race_frame_index(0, 3), 0);
        assert_eq!(race_frame_index(RACE_TWEEN_STEPS - 1, 3), 0);
        assert_eq!(race_frame_index(RACE_TWEEN_STEPS, 3), 1);
        assert_eq!(race_frame_index(frame_ticks, 3), 2);
        assert_eq!(
            race_frame_index(
                frame_ticks + RACE_FINAL_HOLD_TICKS + RACE_FINAL_DISSOLVE_TICKS - 1,
                3
            ),
            2
        );
        assert_eq!(race_frame_index(cycle_ticks - 1, 3), 0);
        assert_eq!(race_frame_index(cycle_ticks, 3), 0);
    }

    #[test]
    fn race_tween_reaches_final_value_during_final_hold_and_transition() {
        let frame_ticks = RACE_TWEEN_STEPS * 3;
        let cycle_ticks = frame_ticks
            + RACE_FINAL_HOLD_TICKS
            + RACE_FINAL_DISSOLVE_TICKS
            + RACE_INITIAL_COALESCE_TICKS;

        assert_eq!(race_tween(frame_ticks, 3), 1.0);
        assert_eq!(race_tween(cycle_ticks - 1, 3), 1.0);
    }

    #[test]
    fn race_phase_transitions_from_hold_to_dissolve_to_coalesce() {
        let frame_ticks = RACE_TWEEN_STEPS * 3;
        let dissolve_start = frame_ticks + RACE_FINAL_HOLD_TICKS;
        let coalesce_start = dissolve_start + RACE_FINAL_DISSOLVE_TICKS;

        assert!(matches!(
            race_phase(frame_ticks, 3),
            RacePhase::HoldingLast { idx: 2 }
        ));
        assert!(matches!(
            race_phase(dissolve_start - 1, 3),
            RacePhase::HoldingLast { idx: 2 }
        ));
        assert!(matches!(
            race_phase(dissolve_start, 3),
            RacePhase::DissolvingLast { idx: 2, progress } if progress > 0.0
        ));
        assert!(matches!(
            race_phase(coalesce_start, 3),
            RacePhase::CoalescingFirst { idx: 0, progress } if progress > 0.0
        ));
    }

    #[test]
    fn dissolve_mask_clears_all_cells_at_full_progress() {
        let area = Rect::new(0, 0, 4, 2);
        let mut buf = Buffer::empty(area);
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                buf.cell_mut((x, y))
                    .expect("cell should be in bounds")
                    .set_char('x');
            }
        }

        apply_transition_mask(&mut buf, area, 1.0, TransitionMask::Dissolve);

        assert!(buf.content().iter().all(|cell| cell.symbol() == " "));
    }

    #[test]
    fn coalesce_mask_keeps_cells_visible_at_full_progress() {
        let area = Rect::new(0, 0, 4, 2);
        let mut buf = Buffer::empty(area);
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                buf.cell_mut((x, y))
                    .expect("cell should be in bounds")
                    .set_char('x');
            }
        }

        apply_transition_mask(&mut buf, area, 1.0, TransitionMask::Coalesce);

        assert!(buf.content().iter().all(|cell| cell.symbol() == "x"));
    }

    #[test]
    fn model_table_agent_columns_sort_by_usage() {
        let cells = HashMap::from([
            (
                ("claude".to_string(), "qwen3.7-max".to_string()),
                usage(100, 0),
            ),
            (("codex".to_string(), "gpt-5.4".to_string()), usage(300, 0)),
            (
                ("copilot".to_string(), "gpt-5.5".to_string()),
                usage(200, 0),
            ),
        ]);

        let agents = sorted_agents_by_usage(&cells, false);

        assert_eq!(
            agents,
            vec![
                ("codex", "Codex"),
                ("copilot", "Copilot"),
                ("claude", "Claude Code"),
                ("zed", "Zed Agent"),
                ("omp", "OMP"),
                ("mimo", "Mimo"),
                ("remora", "Remora"),
            ]
        );
    }

    #[test]
    fn model_table_agent_columns_can_hide_empty_agents() {
        let cells = HashMap::from([(
            ("claude".to_string(), "qwen3.7-max".to_string()),
            usage(100, 0),
        )]);

        let agents = sorted_agents_by_usage(&cells, true);

        assert_eq!(agents, vec![("claude", "Claude Code")]);
    }

    #[test]
    fn model_table_widths_keep_stat_columns_compact() {
        let sorted = vec![
            ("qwen3.7-max".to_string(), usage(174_400_000, 547_900)),
            ("deepseek-v4-pro".to_string(), usage(45_700_000, 281_400)),
        ];
        let cells = HashMap::from([
            (
                ("claude".to_string(), "qwen3.7-max".to_string()),
                usage(174_400_000, 547_900),
            ),
            (
                ("claude".to_string(), "deepseek-v4-pro".to_string()),
                usage(45_700_000, 281_400),
            ),
            (
                ("copilot".to_string(), "qwen3.7-max".to_string()),
                usage(510_900, 59_900),
            ),
        ]);
        let agent_columns = sorted_agents_by_usage(&cells, false);

        let widths = model_table_widths(103, &sorted, &cells, &agent_columns);

        // Model column: Min constraint, capped at MODEL_MIN_WIDTH (26)
        assert_eq!(constraint_length(widths[0]), MODEL_MIN_WIDTH);
        assert_eq!(constraint_length(widths[1]), SHARE_WIDTH);
        // With 103px wide terminal, all agents get ideal widths (no shrinking).
        // Only agents that fit at their ideal width are shown.
    }

    #[test]
    fn model_table_narrow_terminal_hides_low_usage_agents() {
        let sorted = vec![
            ("qwen3.7-max".to_string(), usage(174_400_000, 547_900)),
            ("deepseek-v4-pro".to_string(), usage(45_700_000, 281_400)),
        ];
        let cells = HashMap::from([
            (
                ("claude".to_string(), "qwen3.7-max".to_string()),
                usage(174_400_000, 547_900),
            ),
            (
                ("copilot".to_string(), "qwen3.7-max".to_string()),
                usage(510_900, 59_900),
            ),
            (
                ("codex".to_string(), "qwen3.7-max".to_string()),
                usage(300, 100),
            ),
        ]);
        let agent_columns = sorted_agents_by_usage(&cells, true);

        // Narrow terminal (50px) — only fixed columns + the highest-usage agent
        // can fit; low-usage agents are hidden entirely.
        let widths = model_table_widths(50, &sorted, &cells, &agent_columns);
        // Should have 3 fixed columns + at most 1 visible agent column
        assert!(widths.len() <= 4);
        assert_eq!(constraint_length(widths[0]), MODEL_MIN_WIDTH);
    }

    #[test]
    fn model_table_model_column_capped_at_min_width() {
        // 26-char model name fits within MODEL_MIN_WIDTH
        let sorted = vec![
            ("claude-haiku-4-5-20251001".to_string(), usage(100, 0)),
            ("short".to_string(), usage(50, 0)),
        ];
        let cells = HashMap::new();
        let agent_columns: Vec<(&str, &str)> = vec![];

        let widths = model_table_widths(60, &sorted, &cells, &agent_columns);
        assert_eq!(constraint_length(widths[0]), MODEL_MIN_WIDTH);

        // 30-char model name still capped at MODEL_MIN_WIDTH, not expanded
        let longer_sorted = vec![("copilot-suggestions-himalia-001".to_string(), usage(100, 0))];
        let widths = model_table_widths(60, &longer_sorted, &cells, &agent_columns);
        assert_eq!(constraint_length(widths[0]), MODEL_MIN_WIDTH);
    }

    #[test]
    fn prioritize_agent_columns_greedy_left_to_right() {
        let fixed = 26 + 6 + 10; // model + share + total
        let sp = 1; // TABLE_COLUMN_SPACING

        // All agents fit → all shown at ideal width
        // base = 42 + 2*1 = 44; a(10+1)=55; b(20+1)=76; c(10+1)=87 → need ≥87
        let (agents, widths) = prioritize_agent_columns(
            &[("a", "A"), ("b", "B"), ("c", "C")],
            &[10, 20, 10],
            87,
            fixed,
            sp,
        );
        assert_eq!(agents, vec![("a", "A"), ("b", "B"), ("c", "C")]);
        assert_eq!(widths, vec![10, 20, 10]);

        // Only a fits (inner=55); b would need 55+21=76 >55 → break
        let (agents, widths) = prioritize_agent_columns(
            &[("a", "A"), ("b", "B"), ("c", "C")],
            &[10, 20, 10],
            55,
            fixed,
            sp,
        );
        assert_eq!(agents, vec![("a", "A")]);
        assert_eq!(widths, vec![10]);

        // a + b fit, c doesn't (inner=76; c needs 76+11=87>76)
        let (agents, widths) = prioritize_agent_columns(
            &[("a", "A"), ("b", "B"), ("c", "C")],
            &[10, 20, 10],
            76,
            fixed,
            sp,
        );
        assert_eq!(agents, vec![("a", "A"), ("b", "B")]);
        assert_eq!(widths, vec![10, 20]);

        // Very narrow → no agent columns (inner=44 < 44+11=55)
        let (agents, widths) = prioritize_agent_columns(
            &[("a", "A"), ("b", "B")],
            &[10, 20],
            44,
            fixed,
            sp,
        );
        assert_eq!(agents, Vec::<(&str, &str)>::new());
        assert_eq!(widths, Vec::<u16>::new());

        // Empty input → empty output
        let (agents, widths) = prioritize_agent_columns(
            &[],
            &[],
            100,
            40,
            1,
        );
        assert_eq!(agents, Vec::<(&str, &str)>::new());
        assert_eq!(widths, Vec::<u16>::new());
    }

    // Overview 的 Model 列底纹按 Total tokens 归一化（除以最大值，非总和）。
    // 以下两组测试复刻 draw_model_table 中 ratio -> share_width 的计算公式，
    // 锁定其行为契约：最大模型填满整列、其余按比例、不溢出列宽、空数据安全。
    fn normalized_bar_width(total_tokens: u64, max_total_tokens: u64, col_width: usize) -> usize {
        let ratio = if max_total_tokens > 0 {
            total_tokens as f64 / max_total_tokens as f64
        } else {
            0.0
        };
        (col_width as f64 * ratio).round() as usize
    }

    #[test]
    fn overview_bar_max_normalization_boundary_values() {
        let col_width = 24;
        let max_tokens = 1_000_000u64;

        // 最大模型：ratio 恒为 1.0（同值自除），填满整列
        assert_eq!(
            normalized_bar_width(max_tokens, max_tokens, col_width),
            col_width
        );

        // 半量模型：恰好半宽
        assert_eq!(normalized_bar_width(500_000, max_tokens, col_width), 12);

        // 微量模型：四舍五入到 0
        assert_eq!(normalized_bar_width(1, max_tokens, col_width), 0);

        // 无数据（max_total_tokens = 0）：ratio = 0.0，安全降级为空底纹
        assert_eq!(normalized_bar_width(0, 0, col_width), 0);

        // 即使 total_tokens 等于 max（并列最大）也填满整列，不溢出
        assert_eq!(
            normalized_bar_width(max_tokens, max_tokens, col_width),
            col_width
        );
    }

    #[test]
    fn overview_bar_max_normalization_is_proportional_and_capped() {
        let col_width = 24;
        let tokens = [1000u64, 500, 250, 125]; // max = 1000
        let max_tokens = *tokens.iter().max().unwrap();

        let widths: Vec<usize> = tokens
            .iter()
            .map(|&t| normalized_bar_width(t, max_tokens, col_width))
            .collect();

        // 比例正确：500 恰为 1000 的一半宽度
        assert_eq!(widths, vec![24, 12, 6, 3]);
        assert_eq!(widths[1], widths[0] / 2);

        // 任何模型底纹都不超过列宽（ratio <= 1.0 保证）
        assert!(widths.iter().all(|&w| w <= col_width));
    }
}
