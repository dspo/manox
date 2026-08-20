//! Phase E stub — member_panel gutted. Members are regular threads now.

use agent::Thread;
use agent::team::Team;
use gpui::{App, Context, Entity, Render, WeakEntity, Window, prelude::*};
use gpui_component::ActiveTheme as _;

use crate::Workspace;

pub struct MemberPanel;

impl MemberPanel {
    pub fn new(
        _member: Entity<Thread>,
        _member_name: String,
        _role: String,
        _team: WeakEntity<Team>,
        _weak_workspace: WeakEntity<Workspace>,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|_| Self)
    }

    pub fn member_name(&self) -> &str {
        ""
    }
}

impl Render for MemberPanel {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let t = cx.theme().clone();
        gpui::div().h_full().w_full().bg(t.background)
    }
}
