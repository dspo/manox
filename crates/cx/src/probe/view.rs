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

    // 按 provider 首次出现顺序编号，用于斑马纹分组（rows 已按 provider 排序，行连续）。
    let mut provider_group: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    let mut next_group = 0usize;
    for row in &app.rows {
        provider_group
            .entry(row.provider_name.as_str())
            .or_insert_with(|| {
                let idx = next_group;
                next_group += 1;
                idx
            });
    }

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

            // 该 provider 的斑马纹底色（白/浅灰交替），贯穿整行。
            let group_idx = provider_group
                .get(row.provider_name.as_str())
                .copied()
                .unwrap_or(0);
            let tint = zebra_tint(group_idx);

            // 浅底上文字统一用黑色前景；全部失败时 model 仍用红色突出。
            let provider_cell_style = Style::default().fg(Color::Black);
            let model_style = if all_failed {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Black)
            };

            let row_widget = Row::new(vec![
                Cell::from(row.provider_name.clone()).style(provider_cell_style),
                Cell::from(row.model_id.clone()).style(model_style),
                Cell::from(anthropic_text).style(anthropic_style),
                Cell::from(responses_text).style(responses_style),
                Cell::from(completions_text).style(completions_style),
            ]);

            // 选中行整行反色高亮；非选中行整行套斑马纹底色，各 Cell 仅设前景色，
            // 底色由行级 tint 贯穿透出。
            if actual_idx == app.selected_row {
                row_widget.style(selected_style)
            } else {
                row_widget.style(normal_style.bg(tint))
            }
        })
        .collect();

    let widths = [
        Constraint::Min(12), // Provider
        Constraint::Min(20), // Model
        Constraint::Min(15), // Anthropic Message
        Constraint::Min(15), // OpenAI Responses
        Constraint::Min(15), // OpenAI Completions
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL));

    f.render_widget(table, area);
}

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// provider 分组斑马纹的两种浅色底色：白与浅灰交替。
const ZEBRA_TINTS: &[Color] = &[
    Color::Rgb(250, 250, 250), // 近白
    Color::Rgb(224, 224, 224), // 浅灰
];

/// 按 provider 分组序号选择斑马纹底色，相邻 provider 在白/浅灰间交替，
/// 同一 provider 的所有行底色一致，便于区分。
fn zebra_tint(provider_group_idx: usize) -> Color {
    ZEBRA_TINTS[provider_group_idx % ZEBRA_TINTS.len()]
}

fn check_all_failed(row: &ProbeRow) -> bool {
    row.results.values().all(|result| {
        matches!(
            result.status,
            ProbeStatus::ServerError | ProbeStatus::ClientError
        )
    })
}

fn format_cell(
    result: Option<&super::types::ProbeCellResult>,
    spinner_tick: usize,
) -> (String, Style) {
    match result {
        Some(result) => {
            match result.status {
                ProbeStatus::Available => {
                    let text = if let Some(latency) = result.latency_ms {
                        format!("{}ms", latency)
                    } else {
                        "可用".to_string()
                    };
                    // 浅底斑马纹上用深绿字表示可用（不再用绿底，避免盖掉行底色）；
                    // 未配置用稍浅的绿字区分。
                    if result.configured {
                        (
                            text,
                            Style::default()
                                .fg(Color::Rgb(0, 128, 0))
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        (text, Style::default().fg(Color::Rgb(60, 140, 60)))
                    }
                }
                ProbeStatus::NotApplicable => (
                    "-".to_string(),
                    Style::default().fg(Color::Rgb(120, 120, 120)),
                ),
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
                        (text, Style::default().fg(Color::Rgb(176, 0, 0)))
                    } else {
                        (
                            "-".to_string(),
                            Style::default().fg(Color::Rgb(120, 120, 120)),
                        )
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
                        (text, Style::default().fg(Color::Rgb(150, 110, 0)))
                    } else {
                        (
                            "-".to_string(),
                            Style::default().fg(Color::Rgb(120, 120, 120)),
                        )
                    }
                }
                ProbeStatus::Probing => {
                    let frame = SPINNER_FRAMES[spinner_tick % SPINNER_FRAMES.len()];
                    (
                        format!("{}", frame),
                        Style::default().fg(Color::Rgb(0, 120, 120)),
                    )
                }
                ProbeStatus::Unknown => {
                    if result.configured {
                        (
                            "?".to_string(),
                            Style::default().fg(Color::Rgb(120, 120, 120)),
                        )
                    } else {
                        (
                            "-".to_string(),
                            Style::default().fg(Color::Rgb(120, 120, 120)),
                        )
                    }
                }
            }
        }
        None => (
            "-".to_string(),
            Style::default().fg(Color::Rgb(120, 120, 120)),
        ),
    }
}

fn draw_footer(f: &mut ratatui::Frame, area: Rect, _app: &ProbeApp) {
    let text = "r: 开始探测  ↑↓/jk: 滚动  q/Esc: 退出";
    let paragraph =
        ratatui::widgets::Paragraph::new(text).style(Style::default().fg(Color::DarkGray));

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
                map.insert(
                    WireApi::Anthropic,
                    ProbeCellResult {
                        status: ProbeStatus::ServerError,
                        latency_ms: None,
                        http_status: Some(500),
                        error_message: None,
                        configured: true,
                    },
                );
                map.insert(
                    WireApi::Responses,
                    ProbeCellResult {
                        status: ProbeStatus::ClientError,
                        latency_ms: None,
                        http_status: Some(401),
                        error_message: None,
                        configured: true,
                    },
                );
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
                map.insert(
                    WireApi::Anthropic,
                    ProbeCellResult {
                        status: ProbeStatus::Available,
                        latency_ms: Some(100),
                        http_status: Some(200),
                        error_message: None,
                        configured: true,
                    },
                );
                map.insert(
                    WireApi::Responses,
                    ProbeCellResult {
                        status: ProbeStatus::ClientError,
                        latency_ms: None,
                        http_status: Some(401),
                        error_message: None,
                        configured: true,
                    },
                );
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
                map.insert(
                    WireApi::Anthropic,
                    ProbeCellResult {
                        status: ProbeStatus::Unknown,
                        latency_ms: None,
                        http_status: None,
                        error_message: None,
                        configured: true,
                    },
                );
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

    #[test]
    fn test_zebra_tint_alternates() {
        // 相邻分组在两色间交替，同序号稳定
        assert_eq!(zebra_tint(0), zebra_tint(0));
        assert_eq!(zebra_tint(0), zebra_tint(2));
        assert_eq!(zebra_tint(1), zebra_tint(3));
        assert_ne!(zebra_tint(0), zebra_tint(1));
    }

    #[test]
    fn test_zebra_tint_within_palette() {
        // 任意分组序号都落在调色板内，不会越界 panic
        for idx in [0, 1, 5, 42, usize::MAX] {
            assert!(ZEBRA_TINTS.contains(&zebra_tint(idx)));
        }
    }
}
