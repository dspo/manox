//! SVG 模型表格渲染 — Overview 视图下方专业数据表。
//!
//! 用 SVG `<g>` 分组逐行渲染，每行含模型名、占比条形（在 Model 列下方）、
//! 占比百分比、总量、各 agent 分项。
//!
//! 与 TUI 一致的设计：
//! - Share 占比条形在 Model 列下方（而非单独 Share 列下）
//! - 条形宽度按 Total tokens / max_total_tokens 归一化（最大模型填满 Model 列）
//! - Share 列仅显示百分比文字

use super::format::{format_share, format_tokens};
use super::overview::OverviewTableData;
use super::palette::{BORDER, DIM, MONO, SANS, STRIPED, SVG_COLORS, TEXT, TITLE};
use super::types::UsageTotals;

/// Overview 模型表格入口。
pub(super) fn model_table(data: &OverviewTableData, table_bounds: (f64, f64, f64)) -> String {
    render_table(data, table_bounds)
}

/// 表格列定义常量。
const ROW_HEIGHT: f64 = 28.0;
const HEADER_HEIGHT: f64 = 24.0;
const COL_GAP: f64 = 14.0;
const MODEL_COL_WIDTH: f64 = 220.0;
const SHARE_COL_WIDTH: f64 = 56.0;
const TOTAL_COL_WIDTH: f64 = 100.0;
const AGENT_COL_MIN_WIDTH: f64 = 80.0;
const FONT_SIZE_HEADER: f64 = 11.0;
const FONT_SIZE_ROW: f64 = 11.0;
const FONT_SIZE_NAME: f64 = 12.0;
const BAR_HEIGHT: f64 = 18.0;
const BAR_RX: f64 = 6.0;
const ROW_RX: f64 = 4.0;
const PAD_LEFT: f64 = 12.0;
/// 表格右边留出的视觉 padding（px）——确保背景矩形覆盖最后一列文字。
const TABLE_RIGHT_PAD: f64 = 16.0;
const IMAGE_TOP_AGENT_COLUMNS: usize = 3;

