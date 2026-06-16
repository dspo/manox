use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Row, Table};

use super::tui::ProbeApp;
use super::types::{ProbeRow, ProbeStatus};
use crate::WireApi;

pub fn draw(f: &mut ratatui::Frame, app: &mut ProbeApp) {
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
    draw_table(f, chunks[1], app);
    draw_footer(f, chunks[2], app);
}

fn draw_header(f: &mut ratatui::Frame, area: Rect, app: &ProbeApp) {
    let title = if app.is_probing {
        format!(
            " cx probe · 探测中... ({}/{}) ",
            app.completed_count, app.total_count
        )
    } else {
        " cx probe ".to_string()
    };

    let block = Block::default().title(title).borders(Borders::BOTTOM);
    let paragraph = ratatui::widgets::Paragraph::new("").block(block);
    f.render_widget(paragraph, area);
}

fn draw_table(f: &mut ratatui::Frame, area: Rect, app: &ProbeApp) {
    let header = Row::new(vec![
        "Provider",
        "Model",
        "Anthropic Message",
        "OpenAI Responses",
        "OpenAI Completions",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD))
    .height(1);

    let visible_height = (area.height.saturating_sub(2)) as usize;
    let start = app.scroll_offset;
    let end = (app.scroll_offset + visible_height).min(app.rows.len());

    let selected_style = Style::default().add_modifier(Modifier::REVERSED);
    let normal_style = Style::default();

    let rows: Vec<Row> = app.rows[start..end]
        .iter()
        .enumerate()
        .map(|(idx, row)| {
            let actual_idx = start + idx;
            let _style = if actual_idx == app.selected_row {
                selected_style
            } else {
                normal_style
            };

            // 检查该模型是否对所有 wire_api 都失败
            let all_failed = check_all_failed(row);

            let (anthropic_text, anthropic_style) =
                format_cell(row.results.get(&WireApi::Anthropic), app.spinner_tick);
            let (responses_text, responses_style) =
                format_cell(row.results.get(&WireApi::Responses), app.spinner_tick);
            let (completions_text, completions_style) =
                format_cell(row.results.get(&WireApi::Completions), app.spinner_tick);

            // 如果所有 wire_api 都失败，model id 用红色字体
            let model_style = if all_failed {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };

            let row_widget = Row::new(vec![
                Cell::from(row.provider_name.clone()),
                Cell::from(row.model_id.clone()).style(model_style),
                Cell::from(anthropic_text).style(anthropic_style),
                Cell::from(responses_text).style(responses_style),
                Cell::from(completions_text).style(completions_style),
            ]);

            // 只在选中行时应用选中样式，避免覆盖 Cell 级别样式
            if actual_idx == app.selected_row {
                row_widget.style(selected_style)
            } else {
                row_widget
            }
        })
        .collect();

    let widths = [
        Constraint::Min(12),  // Provider
        Constraint::Min(20),  // Model
        Constraint::Min(15),  // Anthropic Message
        Constraint::Min(15),  // OpenAI Responses
        Constraint::Min(15),  // OpenAI Completions
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL));

    f.render_widget(table, area);
}

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn check_all_failed(row: &ProbeRow) -> bool {
    row.results.values().all(|result| {
        matches!(result.status, ProbeStatus::ServerError | ProbeStatus::ClientError)
    })
}

fn format_cell(result: Option<&super::types::ProbeCellResult>, spinner_tick: usize) -> (String, Style) {
    match result {
        Some(result) => {
            match result.status {
                ProbeStatus::Available => {
                    let text = if let Some(latency) = result.latency_ms {
                        format!("{}ms", latency)
                    } else {
                        "可用".to_string()
                    };
                    // 已配置：绿底白字；未配置：绿色文字
                    if result.configured {
                        (text, Style::default().bg(Color::Green).fg(Color::White))
                    } else {
                        (text, Style::default().fg(Color::Green))
                    }
                }
                ProbeStatus::NotApplicable => {
                    ("-".to_string(), Style::default().fg(Color::DarkGray))
                }
                ProbeStatus::ServerError => {
                    let text = if let Some(status) = result.http_status {
                        if let Some(ref msg) = result.error_message {
                            let truncated: String = msg.chars().take(30).collect();
                            format!("🔴 {} | {}", status, truncated)
                        } else {
                            format!("🔴 {}", status)
                        }
                    } else {
                        "🔴 错误".to_string()
                    };
                    if result.configured {
                        (text, Style::default().fg(Color::Red))
                    } else {
                        ("-".to_string(), Style::default().fg(Color::DarkGray))
                    }
                }
                ProbeStatus::ClientError => {
                    let text = if let Some(status) = result.http_status {
                        if let Some(ref msg) = result.error_message {
                            let truncated: String = msg.chars().take(30).collect();
                            format!("🟡 {} | {}", status, truncated)
                        } else {
                            format!("🟡 {}", status)
                        }
                    } else {
                        "🟡 错误".to_string()
                    };
                    if result.configured {
                        (text, Style::default().fg(Color::Yellow))
                    } else {
                        ("-".to_string(), Style::default().fg(Color::DarkGray))
                    }
                }
                ProbeStatus::Probing => {
                    let frame = SPINNER_FRAMES[spinner_tick % SPINNER_FRAMES.len()];
                    (format!("{}", frame), Style::default().fg(Color::Cyan))
                }
                ProbeStatus::Unknown => {
                    if result.configured {
                        ("?".to_string(), Style::default().fg(Color::DarkGray))
                    } else {
                        ("-".to_string(), Style::default().fg(Color::DarkGray))
                    }
                }
            }
        }
        None => ("-".to_string(), Style::default().fg(Color::DarkGray)),
    }
}

