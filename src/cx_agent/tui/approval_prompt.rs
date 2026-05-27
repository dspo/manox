//! Approval prompt helpers for the chat TUI.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::cx_agent::approval::ToolCategory;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool_name: String,
    pub category: ToolCategory,
    pub arguments_preview: String,
}

#[allow(dead_code)]
impl ApprovalRequest {
    pub fn new(
        tool_name: impl Into<String>,
        category: ToolCategory,
        arguments_preview: impl Into<String>,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            category,
            arguments_preview: arguments_preview.into(),
        }
    }

    pub fn from_json(
        tool_name: impl Into<String>,
        category: ToolCategory,
        value: &serde_json::Value,
    ) -> Self {
        let preview = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
        Self::new(tool_name, category, preview)
    }

    pub fn category_label(&self) -> &'static str {
        match self.category {
            ToolCategory::Read => "read",
            ToolCategory::Write => "write",
            ToolCategory::Execute => "execute",
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ApprovalPromptMetrics {
    pub total_lines: usize,
    pub viewport_lines: usize,
}

#[allow(dead_code)]
impl ApprovalPromptMetrics {
    pub fn max_scroll(self) -> usize {
        self.total_lines.saturating_sub(self.viewport_lines.max(1))
    }

    pub fn page_step(self) -> usize {
        self.viewport_lines.saturating_sub(1).max(3)
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ApprovalPromptState {
    pub request: ApprovalRequest,
    pub scroll: usize,
}

#[allow(dead_code)]
impl ApprovalPromptState {
    pub fn new(request: ApprovalRequest) -> Self {
        Self { request, scroll: 0 }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        match (key.code, key.modifiers) {
            (KeyCode::PageUp, _) => {
                self.scroll = self.scroll.saturating_sub(8);
                true
            }
            (KeyCode::PageDown, _) => {
                self.scroll = self.scroll.saturating_add(8);
                true
            }
            (KeyCode::Up, mods) if mods.contains(KeyModifiers::CONTROL) => {
                self.scroll = self.scroll.saturating_sub(1);
                true
            }
            (KeyCode::Down, mods) if mods.contains(KeyModifiers::CONTROL) => {
                self.scroll = self.scroll.saturating_add(1);
                true
            }
            _ => false,
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(3);
                true
            }
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_add(3);
                true
            }
            _ => false,
        }
    }
}

pub fn render_approval_preview_lines(preview: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let style = Style::default().fg(Color::White);
    for line in wrap_preview(preview, width) {
        lines.push(Line::from(Span::styled(line, style)));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("(no arguments)", style)));
    }
    lines
}

fn wrap_preview(preview: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    if preview.is_empty() {
        return out;
    }
    for raw in preview.split('\n') {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut rest = raw;
        while !rest.is_empty() {
            let take = preview_split_at(rest, width);
            let (head, tail) = rest.split_at(take);
            out.push(head.to_string());
            rest = tail;
        }
    }
    out
}

fn preview_split_at(text: &str, max_chars: usize) -> usize {
    if text.chars().count() <= max_chars {
        return text.len();
    }
    let mut count = 0;
    for (idx, ch) in text.char_indices() {
        if count == max_chars {
            return idx;
        }
        count += 1;
        if idx + ch.len_utf8() == text.len() && count <= max_chars {
            return text.len();
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_preview_from_json() {
        let request =
            ApprovalRequest::from_json("bash", ToolCategory::Execute, &json!({"cmd":"ls"}));
        assert!(request.arguments_preview.contains("cmd"));
        assert_eq!(request.category_label(), "execute");
    }

    #[test]
    fn wraps_preview_lines() {
        let lines = render_approval_preview_lines("abcdefgh", 3);
        let rendered: Vec<String> = lines.into_iter().map(|line| line.to_string()).collect();
        assert_eq!(rendered, vec!["abc", "def", "gh"]);
    }

    #[test]
    fn prompt_metrics_max_scroll_is_clamped() {
        let metrics = ApprovalPromptMetrics {
            total_lines: 12,
            viewport_lines: 4,
        };
        assert_eq!(metrics.max_scroll(), 8);
        assert_eq!(metrics.page_step(), 3);
        assert_eq!(std::cmp::min(20, metrics.max_scroll()), 8);
    }
}
