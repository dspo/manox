//! TUI 渲染：header / footer / Models。

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::collections::{HashMap, HashSet};

use super::aggregate::{top_models_covering, totals_by_agent_model, totals_by_model};
use super::date::{date_offset, days_diff};
use super::format::{format_tokens, short_date};
use super::tui::{ChartTab, FocusArea, StatsApp};
use super::types::{Period, UsageRecord, UsageTotals};
use super::{MATRIX_AGENTS, PALETTE};

type ChartSeries = (String, Vec<f64>, Color);
type ChartOccupancy = HashSet<(u16, u16)>;

const MODEL_MIN_WIDTH: u16 = 18;
const SHARE_WIDTH: u16 = 6;
const TABLE_COLUMN_SPACING: u16 = 1;
const STRIPED_ROW_BG: Color = Color::Rgb(238, 242, 247);
const STEP_CHART_MAX_WIDTH: u16 = 78;
const STEP_CHART_HEIGHT: u16 = 14;
const Y_TICK_COUNT: usize = 10;
const X_TICK_MIN_COUNT: usize = 6;
const RACE_VISIBLE_MODELS: usize = 10;
const RACE_TWEEN_STEPS: usize = 12;

#[derive(Debug, Clone)]
struct RaceEntry {
    model: String,
    value: u64,
    color: Color,
}

#[derive(Debug, Clone)]
struct RaceFrame {
    date: String,
    entries: Vec<RaceEntry>,
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

fn draw_header(f: &mut ratatui::Frame, area: Rect, _app: &StatsApp) {
    let title = Line::from(vec![
        Span::styled(
            " cx stats ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("· Token Usage Dashboard   "),
        Span::styled(
            " Models ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let block = Block::default().borders(Borders::BOTTOM);
    let p = Paragraph::new(title).block(block);
    f.render_widget(p, area);
}

fn draw_footer(f: &mut ratatui::Frame, area: Rect, app: &StatsApp) {
    let period_hint = match app.period {
        Period::Last7 => "[1] 7d  2 30d  3 All",
        Period::Last30 => "1 7d  [2] 30d  3 All",
        Period::All => "1 7d  2 30d  [3] All",
    };
    let focus_hint = match app.focus {
        FocusArea::Models => "↓ tabs   j/k scroll",
        FocusArea::ChartTabs => "↑ models   Tab switch view",
    };
    let text = format!("{period_hint}   r cycle dates   {focus_hint}   q quit");
    let p = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(p, area);
}

fn draw_models_view(f: &mut ratatui::Frame, area: Rect, app: &mut StatsApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(STEP_CHART_HEIGHT),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);

    match app.chart_tab {
        ChartTab::Overview => draw_tokens_per_day_chart(f, chunks[0], app),
        ChartTab::Funview => draw_bar_chart_race(f, chunks[0], app),
    }
    draw_chart_tabs(f, chunks[1], app);
    draw_model_list(f, chunks[2], app);
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
        app.period.label(),
        &min_date,
        &max_date,
        day_count,
        max_y,
        &chart_series,
    );
}

fn draw_bar_chart_race(f: &mut ratatui::Frame, area: Rect, app: &StatsApp) {
    let chart_area = fixed_chart_area(area);
    if chart_area.width < 32 || chart_area.height < 6 {
        f.render_widget(Paragraph::new("Model Tokens Race · All time"), chart_area);
        return;
    }

    let frames = race_frames(&app.records);
    if frames.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "  No data for bar chart race.",
            Style::default().fg(Color::DarkGray),
        )))
        .block(Block::default().title(" Model Tokens Race · All time "));
        f.render_widget(p, chart_area);
        return;
    }

    let current_idx = race_frame_index(app.race_tick, frames.len());
    let previous_idx = current_idx.saturating_sub(1);
    let current = &frames[current_idx];
    let previous = &frames[previous_idx];
    let tween = race_tween(app.race_tick);
    let max_value = race_max_value(&frames);

    draw_race_frame(f, chart_area, previous, current, tween, max_value);
}

