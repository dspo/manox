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
use super::tui::StatsApp;
use super::types::{Period, UsageTotals};
use super::{MATRIX_AGENTS, PALETTE};

type ChartSeries = (String, Vec<f64>, Color);
type ChartOccupancy = HashSet<(u16, u16)>;

const MODEL_MIN_WIDTH: u16 = 18;
const SHARE_WIDTH: u16 = 8;
const TOTAL_WIDTH: u16 = 18;
const AGENT_WIDTH: u16 = 18;
const TABLE_COLUMN_SPACING: u16 = 1;
const STRIPED_ROW_BG: Color = Color::Rgb(238, 242, 247);
const STEP_CHART_MAX_WIDTH: u16 = 78;
const STEP_CHART_HEIGHT: u16 = 14;
const Y_TICK_COUNT: usize = 10;
const X_TICK_MIN_COUNT: usize = 6;

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
    let text = format!("{period_hint}   r cycle dates   ↑↓ scroll   q quit");
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

    draw_tokens_per_day_chart(f, chunks[0], app);
    draw_period_switch(f, chunks[1], app);
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

    let mut min_date = app.today.clone();
    let mut max_date = "0000-00-00".to_string();
    for r in &records {
        if r.date < min_date {
            min_date = r.date.clone();
        }
        if r.date > max_date {
            max_date = r.date.clone();
        }
    }
    if max_date == "0000-00-00" {
        return;
    }

    let day_count = (days_diff(&min_date, &max_date).unwrap_or(0).max(0) + 1) as usize;
    let day_count = day_count.max(1);

    // 每个模型每天的 token 数（含 cache 的总量，与 ccusage 对齐）
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
    let plot_right = chart_area.right().saturating_sub(1);
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

    let legend_width = chart_legend_max_width(series)
        .min(plot_right.saturating_sub(plot_left) + 1)
        .max(1);
    let legend_x = plot_right.saturating_add(1).saturating_sub(legend_width);
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

    let boundaries: Vec<u16> = (0..=values.len())
        .map(|idx| chart_x_boundary(idx, day_count, plot_left, plot_right))
        .collect();

    for idx in 0..values.len() {
        let x0 = boundaries[idx];
        let x1 = boundaries[idx + 1];
        let y = rows[idx];
        let previous_y = idx.checked_sub(1).and_then(|previous| rows.get(previous));
        let next_y = rows.get(idx + 1);
        let start = if previous_y.is_some_and(|previous| *previous != y) {
            x0.saturating_add(1)
        } else {
            x0
        };
        let end = if next_y.is_some_and(|next| *next != y) {
            x1.saturating_sub(1)
        } else {
            x1
        };
        draw_horizontal(buf, start, end, y, style, occupied);

        if let Some(&next_y) = next_y {
            if next_y != y {
                draw_rounded_transition(buf, x1, y, next_y, style, occupied);
            }
        }
    }
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
    for (i, p) in [Period::Last7, Period::Last30, Period::All]
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
        spans.push(Span::styled(p.label().to_string(), style));
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
        .block(Block::default().borders(Borders::ALL).title(" Models "));
        f.render_widget(p, area);
        return;
    }

    let mut sorted: Vec<(String, UsageTotals)> = totals.into_iter().collect();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1.total_tokens()));

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
                Cell::from(format!(
                    "↑{} ↓{}",
                    format_tokens(usage.in_tokens),
                    format_tokens(usage.out_tokens)
                )),
            ];
            for (agent, _) in MATRIX_AGENTS {
                let cell = match cells.get(&(agent.to_string(), model.clone())) {
                    Some(usage) => Cell::from(format!(
                        "↑{} ↓{}",
                        format_tokens(usage.in_tokens),
                        format_tokens(usage.out_tokens)
                    )),
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
    .chain(MATRIX_AGENTS.iter().map(|(_, label)| {
        Cell::from(Span::styled(
            *label,
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ))
    }))
    .collect();
    let header = Row::new(header_cells);

    let column_count = (3 + MATRIX_AGENTS.len()) as u16;
    let inner_width = area.width.saturating_sub(2);
    let non_model_width = SHARE_WIDTH + TOTAL_WIDTH + AGENT_WIDTH * MATRIX_AGENTS.len() as u16;
    let spacing_width = TABLE_COLUMN_SPACING * column_count.saturating_sub(1);
    let model_width = inner_width
        .saturating_sub(non_model_width + spacing_width)
        .max(MODEL_MIN_WIDTH);

    let mut widths = vec![
        Constraint::Length(model_width),
        Constraint::Length(SHARE_WIDTH),
        Constraint::Length(TOTAL_WIDTH),
    ];
    widths.extend(
        MATRIX_AGENTS
            .iter()
            .map(|_| Constraint::Length(AGENT_WIDTH)),
    );

    let shown = sorted.len().saturating_sub(app.models_scroll).min(visible);
    let title = format!(" Models · {} of {} ", shown, sorted.len());
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(TABLE_COLUMN_SPACING)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(table, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chart_area_is_capped_for_large_terminals() {
        let area = fixed_chart_area(Rect::new(2, 3, 120, 30));

        assert_eq!(
            area,
            Rect::new(2, 3, STEP_CHART_MAX_WIDTH, STEP_CHART_HEIGHT)
        );
    }

    #[test]
    fn chart_x_boundaries_include_plot_edges() {
        assert_eq!(chart_x_boundary(0, 7, 10, 30), 10);
        assert_eq!(chart_x_boundary(7, 7, 10, 30), 30);
        assert_eq!(chart_x_boundary(99, 7, 10, 30), 30);
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
}
