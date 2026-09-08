//! The workspace chrome render face (U9b cluster 7): the `Render` impl
//! and the three large element builders it composes — `render_manox`
//! (the full sidebar/conversation/right-pane/overlay chrome),
//! `shell_root` (the shared layout container) and
//! `render_terminal_column`. Split from `workspace.rs` as one
//! contiguous run of impl blocks — `super` is the workspace module, so
//! the builders reach the parent's private fields and methods
//! unchanged.

use super::*;

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_manox(window, cx)
    }
}
impl Workspace {
    /// The full workspace chrome: sidebar, conversation column, context rail,
    /// right pane, question-card overlays. Shared by both harness builds.
    fn render_manox(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !matches!(self.view_mode, ViewMode::Workspace) {
            self.drop_turn_navigator(cx);
        }
        // The native webview keeps its last bounds until explicitly hidden,
        // so every frame must pin which one may draw (the active browser tab
        // of a visible right pane) — an inactive tab would otherwise paint
        // over the pane's content.
        self.sync_browser_visibility(cx);
        // Settings reuses the shared shell (`sidebar | divider | main`) — the
        // same layout container as the app page, only with the settings nav in
        // the sidebar slot and the settings panel as the main column. The
        // underlying Workspace state (conversation sidebar, composer) is
        // preserved and returns unchanged when the user clicks "Back to app".
        if matches!(self.view_mode, ViewMode::Settings) {
            let settings = self
                .settings_view
                .as_ref()
                .expect("enter_settings must have created the SettingsView")
                .clone();
            let nav = settings.update(cx, |s, cx| s.render_nav(window, cx));
            let main = settings.update(cx, |s, cx| s.render_main(window, cx));
            // Horizontal slide: enter glides the panel in from the left edge
            // (offset -PANEL_W → 0), exit glides it out to the right
            // (offset 0 → +PANEL_W). The animation id mixes the current
            // transition generation into the per-direction tag so a fresh
            // tween fires on every direction change (a stable id would
            // replay from the cached delta and visibly jump, and a
            // direction change with the same id would not animate at all).
            let (anim_id, sign) = if self.exiting_settings {
                (
                    format!("settings-exit-{}", self.settings_transition_gen),
                    1.0,
                )
            } else {
                (
                    format!("settings-enter-{}", self.settings_transition_gen),
                    -1.0,
                )
            };
            let panel_w = px(280.0);
            let shell = self.shell_root(nav, main, cx);
            let anim_el = shell.with_animation(
                anim_id,
                Animation::new(Duration::from_millis(SLIDE_MS)).with_easing(ease_out_quint()),
                move |el, delta| {
                    let offset = panel_w * sign * (1.0 - delta);
                    el.relative().ml(offset)
                },
            );
            return h_flex().size_full().child(anim_el).into_any_element();
        }
        // Terminal pane: the shared shell (sidebar + draggable divider) with a
        // full-bleed terminal view as the main column. The terminal view owns
        // its PTY and grid; this branch only mounts it.
        // Resize/scrollback/selection are handled inside `TerminalView` /
        // `TerminalElement`.
        if matches!(self.view_mode, ViewMode::Terminal) {
            let title_text: SharedString = self
                .store
                .as_ref()
                .and_then(|s| {
                    s.read(cx)
                        .store
                        .project
                        .clone()
                        .map(std::path::PathBuf::from)
                })
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("manox")
                .to_string()
                .into();
            let terminal = self
                .terminal_view
                .clone()
                .expect("view_mode == Terminal implies terminal_view is set");
            let icon = Icon::new(IconName::SquareTerminal)
                .small()
                .into_any_element();
            return self
                .shell_root(
                    self.sidebar.clone(),
                    self.render_terminal_column(icon, title_text, terminal),
                    cx,
                )
                .on_action(
                    cx.listener(|this, _: &crate::ToggleCockpitTasks, _window, cx| {
                        this.context_rail.update(cx, |r, cx| {
                            r.cockpit_hide_tasks = !r.cockpit_hide_tasks;
                            cx.notify();
                        });
                        cx.notify();
                    }),
                )
                .into_any_element();
        }
        // External agent CLI session: render the active session's terminal TUI
        // in place of the conversation. Same shared shell as the conversation
        // and terminal views — only the main column (the agent's TUI) and the
        // title differ, so the sidebar divider stays draggable here too. The
        // bar title is the agent's OSC title (mirrored from
        // `TerminalEvent::Title`), falling back to the kind label ("Claude
        // Code" / "Codex" / "GitHub Copilot") until the TUI sets its own. The
        // provider/model picked at spawn is intentionally omitted: the user
        // can switch models mid-session inside the TUI (`/model`), and manox
        // cannot observe that change.
        if matches!(self.view_mode, ViewMode::ExternalSession) {
            let active = self
                .active_external
                .as_deref()
                .and_then(|id| self.external_sessions.iter().find(|s| s.id == id));
            if let Some(session) = active {
                let kind = session.kind;
                // Titlebar + sidebar share `display_title()` so a TUI rename
                // (OSC title) updates both at once.
                let title: SharedString = session.display_title();
                let terminal = session.terminal_view.clone();
                let icon = gpui::svg()
                    .path(kind.icon_asset())
                    .size(px(16.))
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element();
                return self
                    .shell_root(
                        self.sidebar.clone(),
                        self.render_terminal_column(icon, title, terminal),
                        cx,
                    )
                    .into_any_element();
            }
            // No live session matches the recorded id (closed underneath us).
            // Fall back to the conversation pane: flip the mode and fall
            // through to the Workspace branch below, so this frame renders the
            // full shell (sidebar + divider + conversation) rather than a
            // sidebar-only stub that skips the divider and the mode-switching
            // actions.
            self.view_mode = ViewMode::Workspace;
            cx.notify();
        }
        let theme = cx.theme().clone();
        let running = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .expect("foreground store present");

        self.ensure_blank_project_input(window, cx);

        if self.blocking_overlay_active() && self.turn_navigator.is_some() {
            self.close_turn_navigator(window, cx);
        }

        let editor_open = self.editor_open;
        let right_pane_open = self.right_pane_open();
        let editor_preview = self.editor_preview;
        let editor_width = self.editor_width;
        // Title text is the active thread's display title (persisted/generated
        // title > mechanical summary). Falls back to "manox" so an unselected
        // first screen stays branded before any title is generated.
        let title_text: SharedString = {
            let s = self
                .store
                .as_ref()
                .map(|s| s.read(cx).store.with(|st| st.display_title.clone()))
                .expect("foreground store present");
            if s.is_empty() { "manox".to_string() } else { s }
        }
        .into();
        // Empty first screen: no messages and nothing streaming. The composer is
        // hoisted into a vertically-centered hero (heading + composer + "Choose
        // project"); once the conversation starts it drops to the bottom footer.
        // Restoring history keeps the composer mounted so the user can draft
        // immediately, while submission remains gated until the transcript is
        // authoritative.
        let first_screen = self.conversation.read(cx).is_empty(cx) && !running;
        // T10c (§D.6): the v1 `history_phase` mirror retired with the fold —
        // at HEAD the field was unwritten (default `Ready`), so the loading
        // branch already never fired. The §D.1 snapshot is the restore
        // boundary; a pending-snapshot loading indicator belongs to the
        // §K.5 closeout.
        let loading = false;
        let composer_placement = composer_placement(editor_open && right_pane_open, first_screen);
        let main_body_w = window.bounds().size.width
            - self.sidebar_width
            - px(SIDEBAR_DIVIDER_WIDTH)
            - if right_pane_open {
                editor_width + px(EDITOR_DIVIDER_WIDTH)
            } else {
                px(0.)
            };
        let show_rail = !first_screen
            && (!editor_open || !right_pane_open)
            && self
                .store
                .as_ref()
                .map(|s| s.read(cx).store.has_interacted)
                .expect("foreground store present")
            && crate::views::context_rail::ContextRail::rail_width_for(main_body_w).is_some();
        let overlay = self
            .render_blank_project_overlay(window, &theme, cx)
            .or_else(|| self.render_pending_auth_overlay(&theme, cx));
        let turn_navigator_overlay =
            self.render_turn_navigator_overlay(window, &theme, right_pane_open, show_rail, cx);
        // The inline composer stays visible while inline AskUserQuestion cards
        // are open; submitting text resolves the ask as a free-form response.
        // The editor pane still hides the inline composer while editing there.
        let footer = (composer_placement == ComposerPlacement::Footer).then(|| {
            v_flex()
                .w_full()
                .flex_shrink_0()
                .bg(theme.background)
                .py_2()
                .gap_2()
                .child(centered(gpui::div().w_full().h(px(1.)).bg(theme.border)))
                .children(self.render_attachments(&theme, cx))
                .child(centered(self.render_composer(running, window, &theme, cx)))
        });

        // Hero occupies the message-list region on the first screen.
        // Notice items on the first screen (e.g. mode-switch acknowledgement).
        // They are stored in the conversation but hidden behind the hero layout;
        // show them as a temporary banner below the composer so the user sees
        // the feedback without leaving the first-screen view.
        let hero_notices = if first_screen {
            self.conversation
                .read(cx)
                .items()
                .iter()
                .rev()
                .filter_map(|e| {
                    if let ConvItem::Error(msg) | ConvItem::Notice(msg) = e.read(cx).kind() {
                        Some(msg.clone())
                    } else {
                        None
                    }
                })
                .next()
        } else {
            None
        };
        let hero = if composer_placement != ComposerPlacement::Hero {
            None
        } else if loading {
            // History restore and drafting are independent: the progress label
            // describes the transcript while the disabled send action makes the
            // input gate explicit without delaying the editor itself.
            Some(
                v_flex()
                    .flex_1()
                    .w_full()
                    .justify_center()
                    .items_center()
                    .child(centered(
                        v_flex()
                            .w_full()
                            .gap_3()
                            .items_center()
                            .child(
                                crate::views::braille_spinner::BrailleSpinner::new()
                                    .color(theme.muted_foreground),
                            )
                            .child(
                                gpui::div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(i18n::t("workspace-loading-history")),
                            )
                            .children(self.render_attachments(&theme, cx))
                            .child(self.render_composer(running, window, &theme, cx)),
                    )),
            )
        } else {
            Some(
                v_flex()
                    .flex_1()
                    .w_full()
                    .justify_center()
                    .items_center()
                    .child(centered(
                        v_flex()
                            .w_full()
                            .gap_5()
                            .items_center()
                            .child(
                                gpui::div()
                                    .text_base()
                                    .font_weight(gpui::FontWeight::BLACK)
                                    .text_color(theme.foreground)
                                    .child(i18n::t("workspace-empty-prompt")),
                            )
                            .children(self.render_attachments(&theme, cx))
                            .child(self.render_composer(running, window, &theme, cx))
                            .children(hero_notices.map(|msg| {
                                gpui::div()
                                    .w_full()
                                    .px_3()
                                    .py_1p5()
                                    .rounded(theme.radius)
                                    .bg(theme.accent.opacity(0.1))
                                    .border_1()
                                    .border_color(theme.accent.opacity(0.2))
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(msg)
                            })),
                    )),
            )
        };

        // No chrome on the panel: Ctrl-G closes, Cmd-Enter sends, Cmd-Shift-P
        // toggles preview — all keyboard-driven per the no-button constraint.
        // The divider is the visual separator and the drag handle for resizing.
        let editor_divider = gpui::div()
            .id("editor-divider")
            .w(px(EDITOR_DIVIDER_WIDTH))
            .h_full()
            .flex_shrink_0()
            .relative()
            .cursor(CursorStyle::ResizeLeftRight)
            .child(
                gpui::div()
                    .absolute()
                    .left(px(2.5))
                    .w(px(1.))
                    .h_full()
                    .bg(theme.border),
            )
            .on_drag(DraggedEditorDivider, |_, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DraggedEditorDivider)
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, e: &MouseUpEvent, _, cx| {
                    // Double-click resets the pane to its default width.
                    if e.click_count >= 2 {
                        this.editor_width = px(EDITOR_PANEL_WIDTH);
                        cx.notify();
                    }
                }),
            );
        // The sidebar divider lives in `shell_root` — shared by every view
        // mode so the sidebar resizes identically everywhere.
        // Right pane is a peer tab container for the editor, launcher,
        // browser, sub-agent observers, and embedded sessions. The
        // top-level TabBar is built from `right_tabs`; the content below
        // dispatches on the active tab.
        let active_tab = self.right_tabs.get(self.active_right_tab).cloned();
        let hovered_tab = self.hovered_right_tab;
        let right_tab_children: Vec<Tab> = self
            .right_tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                // Full label rides the tooltip; the fixed-width tab shows the
                // capped form. Session tabs additionally carry their kind's
                // brand glyph as a prefix.
                let (full, icon_path): (SharedString, Option<&'static str>) = match tab {
                    RightTab::Editor => (i18n::t("member-editor-tab"), None),
                    RightTab::Launcher => (i18n::t("right-tab-launcher"), None),
                    RightTab::Browser(id) => {
                        // The page's <title>, polled by the host; the URL is
                        // the fallback before the first title lands.
                        let label = self
                            .browser_views
                            .get(id)
                            .map(|v| {
                                let view = v.read(cx);
                                let title = view.title();
                                if title.is_empty() {
                                    view.url().to_string()
                                } else {
                                    title.to_string()
                                }
                            })
                            .unwrap_or_default();
                        (i18n::t_str("browser-tab", &[("title", &label)]), None)
                    }
                    // The subagent's address (e.g. `Sailor_0`); the panel's
                    // banner carries the topic.
                    RightTab::Subagent(id) => (id.as_str().into(), None),
                    RightTab::Session(id) => {
                        let session = self.external_sessions.iter().find(|s| s.id == *id);
                        let label = session.map(|s| s.display_title()).unwrap_or_default();
                        (label, session.map(|s| s.kind.icon_asset()))
                    }
                };
                let mut base = Tab::new()
                    .label(cap_tab_label(&full))
                    .w(px(RIGHT_TAB_WIDTH))
                    .tooltip({
                        let full = full.clone();
                        move |window, cx| Tooltip::new(full.clone()).build(window, cx)
                    })
                    .on_hover(cx.listener(move |this, hovering: &bool, _window, cx| {
                        if *hovering {
                            this.hovered_right_tab = Some(ix);
                        } else if this.hovered_right_tab == Some(ix) {
                            this.hovered_right_tab = None;
                        }
                        cx.notify();
                    }));
                if let Some(path) = icon_path {
                    base = base.prefix(
                        Icon::default()
                            .path(path)
                            .xsmall()
                            .text_color(theme.muted_foreground),
                    );
                }
                // The close × reveals on hover for every tab kind. Routing
                // stays in `close_right_tab`: Editor keeps its draft-transfer
                // semantics, Session kills + tears the session down.
                if hovered_tab == Some(ix) {
                    base = base.suffix(
                        gpui::div()
                            .id(("right-tab-close", ix))
                            .cursor_pointer()
                            .child(
                                Icon::new(IconName::Close)
                                    .xsmall()
                                    .text_color(theme.muted_foreground),
                            )
                            // Stop the click from also selecting the tab
                            // underneath the ×.
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_right_tab(ix, window, cx);
                            })),
                    );
                }
                base
            })
            .collect();
        let editor_pane = v_flex()
            .w(editor_width)
            .h_full()
            .flex_shrink_0()
            .bg(theme.background)
            .child(
                h_flex().w_full().px_2().pt_1().items_center().child(
                    TabBar::new("right-tabs")
                        .underline()
                        .small()
                        .selected_index(self.active_right_tab)
                        .on_click(cx.listener(|this, ix: &usize, _window, cx| {
                            this.set_active_right_tab(*ix, cx);
                        }))
                        .children(right_tab_children)
                        .suffix(
                            Button::new("right-tab-new")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Plus)
                                .tooltip(i18n::t("right-tab-new"))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.open_launcher_tab(cx);
                                })),
                        ),
                ),
            )
            .child(
                gpui::div()
                    .id("right-pane-content")
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(match active_tab {
                        Some(RightTab::Editor) => v_flex()
                            .h_full()
                            .child(
                                h_flex().w_full().px_2().child(
                                    TabBar::new("editor-write-preview")
                                        .underline()
                                        .small()
                                        .selected_index(if editor_preview { 1 } else { 0 })
                                        .on_click(cx.listener(|this, ix: &usize, window, cx| {
                                            this.set_editor_preview(*ix == 1, window, cx);
                                        }))
                                        .child("Write")
                                        .child("Preview"),
                                ),
                            )
                            .child(if editor_preview {
                                // The preview entity is lazily created and kept stable
                                // across renders so the source is only re-parsed when
                                // the draft changes. The scroll lives on an explicit
                                // `ScrollHandle` + an outer `flex_1`-sized container
                                // — the message-list pattern — rather than the
                                // markdown entity's own `overflow_y_scroll`: an explicit
                                // handle keeps the offset pinned and defaulting to the
                                // top, and a flex-resolved (not `h_full`-percentage)
                                // scroll box reliably clips long content instead of
                                // letting it overflow and lose the first lines off the
                                // top.
                                let value = self.editor_state.read(cx).value().to_string();
                                let theme = cx.theme().clone();
                                if self.editor_preview_md.is_none() {
                                    self.editor_preview_md = Some(cx.new(|_cx| {
                                        Markdown::new("editor-preview", value.clone())
                                            .theme(&theme)
                                            .heading_mode(HeadingMode::Uniform)
                                            .body_size(crate::views::message::MESSAGE_BODY_SIZE)
                                    }));
                                }
                                let md = self
                                    .editor_preview_md
                                    .clone()
                                    .expect("preview md initialized above");
                                if md.read(cx).source() != value.as_str() {
                                    md.update(cx, |m, cx| m.replace(value, cx));
                                }
                                let scroll = self.editor_preview_scroll.clone();
                                gpui::div()
                                    .id("editor-preview-scroll")
                                    .w_full()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .track_scroll(&scroll)
                                    .child(gpui::div().w_full().p_4().child(md.into_any_element()))
                                    .into_any_element()
                            } else {
                                gpui::div()
                                    .w_full()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_hidden()
                                    .child(
                                        // The panel editor is a plain-text
                                        // composer for the same message
                                        // content, so it shares the inline
                                        // input's body typeface: Lilex Light
                                        // at MESSAGE_BODY_SIZE (13px).
                                        Input::new(&self.editor_state)
                                            .size_full()
                                            .appearance(false)
                                            .font_family(theme.mono_font_family.clone())
                                            .font_weight(gpui::FontWeight::LIGHT)
                                            .text_size(crate::views::message::MESSAGE_BODY_SIZE)
                                            .into_any_element(),
                                    )
                                    .into_any_element()
                            })
                            .into_any_element(),
                        Some(RightTab::Browser(id)) => self
                            .browser_views
                            .get(&id)
                            .map(|v| v.clone().into_any_element())
                            .unwrap_or_else(|| gpui::div().into_any_element()),
                        Some(RightTab::Subagent(id)) => self
                            .subagent_panels
                            .get(&id)
                            .map(|p| p.clone().into_any_element())
                            .unwrap_or_else(|| gpui::div().into_any_element()),
                        Some(RightTab::Launcher) => {
                            self.render_launcher_content(self.active_right_tab, cx)
                        }
                        Some(RightTab::Session(id)) => self
                            .external_sessions
                            .iter()
                            .find(|s| s.id == id)
                            .map(|s| s.terminal_view.clone().into_any_element())
                            .unwrap_or_else(|| gpui::div().into_any_element()),
                        None => gpui::div().into_any_element(),
                    }),
            );

        // The shared shell provides the sidebar + draggable divider and the
        // mode-switching actions; this mode chains the conversation-only
        // actions and the turn-navigator overlay onto it. The shell's main
        // slot is the main view: a two-column container holding the message
        // column (conversation + rail) and, when any right-pane tab is open,
        // the right side view (editor / launcher / browser / session tabs).
        // Bind the column to a local before the shell call: the column's
        // builder borrows `self` (title-menu trigger, context rail), which
        // would collide with `shell_root`'s `&mut self` receiver inside a
        // single call expression.
        self.sync_ask_card_snapshots(cx);
        let conversation_column = {
            v_flex()
                .flex_1()
                .h_full()
                .min_w_0()
                .relative()
                .overflow_hidden()
                .child({
                    // Body wrapper: hero / list / footer / overlay. `pt`
                    // reserves space for the title-bar overlay; `pr` (when
                    // the card is shown) reserves the floating card's width
                    // so the message list never hides behind it.
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .w_full()
                        .overflow_hidden()
                        .pt(TITLE_BAR_HEIGHT)
                        .pb_2()
                        .when(show_rail, |this| {
                            this.pr(px(crate::views::context_rail::ENV_CONTENT_INSET))
                        })
                        // Empty first screen shows the centered hero in place
                        // of the (empty) message list; otherwise a bottom-
                        // anchored, tail-following native list.
                        .children(hero)
                        .children({
                            // Keep the row factory a pure read-only projection.
                            // GPUI invokes it while measuring and prepainting;
                            // mutating a MessageItem here invalidates the same
                            // entity tree whose height is being cached.
                            let conversation = self.conversation.clone();
                            let diag_enabled = crate::overlap_diag::enabled();
                            let processor = move |ix: usize, _window: &mut Window, cx: &mut App| {
                                let item = conversation.read(cx).items().get(ix).cloned();
                                match item {
                                    // `flex_shrink_0` guards against any
                                    // available height leaking down the
                                    // flex chain and compressing a row.
                                    Some(item) => {
                                        if diag_enabled {
                                            crate::overlap_diag::record_mapping(
                                                ix,
                                                item.read(cx).diagnostic_id(),
                                            );
                                        }
                                        v_flex()
                                            .w_full()
                                            .pt_1()
                                            .pb_4()
                                            .flex_shrink_0()
                                            .min_w_0()
                                            .debug_selector(move || {
                                                format!("workspace-message-row-{ix}")
                                            })
                                            .when(diag_enabled, |this| {
                                                this.on_prepaint(move |bounds, _window, _cx| {
                                                    crate::overlap_diag::record_row(ix, bounds);
                                                })
                                            })
                                            .child(item)
                                            .into_any_element()
                                    }
                                    // Index out of range mid-splice (count
                                    // changed between a layout pass and the
                                    // render closure): render an empty row.
                                    None => gpui::div().into_any_element(),
                                }
                            };
                            let list_state = self.list_state.clone();
                            let width_state = self.list_state.clone();
                            let message_list_width = self.message_list_width.clone();
                            let diag_state = self.list_state.clone();
                            let mono_family = theme.mono_font_family.clone();
                            (!first_screen).then(move || {
                                // Native `gpui::list`: it owns virtualization,
                                // scroll, the per-item height cache, and tail-
                                // follow. Visible rows re-measure every frame;
                                // the wrapper below explicitly invalidates all
                                // cached heights after a width change. `Tail`
                                // mode pins to the live end and
                                // re-engages at the bottom after an upward
                                // scroll. Item heights are reconciled from the
                                // ThreadEvent handler via
                                // `splice`/`remeasure_items`.
                                let list_el = gpui::list(list_state, processor)
                                    .w_full()
                                    .h_full()
                                    .min_h_0()
                                    .min_w_0();
                                // Body typeface: Lilex Light. Every message row
                                // (assistant, user, reasoning, tool cards, notices)
                                // inherits from this wrapper div: gpui's List applies
                                // its own text refinements only while requesting its
                                // own layout, and with `Auto` sizing the item rows are
                                // laid out in prepaint outside that scope — so the
                                // family/weight must live on a wrapping div. Markdown
                                // bold/headings resolve to Medium via nearest-weight,
                                // italic syntax and tool-card overrides hit the
                                // italic cuts.
                                let list_wrap = v_flex()
                                    .flex_1()
                                    .h_full()
                                    .min_h_0()
                                    .min_w_0()
                                    .font_family(mono_family.clone())
                                    .font_weight(gpui::FontWeight::LIGHT)
                                    .child(list_el)
                                    .on_prepaint(move |bounds, window, _app| {
                                        if message_list_width
                                            .update(bounds.size.width, &width_state)
                                        {
                                            window.refresh();
                                        }
                                        if crate::overlap_diag::enabled() {
                                            crate::overlap_diag::check_completed_frame(
                                                bounds,
                                                diag_state.item_count(),
                                            );
                                        }
                                    });
                                h_flex()
                                    .flex_1()
                                    .w_full()
                                    .min_h_0()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .child(list_wrap)
                            })
                        })
                        .children(footer)
                        // Question card overlay (if any)
                        .children(overlay)
                })
                // Title-bar overlay: absolute top of the conversation column,
                // painted after the body so the "..." menu isn't covered by
                // the conversation list.
                .child(
                    gpui::div()
                        .absolute()
                        .top(px(0.))
                        .left(px(0.))
                        .right(px(0.))
                        .h(TITLE_BAR_HEIGHT)
                        .child(
                            TitleBar::new()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .flex_1()
                                        .min_w_0()
                                        .pr_4()
                                        .child(
                                            gpui::svg()
                                                .path("icons/manox.svg")
                                                .size(px(16.))
                                                .text_color(theme.muted_foreground),
                                        )
                                        .child(
                                            gpui::div()
                                                .text_sm()
                                                .text_left()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .child(title_text),
                                        ),
                                )
                                .child(
                                    h_flex().items_center().pr_2().child(
                                        Button::new("right-pane-toggle")
                                            .ghost()
                                            .xsmall()
                                            .icon(if right_pane_open {
                                                Icon::new(IconName::PanelRight)
                                            } else {
                                                Icon::default().path("icons/panel-right-dashed.svg")
                                            })
                                            .tooltip(i18n::t("right-pane-toggle"))
                                            .on_click(cx.listener(|this, _, _window, cx| {
                                                this.toggle_right_pane(cx);
                                            })),
                                    ),
                                ),
                        ),
                )
                // Floating context card: absolute top-right of the
                // conversation column, below the title bar. Its own `Render`
                // positions it (`top` clears the title bar, `right` + the
                // body wrapper's `pr` keep the message list clear). Hidden
                // while the editor pane is open, on the first screen, before
                // the thread interacts, or below the narrow width gate.
                .when(show_rail, |this| this.child(self.context_rail.clone()))
        };
        // The main view is the shell's main slot: the message column plus the
        // right side view (editor / launcher / browser / session tabs) as its
        // sub-columns. Nesting the right pane inside the main view keeps the
        // shell uniformly
        // `sidebar | divider | main view` across every view mode (Terminal /
        // ExternalSession / Settings pass a single-column main).
        let main_view = h_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .relative()
            .child(conversation_column)
            .when(right_pane_open, |this| {
                this.child(editor_divider).child(editor_pane)
            });
        let mut root = self.shell_root(self.sidebar.clone(), main_view, cx);
        root = root
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                this.enter_settings(window, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::ToggleEditor, window, cx| {
                this.toggle_editor(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &crate::ToggleEditorPreview, window, cx| {
                    this.toggle_editor_preview(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &crate::CloseEditor, window, cx| {
                this.close_editor(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleTurnNavigator, window, cx| {
                this.toggle_turn_navigator(window, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &OpenBrowserTab, window, cx| {
                this.open_browser_tab(crate::views::browser_view::DEFAULT_URL, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseBrowserTab, _window, cx| {
                this.close_active_browser_tab(cx);
            }))
            .on_action(
                cx.listener(|this, _: &crate::BackgroundCurrentThread, window, cx| {
                    this.background_current_thread(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &crate::UndoLastQueued, _window, cx| {
                this.undo_last_queued(cx);
            }))
            // Completion actions only match via the `completion == open > Input`
            // keybindings, so any fire means the popover was open and these
            // keystrokes belong to it. Stop propagation so the Input's own
            // parallel up/down/enter/tab/escape binding (same depth, lower
            // register index) doesn't also fire — otherwise Enter would both
            // confirm and submit, Up/Down would move caret and selection, etc.
            .on_action(cx.listener(|this, _: &crate::CompletionUp, window, cx| {
                this.completion_up(window, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &crate::CompletionDown, window, cx| {
                this.completion_down(window, cx);
                cx.stop_propagation();
            }))
            .on_action(
                cx.listener(|this, _: &crate::CompletionConfirm, window, cx| {
                    this.completion_confirm_selected(window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::CompletionDismiss, _window, cx| {
                    this.close_completion(cx);
                    cx.stop_propagation();
                }),
            )
            // Composer history recall: reachable only through the
            // `composer > Input` bindings on alt-up / alt-down, so these
            // listeners never see the bare arrows the Input uses to move the
            // caret.
            .on_action(
                cx.listener(|this, _: &crate::ComposerRecallUp, window, cx| {
                    this.composer_recall_up(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::ComposerRecallDown, window, cx| {
                    this.composer_recall_down(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::ArchiveCurrentThread, window, cx| {
                    this.archive_current_thread(window, cx);
                }),
            )
            // The right editor pane moved inside the shell's main view (the
            // `main_view` container above); it is no longer a top-level shell
            // column.
            .children(turn_navigator_overlay)
            .on_drag_move(cx.listener(
                |this, e: &DragMoveEvent<DraggedEditorDivider>, _window, cx| {
                    // The root fills the window, so its right edge is the
                    // window's right edge and the editor pane's width is the
                    // distance from the cursor to that edge. Clamp both to a
                    // minimum and to leave the message column at least
                    // `MAIN_MIN_WIDTH` (sidebar + divider + main view sit
                    // left of the editor), so dragging wide never overflows
                    // the window or collapses the conversation column. The
                    // context card is hidden while the editor is open, so it
                    // does not claim a width here — the conversation alone
                    // holds the message column. `sidebar_width` is read live
                    // so a wide sidebar correctly shrinks the available
                    // editor envelope.
                    let new_w = e.bounds.right() - e.event.position.x;
                    let dynamic_max = e.bounds.size.width
                        - this.sidebar_width
                        - px(EDITOR_DIVIDER_WIDTH)
                        - px(MAIN_MIN_WIDTH);
                    let max_w = dynamic_max
                        .min(px(EDITOR_MAX_WIDTH))
                        .max(px(EDITOR_MIN_WIDTH));
                    this.editor_width = new_w.clamp(px(EDITOR_MIN_WIDTH), max_w);
                    cx.notify();
                },
            ));
        root.into_any_element()
    }
    /// The shared window shell every full-window `ViewMode` renders through:
    /// `sidebar | sidebar-divider | main`, plus the mode-switching actions and
    /// the sidebar drag/reset handling. The divider (drag handle, double-click
    /// reset, width clamp + sync) lives here once, so the conversation,
    /// built-in terminal, external-session, and Settings pages all resize
    /// their sidebar identically — only the sidebar slot and main column
    /// differ per mode. The Settings page passes its own nav as the sidebar
    /// slot; dragging the divider there updates the same width state so the
    /// layout container behaves identically across pages.
    fn shell_root(
        &mut self,
        sidebar: impl IntoElement,
        main: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme().clone();
        // The divider is the visual separator and the drag handle for resizing
        // the sidebar. Double-click resets to the default `SIDEBAR_WIDTH` for
        // symmetry with the editor pane.
        let sync_width = |this: &mut Self, cx: &mut App, width: Pixels| {
            this.sidebar_width = width;
            this.sidebar.update(cx, |s, cx| s.set_width(width, cx));
            if let Some(settings) = this.settings_view.as_ref() {
                settings.update(cx, |s, cx| s.set_width(width, cx));
            }
        };
        let sidebar_divider = gpui::div()
            .id("sidebar-divider")
            .w(px(SIDEBAR_DIVIDER_WIDTH))
            .h_full()
            .flex_shrink_0()
            .relative()
            .cursor(CursorStyle::ResizeLeftRight)
            .child(
                gpui::div()
                    .absolute()
                    .left(px(2.5))
                    .w(px(1.))
                    .h_full()
                    .bg(theme.border),
            )
            .on_drag(DraggedSidebarDivider, |_, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DraggedSidebarDivider)
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseUpEvent, _, cx| {
                    if e.click_count >= 2 {
                        sync_width(this, cx, px(SIDEBAR_WIDTH));
                        cx.notify();
                    }
                }),
            );

        h_flex()
            .size_full()
            .relative()
            .bg(theme.background)
            .text_color(theme.foreground)
            // Mode-switching shortcuts apply in every view mode.
            .on_action(cx.listener(|this, _: &FocusConversation, _window, cx| {
                this.focus_conversation(cx);
            }))
            .on_action(cx.listener(|this, _: &FocusTerminal, _window, cx| {
                this.focus_terminal(cx);
            }))
            .on_action(cx.listener(|this, _: &NewTerminalTab, _window, cx| {
                this.open_terminal_tab(cx);
            }))
            .on_action(cx.listener(|this, _: &CloseTerminalTab, _window, cx| {
                this.close_terminal_tab(cx);
            }))
            .child(sidebar)
            .child(sidebar_divider)
            .child(main)
            .on_drag_move(cx.listener(
                move |this, e: &DragMoveEvent<DraggedSidebarDivider>, _window, cx| {
                    // The root fills the window, so the sidebar's right edge is
                    // the cursor's x position relative to the root's left.
                    // Clamp so the message column (and the editor pane when
                    // open) always retain at least `MAIN_MIN_WIDTH`.
                    let new_w = e.event.position.x - e.bounds.left();
                    let editor_reserve = if this.right_pane_open() {
                        this.editor_width + px(EDITOR_DIVIDER_WIDTH)
                    } else {
                        px(0.)
                    };
                    let dynamic_max = e.bounds.size.width
                        - px(SIDEBAR_DIVIDER_WIDTH)
                        - editor_reserve
                        - px(MAIN_MIN_WIDTH);
                    let max_w = dynamic_max
                        .min(px(SIDEBAR_MAX_WIDTH))
                        .max(px(SIDEBAR_MIN_WIDTH));
                    let clamped = new_w.clamp(px(SIDEBAR_MIN_WIDTH), max_w);
                    sync_width(this, cx, clamped);
                    cx.notify();
                },
            ))
    }

    /// The terminal-style main column shared by the built-in Terminal tab and
    /// external agent CLI sessions: a TitleBar (leading icon + title) over a
    /// full-bleed terminal view. One shape for both, so the two terminal
    /// surfaces read as peers inside the shared shell.
    fn render_terminal_column(
        &self,
        icon: AnyElement,
        title: SharedString,
        content: impl IntoElement,
    ) -> gpui::Div {
        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .relative()
            .child(
                TitleBar::new().child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .flex_1()
                        .min_w_0()
                        .child(icon)
                        .child(
                            gpui::div()
                                .text_sm()
                                .text_left()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(title),
                        ),
                ),
            )
            .child(v_flex().flex_1().h_full().w_full().child(content))
    }
}
