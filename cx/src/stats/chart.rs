//! SVG 折线图（Area Chart）渲染模块。
//!
//! 输入与 TUI draw_step_chart 相同的数据结构，但完全独立渲染。
//! 输出为 SVG 字符串，包含：
//! - Y 轴刻度标签 + 网格虚线
//! - X 轴日期标签 + 短刻度线
//! - 每条 series 的渐变填充 + 线条描边
//! - 右侧 Legend

use super::format::{format_tokens, short_date};
use super::overview::OverviewChartData;
use super::palette;

/// 折线图数据 series：模型名、每日值列表、颜色 hex。
pub(super) type ChartSeries = (String, Vec<f64>, String);

/// 折线图区域像素边界。
pub(super) struct PlotBounds {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

// ── 坐标映射 ──────────────────────────────────────────────────

/// 将数值映射到 y 像素坐标（值越大越靠近 plot_top）。
pub(super) fn y_to_px(value: f64, max_y: f64, plot_top: u32, plot_bottom: u32) -> u32 {
    if max_y <= 0.0 {
        return plot_bottom;
    }
    let ratio = (value / max_y).clamp(0.0, 1.0);
    let plot_height = plot_bottom - plot_top;
    plot_bottom - (ratio * plot_height as f64).round() as u32
}

/// 将 day 索引映射到 x 坏素坐标。
pub(super) fn x_to_px(day_idx: usize, day_count: usize, plot_left: u32, plot_right: u32) -> u32 {
    if day_count <= 1 {
        return (plot_left + plot_right) / 2;
    }
    let ratio = day_idx as f64 / (day_count - 1) as f64;
    let plot_width = plot_right - plot_left;
    plot_left + (ratio * plot_width as f64).round() as u32
}

// ── 刻度计算 ──────────────────────────────────────────────────

/// 生成"美观"的 y 轴刻度值列表。
///
/// 策略：从 0 到 max_value，步长为 round_step，确保不超过 target_count 个刻度。
/// round_step 是 max_value/target_count 向上取整到最近的"美观"值（1, 2, 5 × 10^n）。
pub(super) fn nice_ticks(max_value: u64, target_count: usize) -> Vec<u64> {
    if max_value == 0 || target_count <= 1 {
        return vec![0];
    }

    let rough_step = max_value as f64 / target_count as f64;
    let mag = 10.0_f64.powf(rough_step.log10().floor());
    let residual = rough_step / mag;

    // 选择 1, 2, 或 5 作为美化基数
    let nice_step_mag = if residual <= 1.5 {
        1.0
    } else if residual <= 3.0 {
        2.0
    } else if residual <= 7.0 {
        5.0
    } else {
        10.0
    };

    let step = (nice_step_mag * mag).round() as u64;
    if step == 0 {
        return vec![0];
    }

    let mut ticks = Vec::new();
    let mut v = 0u64;
    while v <= max_value {
        ticks.push(v);
        v += step;
    }
    // 确保包含 max_value 附近的上界刻度
    if ticks.last().map_or(true, |&t| t < max_value) {
        ticks.push(v);
    }
    ticks
}

/// 选择 X 轴日期标签的显示索引：超过 14 天时智能跳过。
///
/// 返回应显示日期标签的 day 索引列表。
pub(super) fn x_tick_indices(day_count: usize) -> Vec<usize> {
    if day_count == 0 {
        return Vec::new();
    }
    if day_count <= 14 {
        return (0..day_count).collect();
    }

    // 超过 14 天：选择合适的间隔，保证至少 6 个标签
    let target = 6.min(day_count);
    let step = (day_count as f64 / target as f64).ceil() as usize;
    let step = step.max(1);

    let mut indices = Vec::new();
    let mut i = 0;
    while i < day_count {
        indices.push(i);
        i += step;
    }
    // 始终包含最后一个日期
    if indices.last() != Some(&(day_count - 1)) {
        indices.push(day_count - 1);
    }
    indices
}

// ── SVG 渲染 ──────────────────────────────────────────────────

/// 渲染完整的折线图 SVG 片段（不含外层 <svg> 包裹）。
///
/// 返回可直接嵌入 layout 文档的 SVG `<g>` 元素字符串。
///
/// # 参数
/// - `series`: 模型 series 列表（按排名从高到低排列，渲染时从后到前）
/// - `dates`: 日期标签字符串列表
/// - `max_y`: y 轴最大值
/// - `bounds`: 折线图区域像素边界
pub(super) fn render_area_chart(
    series: &[ChartSeries],
    dates: &[String],
    max_y: f64,
    bounds: &PlotBounds,
) -> String {
    let mut svg = String::with_capacity(4096);

    let plot_left = bounds.left;
    let plot_right = bounds.right;
    let plot_top = bounds.top;
    let plot_bottom = bounds.bottom;
    let day_count = dates.len();

    // max_bound: 留 5% 头部空间
    let max_bound = (max_y * 1.05).max(1.0);
    let ticks = nice_ticks(max_y.round() as u64, 8);
    let x_ticks = x_tick_indices(day_count);

    // ── Y 轴刻度标签 + 网格线 ────────────────────────────────
    svg.push_str("<g class=\"y-axis\">\n");
    for &tick_val in &ticks {
        let tick_f = tick_val as f64;
        let y = y_to_px(tick_f, max_bound, plot_top, plot_bottom);
        let label = format_tokens(tick_val);

        // 刻度标签：plot_left 左侧 8px，text-anchor=end
        svg.push_str(&format!(
            "  <text x=\"{}\" y=\"{}\" text-anchor=\"end\" \
             font-family=\"{mono}\" font-size=\"11\" fill=\"{dim}\">{label}</text>\n",
            plot_left - 8,
            y + 4, // +4 使文字垂直居中于刻度线
            mono = palette::MONO,
            dim = palette::DIM,
            label = label,
        ));

        // 网格虚线：从 plot_left 到 plot_right
        svg.push_str(&format!(
            "  <line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" \
             stroke=\"{grid}\" stroke-width=\"0.5\" stroke-dasharray=\"4,4\"/>\n",
            plot_left,
            y,
            plot_right,
            y,
            grid = palette::GRID,
        ));
    }
    svg.push_str("</g>\n");

    // ── X 轴刻度标签 + 短刻度线 ──────────────────────────────
    svg.push_str("<g class=\"x-axis\">\n");
    let tick_y = plot_bottom + 18; // 标签位于 baseline 下方 18px
    for &idx in &x_ticks {
        let x = x_to_px(idx, day_count, plot_left, plot_right);
        let label = short_date(&dates[idx]);

        // 短刻度线
        svg.push_str(&format!(
            "  <line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" \
             stroke=\"{axis}\" stroke-width=\"0.5\"/>\n",
            x,
            plot_bottom,
            x,
            plot_bottom + 4,
            axis = palette::AXIS,
        ));

        // 日期标签
        svg.push_str(&format!(
            "  <text x=\"{}\" y=\"{}\" text-anchor=\"middle\" \
             font-family=\"{mono}\" font-size=\"11\" fill=\"{dim}\">{label}</text>\n",
            x,
            tick_y,
            mono = palette::MONO,
            dim = palette::DIM,
            label = label,
        ));
    }
    svg.push_str("</g>\n");

    // ── 数据 series ────────────────────────────────────────────
    // 渲染顺序：从后到前（最低用量的模型先画，最高的在顶层）
    // 这样最高用量模型的渐变填充不会被遮挡

    // 先收集所有渐变定义
    svg.push_str("<defs>\n");
    for (idx, (_, _, color)) in series.iter().enumerate() {
        svg.push_str(&format!(
            "  <linearGradient id=\"grad-{idx}\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\">\n\
             \x20   <stop offset=\"0%\" stop-color=\"{color}\" stop-opacity=\"0.3\"/>\n\
             \x20   <stop offset=\"100%\" stop-color=\"{color}\" stop-opacity=\"0.05\"/>\n\
             \x20 </linearGradient>\n",
            idx = idx,
            color = color,
        ));
    }
    svg.push_str("</defs>\n");

    // 绘制每条 series（从后到前）
    for (idx, (_model_name, values, color)) in series.iter().enumerate() {
        if values.is_empty() || day_count == 0 {
            continue;
        }

        // ── 面积填充路径 ────────────────────────────────────
        // 从 baseline 左端出发，经过所有数据点，回到 baseline 右端
        let mut area_path = format!("M{},{}", plot_left, plot_bottom);
        for (day_idx, &val) in values.iter().enumerate() {
            let day_idx = day_idx.min(day_count - 1);
            let x = x_to_px(day_idx, day_count, plot_left, plot_right);
            let y = y_to_px(val, max_bound, plot_top, plot_bottom);
            area_path.push_str(&format!(" L{},{}", x, y));
        }
        // 闭合回到 baseline
        let last_x = x_to_px(
            values.len() - 1.min(day_count - 1),
            day_count,
            plot_left,
            plot_right,
        );
        area_path.push_str(&format!(" L{},{} Z", last_x, plot_bottom));

        svg.push_str(&format!(
            "  <path d=\"{area_path}\" fill=\"url(#grad-{idx})\" />\n",
            area_path = area_path,
            idx = idx,
        ));

        // ── 线条描边路径 ────────────────────────────────────
        let mut line_path = String::new();
        for (day_idx, &val) in values.iter().enumerate() {
            let day_idx = day_idx.min(day_count - 1);
            let x = x_to_px(day_idx, day_count, plot_left, plot_right);
            let y = y_to_px(val, max_bound, plot_top, plot_bottom);
            if day_idx == 0 {
                line_path.push_str(&format!("M{},{}", x, y));
            } else {
                line_path.push_str(&format!(" L{},{}", x, y));
            }
        }

        svg.push_str(&format!(
            "  <path d=\"{line_path}\" \
             stroke=\"{color}\" stroke-width=\"2.5\" fill=\"none\" \
             stroke-linejoin=\"round\" stroke-linecap=\"round\" />\n",
            line_path = line_path,
            color = color,
        ));
    }

    // ── Legend ─────────────────────────────────────────────────
    // 右侧竖排 legend，每个 series 一个色块 + 模型名
    let legend_x = plot_right + 16;
    let legend_start_y = plot_top + 4;
    let legend_item_height = 20;

    svg.push_str("<g class=\"legend\">\n");
    for (idx, (model_name, _, color)) in series.iter().enumerate() {
        let y = legend_start_y + idx as u32 * legend_item_height;
        // 色块
        svg.push_str(&format!(
            "  <rect x=\"{}\" y=\"{}\" width=\"12\" height=\"12\" rx=\"2\" fill=\"{color}\"/>\n",
            legend_x,
            y,
            color = color,
        ));
        // 模型名
        svg.push_str(&format!(
            "  <text x=\"{}\" y=\"{}\" font-family=\"{sans}\" font-size=\"12\" \
             fill=\"{title}\">{name}</text>\n",
            legend_x + 18,
            y + 11,
            sans = palette::SANS,
            title = palette::TITLE,
            name = model_name,
        ));
    }
    svg.push_str("</g>\n");

    svg
}

/// 高级入口：从原始记录 + top 模型列表生成折线图 SVG `<g>` 片段。
///
/// 由 mod.rs dispatch 调用。内部完成：
/// 1. 构建每日 per-model token 总量
/// 2. 生成日期序列
/// 3. 计算 max_y
/// 4. 确定绘图区域边界（使用 layout 常量）
/// 5. 调用 render_area_chart
pub(super) fn area_chart(data: &OverviewChartData, bounds: &PlotBounds) -> String {
    let series: Vec<ChartSeries> = data
        .series
        .iter()
        .map(|series| {
            (
                series.model.clone(),
                series.values.clone(),
                palette::SVG_COLORS[series.color_index % palette::SVG_COLORS.len()].to_string(),
            )
        })
        .collect();
    render_area_chart(&series, &data.dates, data.max_y, bounds)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── nice_ticks ──────────────────────────────────────────

    #[test]
    fn nice_ticks_zero() {
        let ticks = nice_ticks(0, 8);
        assert_eq!(ticks, vec![0]);
    }

    #[test]
    fn nice_ticks_small() {
        let ticks = nice_ticks(100, 5);
        // 100/5=20 → mag=10, residual=2 → step=20
        assert_eq!(ticks, vec![0, 20, 40, 60, 80, 100]);
    }

    #[test]
    fn nice_ticks_medium() {
        let ticks = nice_ticks(1_000_000, 5);
        // 200000 → mag=100000, residual=2 → step=200000
        assert!(ticks.contains(&0));
        assert!(ticks.contains(&1_000_000));
        assert!(ticks.len() <= 7);
    }

    #[test]
    fn nice_ticks_large() {
        let ticks = nice_ticks(5_000_000, 8);
        // 625000 → mag=100000, residual=6.25 → nice=10 → step=1_000_000
        assert!(ticks.contains(&0));
        assert!(*ticks.last().unwrap() >= 5_000_000);
    }

    #[test]
    fn nice_ticks_one_target() {
        let ticks = nice_ticks(100, 1);
        assert_eq!(ticks, vec![0]);
    }

    // ── y_to_px ─────────────────────────────────────────────

    #[test]
    fn y_to_px_zero() {
        assert_eq!(y_to_px(0.0, 100.0, 100, 500), 500);
    }

    #[test]
    fn y_to_px_max() {
        assert_eq!(y_to_px(100.0, 100.0, 100, 500), 100);
    }

    #[test]
    fn y_to_px_mid() {
        assert_eq!(y_to_px(50.0, 100.0, 100, 500), 300);
    }

    #[test]
    fn y_to_px_negative_max() {
        assert_eq!(y_to_px(42.0, 0.0, 100, 500), 500);
    }

    // ── x_to_px ─────────────────────────────────────────────

    #[test]
    fn x_to_px_single() {
        assert_eq!(x_to_px(0, 1, 80, 1040), 560);
    }

    #[test]
    fn x_to_px_first() {
        assert_eq!(x_to_px(0, 7, 80, 1040), 80);
    }

    #[test]
    fn x_to_px_last() {
        assert_eq!(x_to_px(6, 7, 80, 1040), 1040);
    }

    #[test]
    fn x_to_px_mid() {
        // 3/6 = 0.5 → (80 + 960 * 0.5) = 560
        assert_eq!(x_to_px(3, 7, 80, 1040), 560);
    }

    // ── x_tick_indices ──────────────────────────────────────

    #[test]
    fn x_tick_indices_empty() {
        assert!(x_tick_indices(0).is_empty());
    }

    #[test]
    fn x_tick_indices_small() {
        let idx = x_tick_indices(7);
        assert_eq!(idx, vec![0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn x_tick_indices_14() {
        let idx = x_tick_indices(14);
        assert_eq!(idx.len(), 14);
    }

    #[test]
    fn x_tick_indices_30() {
        let idx = x_tick_indices(30);
        assert!(idx.len() >= 6);
        assert_eq!(idx.first(), Some(&0));
        assert_eq!(idx.last(), Some(&29));
    }

    #[test]
    fn x_tick_indices_always_includes_last() {
        for n in [15, 20, 31, 60, 100] {
            let idx = x_tick_indices(n);
            assert_eq!(idx.last(), Some(&(n - 1)));
        }
    }

    // ── render_area_chart smoke ──────────────────────────────

    #[test]
    fn render_smoke() {
        let series: Vec<ChartSeries> = vec![
            ("model-a".into(), vec![10.0, 20.0, 30.0], "#4e79a7".into()),
            ("model-b".into(), vec![5.0, 15.0, 10.0], "#f28e2b".into()),
        ];
        let dates: Vec<String> = vec![
            "2024-06-01".into(),
            "2024-06-02".into(),
            "2024-06-03".into(),
        ];
        let bounds = PlotBounds {
            left: 80,
            right: 1040,
            top: 100,
            bottom: 500,
        };
        let svg = render_area_chart(&series, &dates, 30.0, &bounds);

        // 验证关键元素存在
        assert!(svg.contains("<g class=\"y-axis\">"));
        assert!(svg.contains("<g class=\"x-axis\">"));
        assert!(svg.contains("<defs>"));
        assert!(svg.contains("linearGradient id=\"grad-0\""));
        assert!(svg.contains("linearGradient id=\"grad-1\""));
        assert!(svg.contains("fill=\"url(#grad-0)\""));
        assert!(svg.contains("fill=\"url(#grad-1)\""));
        assert!(svg.contains("stroke=\"#4e79a7\""));
        assert!(svg.contains("stroke=\"#f28e2b\""));
        assert!(svg.contains("<g class=\"legend\">"));
        assert!(svg.contains("model-a"));
        assert!(svg.contains("model-b"));
    }

    #[test]
    fn render_empty_series() {
        let series: Vec<ChartSeries> = vec![];
        let dates: Vec<String> = vec!["2024-06-01".into()];
        let bounds = PlotBounds {
            left: 80,
            right: 1040,
            top: 100,
            bottom: 500,
        };
        let svg = render_area_chart(&series, &dates, 0.0, &bounds);
        // 应仍有 y/x axis, 无 series paths
        assert!(svg.contains("<g class=\"y-axis\">"));
        assert!(!svg.contains("linearGradient"));
    }

    #[test]
    fn render_single_day() {
        let series: Vec<ChartSeries> = vec![("glm-5".into(), vec![100.0], "#4e79a7".into())];
        let dates: Vec<String> = vec!["2024-06-01".into()];
        let bounds = PlotBounds {
            left: 80,
            right: 1040,
            top: 100,
            bottom: 500,
        };
        let svg = render_area_chart(&series, &dates, 100.0, &bounds);
        assert!(svg.contains("linearGradient"));
        assert!(svg.contains("glm-5"));
    }
}