/// 渲染模型表格为 SVG `<g>` 元素。
///
pub(super) fn render_table(data: &OverviewTableData, table_bounds: (f64, f64, f64)) -> String {
    let (tx, ty, available_w) = table_bounds;
    let rows = &data.rows;
    let cells = &data.cells;
    let agents = displayed_agent_columns(&data.agent_columns);
    let total_all = data.total_all;

    // ── 布局计算 ────────────────────────────────────────
    // 固定列：Model | Share | Total
    let fixed_width = MODEL_COL_WIDTH + COL_GAP + SHARE_COL_WIDTH + COL_GAP + TOTAL_COL_WIDTH;

    // agent 列宽度：均匀分配剩余空间
    let remaining = available_w - fixed_width - PAD_LEFT - TABLE_RIGHT_PAD;
    let agent_count = agents.len().max(1);
    let agent_col_width = if remaining > 0.0 {
        let w = (remaining - COL_GAP * (agent_count as f64 - 1.0)) / agent_count as f64;
        w.max(AGENT_COL_MIN_WIDTH)
    } else {
        AGENT_COL_MIN_WIDTH
    };

    // 实际表格宽度 = 所有列占据的总宽度（用于背景矩形）
    let actual_table_w =
        fixed_width + PAD_LEFT + TABLE_RIGHT_PAD + agent_count as f64 * (agent_col_width + COL_GAP)
            - COL_GAP;

    // 各列 x 坐标
    let x_model = tx + PAD_LEFT;
    let x_model_center = x_model + MODEL_COL_WIDTH / 2.0;
    let x_share = x_model + MODEL_COL_WIDTH + COL_GAP;
    let x_share_center = x_share + SHARE_COL_WIDTH / 2.0;
    let x_total = x_share + SHARE_COL_WIDTH + COL_GAP;
    let x_total_end = x_total + TOTAL_COL_WIDTH;
    let x_total_center = x_total + TOTAL_COL_WIDTH / 2.0;
    let agent_x: Vec<f64> = agents
        .iter()
        .enumerate()
        .map(|(i, _)| x_total_end + COL_GAP + i as f64 * (agent_col_width + COL_GAP))
        .collect();
    // 每个 agent 列文字右对齐到列右端
    let agent_text_x: Vec<f64> = agent_x.iter().map(|x| x + agent_col_width).collect();
    let agent_center_x: Vec<f64> = agent_x.iter().map(|x| x + agent_col_width / 2.0).collect();

    // ── 条形图归一化基准 ──────────────────────────────
    // 与 TUI 一致：条形宽度按 Total tokens / max_total_tokens 归一化
    let max_total: u64 = rows
        .iter()
        .map(|row| row.usage.total_tokens())
        .max()
        .unwrap_or(0);

    let mut svg = String::new();

    let header_y = ty;

    // ── 表头背景条 ──（覆盖实际宽度）
    svg.push_str(&format!(
        "<rect x=\"{tx:.1}\" y=\"{header_y:.1}\" width=\"{actual_table_w:.1}\" \
         height=\"{HEADER_HEIGHT:.1}\" rx=\"{ROW_RX}\" fill=\"{BORDER}\" opacity=\"0.15\"/>\n"
    ));

    let header_text_y = header_y + HEADER_HEIGHT / 2.0 + FONT_SIZE_HEADER * 0.35;

    // ── 表头文字 ──
    // Model
    svg.push_str(&format!(
        "<text x=\"{x_model_center:.1}\" y=\"{header_text_y:.1}\" \
         font-family=\"{SANS}\" font-size=\"{FONT_SIZE_HEADER}\" \
         font-weight=\"600\" fill=\"{DIM}\" text-anchor=\"middle\">Model</text>\n"
    ));
    // Share
    svg.push_str(&format!(
        "<text x=\"{x_share_center:.1}\" y=\"{header_text_y:.1}\" \
         font-family=\"{SANS}\" font-size=\"{FONT_SIZE_HEADER}\" \
         font-weight=\"600\" fill=\"{DIM}\" text-anchor=\"middle\">Share</text>\n"
    ));
    // Total
    svg.push_str(&format!(
        "<text x=\"{x_total_center:.1}\" y=\"{header_text_y:.1}\" \
         font-family=\"{SANS}\" font-size=\"{FONT_SIZE_HEADER}\" \
         font-weight=\"600\" fill=\"{DIM}\" text-anchor=\"middle\">Total</text>\n"
    ));
    // Agent headers
    for (i, agent) in agents.iter().enumerate() {
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{header_text_y:.1}\" \
             font-family=\"{SANS}\" font-size=\"{FONT_SIZE_HEADER}\" \
             font-weight=\"600\" fill=\"{DIM}\" text-anchor=\"middle\">{}</text>\n",
            agent_center_x[i], agent.label
        ));
    }

    let data_start_y = header_y + HEADER_HEIGHT + 4.0;

    // ── 数据行 ──
    for (row_idx, row) in rows.iter().enumerate() {
        let row_y = data_start_y + row_idx as f64 * ROW_HEIGHT;
        let row_mid_y = row_y + ROW_HEIGHT / 2.0 + FONT_SIZE_ROW * 0.35;
        let model_text_y = row_y + ROW_HEIGHT / 2.0 + FONT_SIZE_NAME * 0.35;
        let bar_y = row_y + (ROW_HEIGHT - BAR_HEIGHT) / 2.0;
        let color = SVG_COLORS[row.color_index % SVG_COLORS.len()];
        let model_name = &row.model;
        let usage = row.usage;

        // ── 条纹行背景 ──（覆盖实际宽度）
        if row_idx % 2 == 1 {
            svg.push_str(&format!(
                "<rect x=\"{tx:.1}\" y=\"{row_y:.1}\" width=\"{actual_table_w:.1}\" \
                 height=\"{ROW_HEIGHT:.1}\" rx=\"{ROW_RX}\" fill=\"{STRIPED}\" opacity=\"0.5\"/>\n"
            ));
        }

        // ── 模型颜色微底纹 ──（覆盖实际宽度）
        svg.push_str(&format!(
            "<rect x=\"{tx:.1}\" y=\"{row_y:.1}\" width=\"{actual_table_w:.1}\" \
             height=\"{ROW_HEIGHT:.1}\" rx=\"{ROW_RX}\" fill=\"{color}\" opacity=\"0.08\"/>\n"
        ));

        // ── 模型列进度胶囊 ──
        let ratio = if max_total > 0 {
            usage.total_tokens() as f64 / max_total as f64
        } else {
            0.0
        };
        svg.push_str(&format!(
            "<rect x=\"{x_model:.1}\" y=\"{bar_y:.1}\" width=\"{MODEL_COL_WIDTH:.1}\" \
             height=\"{BAR_HEIGHT:.1}\" rx=\"{BAR_RX:.1}\" fill=\"{STRIPED}\" opacity=\"0.95\"/>\n"
        ));
        if ratio > 0.0 {
            // 条形宽度按 ratio 成比例；最小 2px 保证极小占比也可见但不失真
            // （原先 .max(16.0) 会把 <7% 的模型都撑到 16px，破坏比例）。
            let bar_fill_w = (ratio * MODEL_COL_WIDTH).max(2.0).min(MODEL_COL_WIDTH);
            svg.push_str(&format!(
                "<rect x=\"{x_model:.1}\" y=\"{bar_y:.1}\" width=\"{bar_fill_w:.1}\" \
                 height=\"{BAR_HEIGHT:.1}\" rx=\"{BAR_RX:.1}\" fill=\"{color}\" opacity=\"0.55\"/>\n"
            ));
        }

        // ── 模型名 ──
        svg.push_str(&format!(
            "<text x=\"{model_text_x:.1}\" y=\"{model_text_y:.1}\" \
             font-family=\"{SANS}\" font-size=\"{FONT_SIZE_NAME}\" \
             font-weight=\"600\" fill=\"{TITLE}\" text-anchor=\"start\">{model_name}</text>\n",
            model_text_x = x_model + 10.0
        ));

        // ── 占比百分比 ──
        let share_pct = if total_all > 0 {
            usage.total_tokens() as f64 / total_all as f64 * 100.0
        } else {
            0.0
        };
        let share_text = format_share(share_pct);
        svg.push_str(&format!(
            "<text x=\"{x_share:.1}\" y=\"{row_mid_y:.1}\" \
             font-family=\"{MONO}\" font-size=\"{FONT_SIZE_ROW}\" \
             fill=\"{DIM}\" text-anchor=\"start\">{share_text}</text>\n"
        ));

        // ── Total tokens ──
        let total_text = format!(
            "↑{} ↓{}",
            format_tokens(usage.in_tokens),
            format_tokens(usage.out_tokens)
        );
        svg.push_str(&format!(
            "<text x=\"{x_total_end:.1}\" y=\"{row_mid_y:.1}\" \
             font-family=\"{MONO}\" font-size=\"{FONT_SIZE_ROW}\" \
             fill=\"{TEXT}\" text-anchor=\"end\">{total_text}</text>\n"
        ));

        // ── 各 Agent 分项 ──
        for (i, agent) in agents.iter().enumerate() {
            let agent_usage = aggregate_agent_usage(&agent.agent_ids, cells, model_name);
            let cell_text = if agent_usage.total_tokens() > 0 {
                format!(
                    "↑{} ↓{}",
                    format_tokens(agent_usage.in_tokens),
                    format_tokens(agent_usage.out_tokens)
                )
            } else {
                "—".to_string()
            };
            svg.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{row_mid_y:.1}\" \
                 font-family=\"{MONO}\" font-size=\"{FONT_SIZE_ROW}\" \
                 fill=\"{TEXT}\" text-anchor=\"end\">{cell_text}</text>\n",
                agent_text_x[i]
            ));
        }
    }

    // ── 底部分隔线 ──
    let bottom_y = data_start_y + rows.len() as f64 * ROW_HEIGHT + 2.0;
    svg.push_str(&format!(
        "<line x1=\"{tx:.1}\" y1=\"{bottom_y:.1}\" x2=\"{:.1}\" y2=\"{bottom_y:.1}\" \
         stroke=\"{BORDER}\" stroke-width=\"0.5\"/>\n",
        tx + actual_table_w
    ));

    svg
}

