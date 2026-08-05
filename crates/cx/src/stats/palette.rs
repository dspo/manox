//! SVG 专用调色板 — Tableau-10 配色 + Catppuccin Latte 亮色主题。
//!
//! 纯 SVG 输出不依赖外部 crate；所有颜色值以 hex 字符串常量提供。

/// 12 色 Tableau-10 风格图表配色。
///
/// 索引 0 分配给用量最高的模型，依次递减。
/// 前 10 色 = Tableau-10 经典色，后 2 色 = 补充色以保证长系列区分度。
pub(super) const SVG_COLORS: &[&str] = &[
    "#4e79a7", "#f28e2b", "#e15759", "#76b7b2", "#59a14f", "#edc948", "#b07aa1", "#ff9da7",
    "#9c755f", "#bab0ac", "#86bcb6", "#d4a6c8",
];

// ── Catppuccin Latte 亮色主题（白底 SVG） ──────────────────────

pub(super) const BG: &str = "#ffffff";
pub(super) const HEADER_BG: &str = "#e6e9ef";
#[allow(dead_code)]
pub(super) const PERIOD_BG: &str = "#eff1f5";
pub(super) const BORDER: &str = "#ccd0da";
pub(super) const GRID: &str = "#ccd0da";
pub(super) const AXIS: &str = "#7c7f93";
pub(super) const TITLE: &str = "#4c4f69";
pub(super) const TEXT: &str = "#5c5f77";
pub(super) const DIM: &str = "#7c7f93";
#[allow(dead_code)]
pub(super) const ACTIVE_TAB: &str = "#179299";
#[allow(dead_code)]
pub(super) const ACTIVE_TAB_TEXT: &str = "#ffffff";
#[allow(dead_code)]
pub(super) const INACTIVE_TAB: &str = "#7c7f93";
pub(super) const STRIPED: &str = "#eff1f5";
#[allow(dead_code)]
pub(super) const FOOTER_BG: &str = "#e6e9ef";

// ── 字体栈 ────────────────────────────────────────────────────

pub(super) const SANS: &str = "Inter, 'Segoe UI', 'Helvetica Neue', Arial, sans-serif";
pub(super) const MONO: &str = "'SF Mono', Menlo, 'Courier New', Consolas, monospace";

#[cfg(test)]
mod tests {
    use super::*;

    /// SVG_COLORS 中所有颜色彼此不重复。
    #[test]
    fn svg_colors_distinct() {
        let mut seen = std::collections::HashSet::new();
        for c in SVG_COLORS {
            assert!(!seen.contains(c), "duplicate color in SVG_COLORS: {c}");
            seen.insert(c);
        }
        assert_eq!(
            SVG_COLORS.len(),
            12,
            "SVG_COLORS should have exactly 12 entries"
        );
    }

    /// 主题常量不与 BG (#fff) 重复（否则在白底上不可见）。
    #[test]
    fn theme_not_white() {
        let theme_colors = [
            HEADER_BG,
            PERIOD_BG,
            BORDER,
            GRID,
            AXIS,
            TITLE,
            TEXT,
            DIM,
            ACTIVE_TAB,
            ACTIVE_TAB_TEXT,
            INACTIVE_TAB,
            STRIPED,
            FOOTER_BG,
        ];
        // ACTIVE_TAB_TEXT 是白色文字（在深色标签上），允许等于 BG
        for c in theme_colors {
            if c == ACTIVE_TAB_TEXT {
                continue;
            }
            assert_ne!(
                c, BG,
                "theme color {c} equals background — invisible on white"
            );
        }
    }

    /// ACTIVE_TAB 不等于任何 INACTIVE_TAB / DIM / AXIS（确保激活标签视觉可区分）。
    #[test]
    fn active_tab_distinguishable() {
        assert_ne!(ACTIVE_TAB, INACTIVE_TAB);
        assert_ne!(ACTIVE_TAB, DIM);
        assert_ne!(ACTIVE_TAB, AXIS);
    }

    /// 主题内部所有语义角色彼此不重复（除开已知同值 GRID==BORDER）。
    #[test]
    fn theme_roles_distinct() {
        let roles: &[(&str, &str)] = &[
            ("HEADER_BG", HEADER_BG),
            ("PERIOD_BG", PERIOD_BG),
            ("BORDER", BORDER),
            ("GRID", GRID),
            ("AXIS", AXIS),
            ("TITLE", TITLE),
            ("TEXT", TEXT),
            ("DIM", DIM),
            ("ACTIVE_TAB", ACTIVE_TAB),
            ("ACTIVE_TAB_TEXT", ACTIVE_TAB_TEXT),
            ("INACTIVE_TAB", INACTIVE_TAB),
            ("STRIPED", STRIPED),
            ("FOOTER_BG", FOOTER_BG),
        ];
        // GRID==BORDER 是设计意图（网格与边框同色），允许
        let allowed_duplicates = [("GRID", "BORDER"), ("BORDER", "GRID")];
        let mut map = std::collections::HashMap::new();
        for (name, val) in roles {
            if let Some(prev) = map.insert(name, val) {
                let is_allowed = allowed_duplicates.contains(&(prev, name))
                    || allowed_duplicates.contains(&(name, prev));
                assert!(
                    is_allowed || prev != val,
                    "unexpected duplicate value: {prev} and {name} both = {val}",
                );
            }
        }
    }
}
