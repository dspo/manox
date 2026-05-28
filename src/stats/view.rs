//! TUI 渲染：header / footer / Models / Matrix。

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table,
};
use std::collections::HashMap;

use super::aggregate::{top_models_covering, totals_by_agent_model, totals_by_model};
use super::date::{date_offset, days_diff};
use super::format::{format_tokens, short_date};
use super::tui::StatsApp;
use super::types::{Period, UsageTotals, View};
use super::{MATRIX_AGENTS, PALETTE};

type PlotPoint = (f64, f64);
type DatasetData = (String, Vec<PlotPoint>, Color);

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
    match app.view {
        View::Models => draw_models_view(f, chunks[1], app),
        View::Matrix => draw_matrix_view(f, chunks[1], app),
    }
    draw_footer(f, chunks[2], app);
}

fn draw_header(f: &mut ratatui::Frame, area: Rect, app: &StatsApp) {
    let active_style = Style::default()
        .fg(Color::Black)
        .bg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let inactive_style = Style::default().fg(Color::Gray);

    let models_span = if app.view == View::Models {
        Span::styled(" Models ", active_style)
    } else {
        Span::styled(" Models ", inactive_style)
    };
    let matrix_span = if app.view == View::Matrix {
        Span::styled(" Matrix ", active_style)
    } else {
        Span::styled(" Matrix ", inactive_style)
    };

    let title = Line::from(vec![
        Span::styled(
            " cx stats ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("· Token Usage Dashboard   "),
        models_span,
        Span::raw(" "),
        matrix_span,
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
    let text = format!("[Tab] toggle view   {period_hint}   r cycle dates   ↑↓ scroll   q quit");
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
            Constraint::Min(10),
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
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Tokens per Day "),
        );
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
    let mut datasets_data: Vec<DatasetData> = Vec::new();
    for (idx, model) in top_models.iter().enumerate() {
        let color = PALETTE[idx % PALETTE.len()];
        let pts: Vec<PlotPoint> = series
            .get(model)
            .map(|v| {
                v.iter()
                    .enumerate()
                    .map(|(i, &y)| {
                        if y > max_y {
                            max_y = y;
                        }
                        (i as f64, y)
                    })
                    .collect()
            })
            .unwrap_or_default();
        datasets_data.push((model.clone(), pts, color));
    }

    let datasets: Vec<Dataset> = datasets_data
        .iter()
        .map(|(name, data, color)| {
            Dataset::default()
                .name(name.clone())
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(*color))
                .data(data)
        })
        .collect();

    let x_labels: Vec<Span> = if day_count <= 1 {
        vec![Span::raw(min_date.clone())]
    } else {
        let mid_idx = day_count / 2;
        let mid_date = date_offset(&min_date, mid_idx as i64).unwrap_or_else(|_| String::new());
        vec![
            Span::styled(short_date(&min_date), Style::default().fg(Color::DarkGray)),
            Span::styled(short_date(&mid_date), Style::default().fg(Color::DarkGray)),
            Span::styled(short_date(&max_date), Style::default().fg(Color::DarkGray)),
        ]
    };

    let y_max_label = format_tokens(max_y as u64);
    let y_mid_label = format_tokens((max_y / 2.0) as u64);

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Tokens per Day · {} ", app.period.label())),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, (day_count.saturating_sub(1)) as f64])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, max_y * 1.05])
                .labels(vec![
                    Span::styled("0", Style::default().fg(Color::DarkGray)),
                    Span::styled(y_mid_label, Style::default().fg(Color::DarkGray)),
                    Span::styled(y_max_label, Style::default().fg(Color::DarkGray)),
                ]),
        );

    f.render_widget(chart, area);
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

    let visible = area.height.saturating_sub(2) as usize;
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
            Row::new(vec![
                Cell::from(Span::styled("●", Style::default().fg(dot_color))),
                Cell::from(Span::styled(
                    model.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
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
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(2),
        Constraint::Length(28),
        Constraint::Length(8),
        Constraint::Length(24),
    ];

    let shown = sorted.len().saturating_sub(app.models_scroll).min(visible);
    let title = format!(" Models · {} of {} ", shown, sorted.len());
    let table = Table::new(rows, widths).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(table, area);
}

fn draw_matrix_view(f: &mut ratatui::Frame, area: Rect, app: &mut StatsApp) {
    let records = app.period_records();
    let cells = totals_by_agent_model(&records);

    let mut model_totals: HashMap<String, u64> = HashMap::new();
    for ((_, model), usage) in &cells {
        *model_totals.entry(model.clone()).or_insert(0) += usage.total_tokens();
    }
    let mut models: Vec<(String, u64)> = model_totals
        .into_iter()
        .filter(|(_, total)| *total > 0)
        .collect();
    models.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let models: Vec<String> = models.into_iter().map(|(m, _)| m).collect();

    let visible = area.height.saturating_sub(4) as usize;
    let max_scroll = models.len().saturating_sub(visible.max(1));
    if app.matrix_scroll > max_scroll {
        app.matrix_scroll = max_scroll;
    }

    let header_cells: Vec<Cell> = std::iter::once(Cell::from(Span::styled(
        "model",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )))
    .chain(MATRIX_AGENTS.iter().map(|(_, label)| {
        Cell::from(Span::styled(
            *label,
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ))
    }))
    .collect();
    let header = Row::new(header_cells).style(Style::default().bg(Color::Reset));

    let rows: Vec<Row> = models
        .iter()
        .skip(app.matrix_scroll)
        .take(visible)
        .map(|model| {
            let mut row_cells: Vec<Cell> = Vec::with_capacity(MATRIX_AGENTS.len() + 1);
            row_cells.push(Cell::from(Span::styled(
                model.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )));
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
            Row::new(row_cells)
        })
        .collect();

    let mut widths = vec![Constraint::Length(28)];
    widths.extend(MATRIX_AGENTS.iter().map(|_| Constraint::Length(20)));

    let title = format!(
        " Agent × Model · {} ({}/{}) ",
        app.period.label(),
        models.len().min(app.matrix_scroll + visible),
        models.len()
    );
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(table, area);
}