/// 计算表格所需总高度（像素）。
///
/// Computes the expected table height in pixels for a given number of model rows.
pub(super) fn table_height(row_count: usize) -> f64 {
    HEADER_HEIGHT + 4.0 + row_count as f64 * ROW_HEIGHT + 2.0 + 4.0
}

struct DisplayedAgentColumn<'a> {
    label: String,
    agent_ids: Vec<&'a str>,
}

fn displayed_agent_columns<'a>(
    agent_columns: &'a [(&'static str, &'static str)],
) -> Vec<DisplayedAgentColumn<'a>> {
    let direct_count = agent_columns.len().min(IMAGE_TOP_AGENT_COLUMNS);
    let mut displayed: Vec<DisplayedAgentColumn<'a>> = agent_columns[..direct_count]
        .iter()
        .map(|(agent_id, label)| DisplayedAgentColumn {
            label: (*label).to_string(),
            agent_ids: vec![*agent_id],
        })
        .collect();

    if agent_columns.len() > direct_count {
        displayed.push(DisplayedAgentColumn {
            label: "Others".to_string(),
            agent_ids: agent_columns[direct_count..]
                .iter()
                .map(|(agent_id, _)| *agent_id)
                .collect(),
        });
    }

    displayed
}

fn aggregate_agent_usage(
    agent_ids: &[&str],
    cells: &std::collections::HashMap<(String, String), UsageTotals>,
    model_name: &str,
) -> UsageTotals {
    let mut total = UsageTotals::default();
    for agent_id in agent_ids {
        if let Some(usage) = cells.get(&(agent_id.to_string(), model_name.to_string())) {
            total.add(usage);
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::stats::overview::OverviewTableRow;
    use crate::stats::types::UsageTotals;

    #[test]
    fn assign_colors_maps_models() {
        let data = OverviewTableData {
            rows: vec![
                OverviewTableRow {
                    model: "glm-5.2".to_string(),
                    usage: UsageTotals::default(),
                    color_index: 0,
                },
                OverviewTableRow {
                    model: "gpt-5.4".to_string(),
                    usage: UsageTotals::default(),
                    color_index: 1,
                },
            ],
            total_all: 0,
            cells: HashMap::new(),
            agent_columns: vec![],
        };
        let svg = render_table(&data, (80.0, 500.0, 1000.0));
        assert!(svg.contains(&format!("fill=\"{}\" opacity=\"0.08\"", SVG_COLORS[0])));
        assert!(svg.contains(&format!("fill=\"{}\" opacity=\"0.08\"", SVG_COLORS[1])));
    }

    #[test]
    fn render_table_basic_structure() {
        let data = OverviewTableData {
            rows: vec![OverviewTableRow {
                model: "glm-5.2".to_string(),
                usage: UsageTotals {
                    in_tokens: 100,
                    total_tokens: 100,
                    out_tokens: 50,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                },
                color_index: 0,
            }],
            total_all: 100,
            cells: HashMap::new(),
            agent_columns: vec![],
        };
        let svg = render_table(&data, (80.0, 500.0, 1000.0));
        assert!(svg.contains("Model"));
        assert!(svg.contains("Share"));
        assert!(svg.contains("Total"));
        assert!(svg.contains("glm-5.2"));
    }

    #[test]
    fn render_table_with_agents() {
        let data = OverviewTableData {
            rows: vec![OverviewTableRow {
                model: "glm-5.2".to_string(),
                usage: UsageTotals {
                    in_tokens: 200,
                    total_tokens: 200,
                    out_tokens: 80,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                },
                color_index: 0,
            }],
            cells: [(
                ("claude".to_string(), "glm-5.2".to_string()),
                UsageTotals {
                    in_tokens: 200,
                    total_tokens: 200,
                    out_tokens: 80,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                },
            )]
            .into_iter()
            .collect(),
            total_all: 200,
            agent_columns: vec![("claude", "Claude Code")],
        };
        let svg = render_table(&data, (80.0, 500.0, 1000.0));
        assert!(svg.contains("Claude Code"));
        assert!(svg.contains("↑200 ↓80"));
    }

    #[test]
    fn render_table_collapses_tail_agents_into_others() {
        let data = OverviewTableData {
            rows: vec![OverviewTableRow {
                model: "glm-5.2".to_string(),
                usage: UsageTotals {
                    in_tokens: 200,
                    total_tokens: 200,
                    out_tokens: 80,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                },
                color_index: 0,
            }],
            cells: [
                (
                    ("claude".to_string(), "glm-5.2".to_string()),
                    UsageTotals {
                        in_tokens: 100,
                        total_tokens: 100,
                        out_tokens: 40,
                        cache_read_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                    },
                ),
                (
                    ("codex".to_string(), "glm-5.2".to_string()),
                    UsageTotals {
                        in_tokens: 50,
                        total_tokens: 50,
                        out_tokens: 20,
                        cache_read_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                    },
                ),
                (
                    ("omp".to_string(), "glm-5.2".to_string()),
                    UsageTotals {
                        in_tokens: 31,
                        total_tokens: 31,
                        out_tokens: 11,
                        cache_read_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                    },
                ),
                (
                    ("mimo".to_string(), "glm-5.2".to_string()),
                    UsageTotals {
                        in_tokens: 22,
                        total_tokens: 22,
                        out_tokens: 13,
                        cache_read_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                    },
                ),
            ]
            .into_iter()
            .collect(),
            total_all: 200,
            agent_columns: vec![
                ("claude", "Claude Code"),
                ("codex", "Codex"),
                ("omp", "OMP"),
                ("mimo", "Mimo"),
            ],
        };
        let svg = render_table(&data, (80.0, 500.0, 1000.0));
        assert!(svg.contains("Claude Code"));
        assert!(svg.contains("Codex"));
        assert!(svg.contains("OMP"));
        assert!(svg.contains("Others"));
        assert!(!svg.contains(">Mimo</text>"));
        assert!(svg.contains("↑50 ↓20"));
        let others_usage = aggregate_agent_usage(&["omp", "mimo"], &data.cells, "glm-5.2");
        assert_eq!(others_usage.in_tokens, 53);
        assert_eq!(others_usage.out_tokens, 24);
    }

    #[test]
    fn render_table_striped_rows_odd() {
        let data = OverviewTableData {
            rows: vec![
                OverviewTableRow {
                    model: "a".to_string(),
                    usage: UsageTotals {
                        in_tokens: 80,
                        out_tokens: 20,
                        ..Default::default()
                    },
                    color_index: 0,
                },
                OverviewTableRow {
                    model: "b".to_string(),
                    usage: UsageTotals {
                        in_tokens: 40,
                        out_tokens: 10,
                        ..Default::default()
                    },
                    color_index: 1,
                },
            ],
            cells: HashMap::new(),
            total_all: 150,
            agent_columns: vec![],
        };
        let svg = render_table(&data, (80.0, 500.0, 1000.0));
        // Row 1 (odd) should have striped background
        assert!(svg.contains(&format!("fill=\"{STRIPED}\" opacity=\"0.5\"")));
    }

    #[test]
    fn share_bar_under_model_column() {
        let data = OverviewTableData {
            rows: vec![OverviewTableRow {
                model: "glm-5.2".to_string(),
                usage: UsageTotals {
                    in_tokens: 100,
                    total_tokens: 100,
                    out_tokens: 50,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                },
                color_index: 0,
            }],
            cells: HashMap::new(),
            total_all: 100,
            agent_columns: vec![],
        };
        let svg = render_table(&data, (80.0, 500.0, 1000.0));

        // Bar should be at x_model position (under Model column), not at a separate share bar position
        let x_model = 80.0 + PAD_LEFT; // 92.0
        // Bar background rect should start at x_model
        assert!(svg.contains(&format!("x=\"{x_model:.1}\"")));
        // Bar background width should be MODEL_COL_WIDTH
        assert!(svg.contains(&format!("width=\"{MODEL_COL_WIDTH:.1}\"")));
    }

    #[test]
    fn bar_normalized_by_max_total() {
        let data = OverviewTableData {
            rows: vec![
                OverviewTableRow {
                    model: "a".to_string(),
                    usage: UsageTotals {
                        in_tokens: 160,
                        out_tokens: 40,
                        ..Default::default()
                    },
                    color_index: 0,
                },
                OverviewTableRow {
                    model: "b".to_string(),
                    usage: UsageTotals {
                        in_tokens: 80,
                        out_tokens: 20,
                        ..Default::default()
                    },
                    color_index: 1,
                },
            ],
            cells: HashMap::new(),
            total_all: 300,
            agent_columns: vec![],
        };
        let svg = render_table(&data, (80.0, 500.0, 1000.0));

        // Model "a" has ratio=1.0 → bar fills entire MODEL_COL_WIDTH
        assert!(svg.contains(&format!("width=\"{MODEL_COL_WIDTH:.1}\"")));
        // Model "b" has ratio=0.5 → bar is half of MODEL_COL_WIDTH
        let half_bar = MODEL_COL_WIDTH * 0.5;
        assert!(svg.contains(&format!("width=\"{half_bar:.1}\"")));
    }

    #[test]
    fn bar_tiny_ratio_not_inflated() {
        // 占比极小的模型条形不应被 .max(16.0) 撑到 16px（破坏比例）。
        // a=200 (max), b=2 → b 的 ratio=0.01 → bar≈2.2px，最小 2px，远小于 16。
        let data = OverviewTableData {
            rows: vec![
                OverviewTableRow {
                    model: "a".to_string(),
                    usage: UsageTotals { in_tokens: 200, ..Default::default() },
                    color_index: 0,
                },
                OverviewTableRow {
                    model: "b".to_string(),
                    usage: UsageTotals { in_tokens: 2, ..Default::default() },
                    color_index: 1,
                },
            ],
            cells: HashMap::new(),
            total_all: 202,
            agent_columns: vec![],
        };
        let svg = render_table(&data, (80.0, 500.0, 1000.0));
        // b 的条形宽度（2.2px，min 2）不应出现 16.0
        assert!(!svg.contains("width=\"16.0\""));
    }

    #[test]
    fn background_rects_cover_actual_table_width() {
        let data = OverviewTableData {
            rows: vec![OverviewTableRow {
                model: "a".to_string(),
                usage: UsageTotals {
                    in_tokens: 80,
                    out_tokens: 20,
                    ..Default::default()
                },
                color_index: 0,
            }],
            cells: HashMap::new(),
            total_all: 100,
            agent_columns: vec![("claude", "Claude Code"), ("codex", "Codex")],
        };
        let svg = render_table(&data, (80.0, 500.0, 1000.0));

        // Background rects should use actual_table_w (not available_w)
        // actual_table_w = fixed_width + PAD_LEFT + TABLE_RIGHT_PAD + 2*(agent_col_width + COL_GAP) - COL_GAP
        // = (220+14+56+14+100) + 12 + 16 + 2*(w+14) - 14
        // At minimum agent_col_width=80: actual_table_w = 404 + 28 + 2*94 - 14 = 516
        assert!(svg.contains("width=\"")); // has width attribute
        // The width should be > 100 (the available_w param), because we compute actual width
        let width_str = svg
            .lines()
            .find(|l| l.contains("opacity=\"0.15\"") && l.contains("rect"))
            .unwrap();
        let width_val: f64 = width_str
            .split("width=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(width_val > 400.0, "actual_table_w should cover all columns");
    }

    #[test]
    fn table_height_calculation() {
        assert_eq!(table_height(0), 24.0 + 4.0 + 2.0 + 4.0);
        assert_eq!(table_height(5), 24.0 + 4.0 + 5.0 * 28.0 + 2.0 + 4.0);
    }

    #[test]
    fn model_col_width_contains_bar() {
        // MODEL_COL_WIDTH must be wide enough for typical model names + bar below
        assert!(
            MODEL_COL_WIDTH >= 140.0,
            "Model column should be at least 140px wide"
        );
    }
}