fn draw_footer(f: &mut ratatui::Frame, area: Rect, _app: &ProbeApp) {
    let text = "r: 开始探测  ↑↓/jk: 滚动  q/Esc: 退出";
    let paragraph = ratatui::widgets::Paragraph::new(text)
        .style(Style::default().fg(Color::DarkGray));

    f.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::types::{ProbeCellResult, ProbeStatus};

    #[test]
    fn test_check_all_failed() {
        // 全部失败
        let row = ProbeRow {
            provider_name: "test".to_string(),
            model_id: "model".to_string(),
            results: {
                let mut map = std::collections::HashMap::new();
                map.insert(WireApi::Anthropic, ProbeCellResult {
                    status: ProbeStatus::ServerError,
                    latency_ms: None,
                    http_status: Some(500),
                    error_message: None,
                    configured: true,
                });
                map.insert(WireApi::Responses, ProbeCellResult {
                    status: ProbeStatus::ClientError,
                    latency_ms: None,
                    http_status: Some(401),
                    error_message: None,
                    configured: true,
                });
                map
            },
        };
        assert!(check_all_failed(&row));

        // 部分可用
        let row = ProbeRow {
            provider_name: "test".to_string(),
            model_id: "model".to_string(),
            results: {
                let mut map = std::collections::HashMap::new();
                map.insert(WireApi::Anthropic, ProbeCellResult {
                    status: ProbeStatus::Available,
                    latency_ms: Some(100),
                    http_status: Some(200),
                    error_message: None,
                    configured: true,
                });
                map.insert(WireApi::Responses, ProbeCellResult {
                    status: ProbeStatus::ClientError,
                    latency_ms: None,
                    http_status: Some(401),
                    error_message: None,
                    configured: true,
                });
                map
            },
        };
        assert!(!check_all_failed(&row));

        // 全部未知（不算失败）
        let row = ProbeRow {
            provider_name: "test".to_string(),
            model_id: "model".to_string(),
            results: {
                let mut map = std::collections::HashMap::new();
                map.insert(WireApi::Anthropic, ProbeCellResult {
                    status: ProbeStatus::Unknown,
                    latency_ms: None,
                    http_status: None,
                    error_message: None,
                    configured: true,
                });
                map
            },
        };
        assert!(!check_all_failed(&row));
    }

    #[test]
    fn test_format_cell_available() {
        let result = ProbeCellResult {
            status: ProbeStatus::Available,
            latency_ms: Some(150),
            http_status: Some(200),
            error_message: None,
            configured: true,
        };
        let (text, _) = format_cell(Some(&result), 0);
        assert_eq!(text, "150ms");
    }

    #[test]
    fn test_format_cell_not_applicable() {
        let result = ProbeCellResult {
            status: ProbeStatus::NotApplicable,
            latency_ms: None,
            http_status: None,
            error_message: None,
            configured: true,
        };
        let (text, _) = format_cell(Some(&result), 0);
        assert_eq!(text, "-");
    }

    #[test]
    fn test_format_cell_server_error() {
        let result = ProbeCellResult {
            status: ProbeStatus::ServerError,
            latency_ms: None,
            http_status: Some(500),
            error_message: Some("internal error".to_string()),
            configured: true,
        };
        let (text, _) = format_cell(Some(&result), 0);
        assert!(text.contains("🔴"));
        assert!(text.contains("500"));
    }

    #[test]
    fn test_format_cell_client_error() {
        let result = ProbeCellResult {
            status: ProbeStatus::ClientError,
            latency_ms: None,
            http_status: Some(401),
            error_message: Some("unauthorized".to_string()),
            configured: true,
        };
        let (text, _) = format_cell(Some(&result), 0);
        assert!(text.contains("🟡"));
        assert!(text.contains("401"));
    }

    #[test]
    fn test_format_cell_probing() {
        let result = ProbeCellResult {
            status: ProbeStatus::Probing,
            latency_ms: None,
            http_status: None,
            error_message: None,
            configured: true,
        };
        let (text, _) = format_cell(Some(&result), 0);
        assert!(!text.is_empty());
    }

    #[test]
    fn test_format_cell_unknown() {
        let result = ProbeCellResult {
            status: ProbeStatus::Unknown,
            latency_ms: None,
            http_status: None,
            error_message: None,
            configured: true,
        };
        let (text, _) = format_cell(Some(&result), 0);
        assert_eq!(text, "?");
    }

    #[test]
    fn test_format_cell_none() {
        let (text, _) = format_cell(None, 0);
        assert_eq!(text, "-");
    }
}
