//! Phase E stub — subagent_panel gutted (sync streaming retired).

use agent::{ToolCallStatus, thread::SubagentChildEvent};
use gpui::{App, Context, Entity, Render, WeakEntity, Window, prelude::*};
use gpui_component::ActiveTheme as _;

use crate::Workspace;

pub struct SubagentPanel;

impl SubagentPanel {
    pub fn new(
        _title: String,
        _role: String,
        _status: ToolCallStatus,
        _backfill: &[SubagentChildEvent],
        _final_text: Option<String>,
        _weak_workspace: WeakEntity<Workspace>,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|_| Self)
    }

    pub fn title(&self) -> &str {
        ""
    }

    pub fn push(&mut self, _child: &SubagentChildEvent, cx: &mut Context<Self>) {
        cx.notify();
    }

    pub fn set_status(&mut self, _status: ToolCallStatus, cx: &mut Context<Self>) {
        cx.notify();
    }
}

impl Render for SubagentPanel {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let t = cx.theme().clone();
        gpui::div().h_full().w_full().bg(t.background)
    }
}