fn draw_race_frame(
    f: &mut ratatui::Frame,
    chart_area: Rect,
    previous: &RaceFrame,
    current: &RaceFrame,
    tween: f64,
    max_value: u64,
) {
    let title = Line::from(Span::styled(
        " Model Tokens Race · All time ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    f.render_widget(
        Paragraph::new(title),
        Rect::new(chart_area.x, chart_area.y, chart_area.width, 1),
    );

    let date_line = Line::from(vec![
        Span::styled(" Date ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            short_date(&current.date),
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(
        Paragraph::new(date_line),
        Rect::new(chart_area.x, chart_area.y + 1, chart_area.width, 1),
    );

    if current.entries.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  Waiting for the first model token usage...",
                Style::default().fg(Color::DarkGray),
            ))),
            Rect::new(
                chart_area.x,
                chart_area.y + 3,
                chart_area.width,
                chart_area.height.saturating_sub(3),
            ),
        );
        return;
    }

    let row_count = RACE_VISIBLE_MODELS
        .min(current.entries.len())
        .min(chart_area.height.saturating_sub(3) as usize);
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
        .clamp(10, 22);
    let value_width = current
        .entries
        .iter()
        .take(row_count)
        .map(|entry| text_width(&format_tokens(entry.value)))
        .max()
        .unwrap_or(4)
        .max(text_width(&format_tokens(max_value)))
        .max(4);

    let bar_left = chart_area.x + model_width + 2;
    let bar_right = chart_area
        .right()
        .saturating_sub(value_width)
        .saturating_sub(3);
    if bar_left >= bar_right {
        return;
    }

    let plot_top = chart_area.y + 3;
    let plot_bottom = plot_top + row_count as u16 - 1;
    let previous_ranks = race_rank_map(previous);
    let previous_values = race_value_map(previous);
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

        let previous_value = previous_values.get(&entry.model).copied().unwrap_or(0);
        let value = interpolate_u64(previous_value, entry.value, eased);
        let bar_len = ((value as f64 / max_value.max(1) as f64) * f64::from(bar_width))
            .round()
            .max(if value > 0 { 1.0 } else { 0.0 }) as u16;
        let label = truncate_text(&entry.model, model_width);
        let value_label = format_tokens(value);
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
    period_label: &str,
    min_date: &str,
    max_date: &str,
    day_count: usize,
    max_y: f64,
    series: &[ChartSeries],
) {
    let chart_area = fixed_chart_area(area);
    if chart_area.width < 24 || chart_area.height < 6 {
        f.render_widget(
            Paragraph::new(format!("Tokens per Day · {period_label}")),
            chart_area,
        );
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
        format!(" Tokens per Day · {period_label} "),
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
        Period::Last7 => Some((date_offset(today, -6).ok()?, today.to_string())),
        Period::Last30 => Some((date_offset(today, -29).ok()?, today.to_string())),
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

fn race_frames(records: &[UsageRecord]) -> Vec<RaceFrame> {
    let Some((min_date, max_date)) = all_time_date_range(records) else {
        return Vec::new();
    };
    let day_count = (days_diff(&min_date, &max_date).unwrap_or(0).max(0) + 1) as usize;
    let color_map = race_color_map(records);
    let mut deltas_by_date: HashMap<String, HashMap<String, u64>> = HashMap::new();
    for record in records {
        let tokens = record.in_tokens.saturating_add(record.out_tokens);
        *deltas_by_date
            .entry(record.date.clone())
            .or_default()
            .entry(record.model.clone())
            .or_default() += tokens;
    }

    let mut cumulative: HashMap<String, u64> = HashMap::new();
    let mut snapshots: Vec<(usize, HashMap<String, u64>)> = Vec::new();
    for day_idx in 0..day_count {
        let date = date_for_day(&min_date, &max_date, day_idx, day_count);
        if let Some(deltas) = deltas_by_date.get(&date) {
            for (model, tokens) in deltas {
                *cumulative.entry(model.clone()).or_default() += *tokens;
            }
            snapshots.push((day_idx, cumulative.clone()));
        }
    }

    let mut frames = Vec::with_capacity(day_count);
    for day_idx in 0..day_count {
        let date = date_for_day(&min_date, &max_date, day_idx, day_count);
        let values = interpolated_race_values(day_idx, &snapshots);
        let entries = race_entries(values, &color_map);
        frames.push(RaceFrame { date, entries });
    }
    frames
}

fn date_for_day(min_date: &str, max_date: &str, day_idx: usize, day_count: usize) -> String {
    if day_idx + 1 == day_count {
        max_date.to_string()
    } else {
        date_offset(min_date, day_idx as i64).unwrap_or_else(|_| min_date.to_string())
    }
}

fn interpolated_race_values(
    day_idx: usize,
    snapshots: &[(usize, HashMap<String, u64>)],
) -> HashMap<String, u64> {
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
        if (*previous_idx..=*next_idx).contains(&day_idx) {
            if day_idx == *next_idx {
                return next_values.clone();
            }
            let span = (*next_idx - *previous_idx).max(1) as f64;
            let tween = (day_idx - *previous_idx) as f64 / span;
            return interpolate_race_values(previous_values, next_values, tween);
        }
    }

    snapshots
        .last()
        .map(|(_, values)| values.clone())
        .unwrap_or_default()
}

fn interpolate_race_values(
    previous: &HashMap<String, u64>,
    next: &HashMap<String, u64>,
    tween: f64,
) -> HashMap<String, u64> {
    let models: HashSet<&String> = previous.keys().chain(next.keys()).collect();
    models
        .into_iter()
        .map(|model| {
            let from = previous.get(model).copied().unwrap_or(0);
            let to = next.get(model).copied().unwrap_or(0);
            (model.clone(), interpolate_u64(from, to, tween))
        })
        .collect()
}

fn race_entries(
    values: HashMap<String, u64>,
    color_map: &HashMap<String, Color>,
) -> Vec<RaceEntry> {
    let mut entries: Vec<RaceEntry> = values
        .into_iter()
        .filter(|(_, value)| *value > 0)
        .map(|(model, value)| RaceEntry {
            color: color_map.get(&model).copied().unwrap_or(Color::White),
            model,
            value,
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

fn all_time_date_range(records: &[UsageRecord]) -> Option<(String, String)> {
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

fn race_frame_index(tick: usize, frame_count: usize) -> usize {
    if frame_count == 0 {
        0
    } else {
        (tick / RACE_TWEEN_STEPS) % frame_count
    }
}

fn race_tween(tick: usize) -> f64 {
    (tick % RACE_TWEEN_STEPS) as f64 / RACE_TWEEN_STEPS as f64
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

fn race_value_map(frame: &RaceFrame) -> HashMap<String, u64> {
    frame
        .entries
        .iter()
        .map(|entry| (entry.model.clone(), entry.value))
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

fn draw_chart_tabs(f: &mut ratatui::Frame, area: Rect, app: &StatsApp) {
    let mut spans: Vec<Span> = Vec::new();
    if app.focus == FocusArea::ChartTabs {
        spans.push(Span::styled("▸ ", Style::default().fg(Color::LightCyan)));
    } else {
        spans.push(Span::raw("  "));
    }

    for (i, tab) in [ChartTab::Overview, ChartTab::Funview].iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" · "));
        }
        let style = if app.chart_tab == *tab {
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(tab.label(), style));
    }

    if app.chart_tab == ChartTab::Overview {
        spans.push(Span::styled(
            format!("   ·   {}", app.period.label()),
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        spans.push(Span::styled(
            "   ·   All time cumulative tokens",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let p = Paragraph::new(Line::from(spans));
    f.render_widget(p, area);
}

fn draw_model_list(f: &mut ratatui::Frame, area: Rect, app: &mut StatsApp) {
    let records = app.period_records();
    let cells = totals_by_agent_model(&records);
    let totals = totals_by_model(&records);
    let total_all: u64 = totals.values().map(|usage| usage.total_tokens()).sum();

    if totals.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "  No models in selected period.",
            Style::default().fg(Color::DarkGray),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(model_list_title(app.focus, "Models".to_string())),
        );
        f.render_widget(p, area);
        return;
    }

    let mut sorted: Vec<(String, UsageTotals)> = totals.into_iter().collect();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1.total_tokens()));
    let agent_columns = sorted_agents_by_usage(&cells);

    let visible = area.height.saturating_sub(3) as usize;
    let max_scroll = sorted.len().saturating_sub(visible.max(1));
    if app.models_scroll > max_scroll {
        app.models_scroll = max_scroll;
    }

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
            let dot_color = PALETTE[idx % PALETTE.len()];
            let mut row_cells = vec![
                Cell::from(Span::styled(
                    model.clone(),
                    Style::default().fg(dot_color).add_modifier(Modifier::BOLD),
                )),
                Cell::from(Span::styled(
                    format!("{:.1}%", pct),
                    Style::default().fg(Color::DarkGray),
                )),
                Cell::from(usage_cell_text(usage)),
            ];
            for (agent, _) in &agent_columns {
                let cell = match cells.get(&(agent.to_string(), model.clone())) {
                    Some(usage) => Cell::from(usage_cell_text(usage)),
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

    let header_cells: Vec<Cell> = [
        Cell::from(Span::styled(
            "Model",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Cell::from(Span::styled(
            "Share",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Cell::from(Span::styled(
            "Total",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
    ]
    .into_iter()
    .chain(agent_columns.iter().map(|(_, label)| {
        Cell::from(Span::styled(
            *label,
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ))
    }))
    .collect();
    let header = Row::new(header_cells);

    let widths = model_table_widths(area.width, &sorted, &cells, &agent_columns);

    let shown = sorted.len().saturating_sub(app.models_scroll).min(visible);
    let title = model_list_title(app.focus, format!("Models · {} of {}", shown, sorted.len()));
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(TABLE_COLUMN_SPACING)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(table, area);
}

fn model_list_title(focus: FocusArea, title: String) -> String {
    if focus == FocusArea::Models {
        format!(" ▸ {title} ")
    } else {
        format!(" {title} ")
    }
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

fn model_table_widths(
    area_width: u16,
    sorted: &[(String, UsageTotals)],
    cells: &HashMap<(String, String), UsageTotals>,
    agent_columns: &[(&'static str, &'static str)],
) -> Vec<Constraint> {
    let total_width = usage_column_width("Total", sorted.iter().map(|(_, usage)| usage));
    let agent_widths: Vec<u16> = agent_columns
        .iter()
        .map(|(agent, label)| {
            let agent = (*agent).to_string();
            let usages = sorted
                .iter()
                .filter_map(|(model, _)| cells.get(&(agent.clone(), model.clone())));
            usage_column_width(label, usages)
        })
        .collect();

    let column_count = (3 + agent_columns.len()) as u16;
    let inner_width = area_width.saturating_sub(2);
    let non_model_width = SHARE_WIDTH + total_width + agent_widths.iter().sum::<u16>();
    let spacing_width = TABLE_COLUMN_SPACING * column_count.saturating_sub(1);
    let model_width = inner_width
        .saturating_sub(non_model_width + spacing_width)
        .max(MODEL_MIN_WIDTH);

    let mut widths = vec![
        Constraint::Length(model_width),
        Constraint::Length(SHARE_WIDTH),
        Constraint::Length(total_width),
    ];
    widths.extend(agent_widths.into_iter().map(Constraint::Length));
    widths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constraint_length(constraint: Constraint) -> u16 {
        match constraint {
            Constraint::Length(width) => width,
            other => panic!("expected length constraint, got {other:?}"),
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
        UsageRecord {
            agent: "claude".to_string(),
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
    fn race_frames_interpolate_empty_days_between_cumulative_snapshots() {
        let records = vec![
            record("alpha", "2026-05-27", 100, 20),
            record("beta", "2026-05-29", 200, 0),
        ];

        let frames = race_frames(&records);

        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].date, "2026-05-27");
        assert_eq!(frames[0].entries[0].model, "alpha");
        assert_eq!(frames[0].entries[0].value, 120);
        assert_eq!(frames[1].date, "2026-05-28");
        assert_eq!(frames[1].entries[0].model, "alpha");
        assert_eq!(frames[1].entries[0].value, 120);
        assert_eq!(frames[1].entries[1].model, "beta");
        assert_eq!(frames[1].entries[1].value, 100);
        assert_eq!(frames[2].date, "2026-05-29");
        assert_eq!(frames[2].entries[0].model, "beta");
        assert_eq!(frames[2].entries[0].value, 200);
        assert_eq!(frames[2].entries[1].model, "alpha");
        assert_eq!(frames[2].entries[1].value, 120);
    }

    #[test]
    fn race_frames_keep_only_top_ten_models() {
        let records: Vec<UsageRecord> = (0..12)
            .map(|idx| record(&format!("model-{idx:02}"), "2026-05-29", idx + 1, 0))
            .collect();

        let frames = race_frames(&records);
        let models: Vec<&str> = frames[0]
            .entries
            .iter()
            .map(|entry| entry.model.as_str())
            .collect();

        assert_eq!(models.len(), RACE_VISIBLE_MODELS);
        assert_eq!(models.first(), Some(&"model-11"));
        assert_eq!(models.last(), Some(&"model-02"));
    }

    #[test]
    fn race_max_value_uses_global_final_scale() {
        let records = vec![
            record("alpha", "2026-05-27", 100, 0),
            record("beta", "2026-05-28", 1000, 0),
        ];

        let frames = race_frames(&records);

        assert_eq!(frames[0].entries[0].value, 100);
        assert_eq!(race_max_value(&frames), 1000);
    }

    #[test]
    fn race_frame_index_advances_by_tween_steps() {
        assert_eq!(race_frame_index(0, 3), 0);
        assert_eq!(race_frame_index(RACE_TWEEN_STEPS - 1, 3), 0);
        assert_eq!(race_frame_index(RACE_TWEEN_STEPS, 3), 1);
        assert_eq!(race_frame_index(RACE_TWEEN_STEPS * 3, 3), 0);
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

        let agents = sorted_agents_by_usage(&cells);

        assert_eq!(
            agents,
            vec![
                ("codex", "Codex"),
                ("copilot", "Copilot"),
                ("claude", "Claude Code"),
                ("zed", "Zed Agent"),
            ]
        );
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
        let agent_columns = sorted_agents_by_usage(&cells);

        let widths = model_table_widths(103, &sorted, &cells, &agent_columns);

        assert!(constraint_length(widths[0]) >= 20);
        assert_eq!(constraint_length(widths[1]), SHARE_WIDTH);
        assert_eq!(constraint_length(widths[2]), text_width("↑174.4m ↓547.9k"));
        assert_eq!(constraint_length(widths[5]), text_width("Codex"));
        assert_eq!(constraint_length(widths[6]), text_width("Zed Agent"));
    }
}
