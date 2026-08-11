//! Shared sub-agent observation data.
//!
//! The rail's agents section renders these as observe-only rows; the retired
//! manox harness additionally drilled into a per-child panel, which the pi
//! harness has no child-thread entities for (the panel was retired with the
//! manox harness).

use agent::ToolCallStatus;
use gpui::AnyElement;
use gpui::prelude::*;
use gpui_component::{Icon, IconName, Sizable as _, Theme};

use crate::views::braille_spinner::BrailleSpinner;

#[derive(Clone, Debug)]
pub(crate) struct SubagentInfo {
    pub id: String,
    pub subagent_type: String,
    pub description: String,
    pub status: ToolCallStatus,
}

pub(crate) fn subagent_display_title(info: &SubagentInfo) -> String {
    if info.description.is_empty() {
        info.subagent_type.clone()
    } else if info.subagent_type.is_empty() {
        info.description.clone()
    } else {
        format!("{} · {}", info.subagent_type, info.description)
    }
}

pub(crate) fn status_indicator(status: ToolCallStatus, theme: &Theme) -> AnyElement {
    match status {
        ToolCallStatus::PendingApproval | ToolCallStatus::Running => BrailleSpinner::new()
            .xsmall()
            .color(theme.accent_foreground)
            .into_any_element(),
        ToolCallStatus::Success | ToolCallStatus::Continued => Icon::default()
            .path("icons/circle-check-big.svg")
            .xsmall()
            .text_color(theme.success)
            .into_any_element(),
        ToolCallStatus::Error | ToolCallStatus::Denied => Icon::new(IconName::CircleX)
            .xsmall()
            .text_color(theme.danger)
            .into_any_element(),
        ToolCallStatus::Cancelled => Icon::new(IconName::Minus)
            .xsmall()
            .text_color(theme.muted_foreground)
            .into_any_element(),
    }
}
