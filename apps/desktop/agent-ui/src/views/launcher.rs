//! Empty-tab launcher: the right pane's "new tab" surface.
//!
//! Five vertically centered shortcut rows — built-in browser, built-in
//! terminal, Claude Code, Codex, GitHub Copilot. Picking a row opens the
//! corresponding view on the launcher's own tab (the workspace's
//! `launcher_pick` does the spawn). The three CLI-agent rows anchor the
//! provider→model cascade under the picked row before spawning.

use std::rc::Rc;

use crate::i18n;
use gpui::{AnyElement, App, ClickEvent, Entity, SharedString, Window, deferred, prelude::*, px};
use gpui_component::menu::PopupMenu;
use gpui_component::{Icon, IconName, Sizable as _, Theme, v_flex};

use crate::external_session::SessionKind;

/// Shared pick callback — `Rc` so the five rows can each hold a clone.
type PickHandler = Rc<dyn Fn(LauncherPick, &mut Window, &mut App)>;

/// The integration the user picked from the launcher.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LauncherPick {
    Browser,
    Terminal,
    Agent(SessionKind),
}

/// One shortcut row: brand glyph + localized label, hover-washed.
fn row(
    id: &'static str,
    icon: AnyElement,
    label: SharedString,
    theme: &Theme,
    on_pick: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let hover = theme.muted_foreground.opacity(0.08);
    gpui::div()
        .id(id)
        .w(px(280.))
        .h(px(32.))
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .rounded(theme.radius)
        .cursor_pointer()
        .text_sm()
        .text_color(theme.foreground)
        .hover(move |s| s.bg(hover))
        .child(icon)
        .child(label)
        .on_click(on_pick)
}

/// The launcher body: five centered shortcut rows. `dropdown` anchors the
/// open provider→model cascade under its CLI-agent row.
pub(crate) fn render_launcher_tab(
    theme: &Theme,
    on_pick: PickHandler,
    dropdown: Option<(SessionKind, Entity<PopupMenu>)>,
) -> AnyElement {
    let glyph = |name: IconName| {
        Icon::new(name)
            .small()
            .text_color(theme.muted_foreground)
            .into_any_element()
    };
    let glyph_path = |path: &'static str| {
        Icon::default()
            .path(path)
            .small()
            .text_color(theme.muted_foreground)
            .into_any_element()
    };

    let mut rows: Vec<AnyElement> = Vec::new();
    let mut push = |pick: LauncherPick, id: &'static str, icon: AnyElement, key: &'static str| {
        let on_pick = on_pick.clone();
        let r = row(id, icon, i18n::t(key), theme, move |_e, window, cx| {
            on_pick(pick, window, cx);
        });
        // The picked CLI agent's cascade hangs below its row.
        rows.push(
            match dropdown.as_ref() {
                Some((menu_kind, menu))
                    if matches!(pick, LauncherPick::Agent(k) if k == *menu_kind) =>
                {
                    gpui::div()
                        .relative()
                        .child(r)
                        .child(
                            deferred(
                                gpui::div()
                                    .id(format!("launcher-cascade-{id}"))
                                    .absolute()
                                    .top_full()
                                    .left_0()
                                    .occlude()
                                    .child(menu.clone()),
                            )
                            .with_priority(1),
                        )
                        .into_any_element()
                }
                _ => r.into_any_element(),
            },
        );
    };

    push(
        LauncherPick::Browser,
        "launcher-browser",
        glyph(IconName::Globe),
        "launcher-open-browser",
    );
    push(
        LauncherPick::Terminal,
        "launcher-terminal",
        glyph_path("icons/terminal.svg"),
        "launcher-open-terminal",
    );
    push(
        LauncherPick::Agent(SessionKind::ClaudeCode),
        "launcher-claude",
        glyph_path("icons/claude.svg"),
        "launcher-open-claude",
    );
    push(
        LauncherPick::Agent(SessionKind::Codex),
        "launcher-codex",
        glyph_path("icons/codex.svg"),
        "launcher-open-codex",
    );
    push(
        LauncherPick::Agent(SessionKind::GithubCopilot),
        "launcher-copilot",
        glyph_path("icons/githubcopilot.svg"),
        "launcher-open-copilot",
    );

    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_1()
        .children(rows)
        .into_any_element()
}
