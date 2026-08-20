//! Phase E stub — subagents view helpers gutted.

use agent::ToolCallStatus;
use gpui::{AnyElement, prelude::*};
use gpui_component::Theme;

#[derive(Clone, Debug)]
pub struct SubagentInfo {
    pub id: String,
    pub subagent_type: String,
    pub description: String,
    pub status: ToolCallStatus,
}

pub fn task_display_title(_subagent_type: &str, _description: &str) -> Option<String> {
    None
}

pub fn subagent_display_title(_info: &SubagentInfo) -> String {
    String::new()
}

pub fn status_indicator(_status: ToolCallStatus, _theme: &Theme) -> AnyElement {
    gpui::div().into_any_element()
}
