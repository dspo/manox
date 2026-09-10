//! The right observation pane (U9b cluster 5): terminal/editor/browser/
//! launcher/subagent tabs, the pane's persistence + per-thread stash/
//! restore, the launcher cascade, and the editor preview/submit legs.
//! Split from `workspace.rs` — `super` is the workspace module; the
//! bare-private methods are lifted `pub(super)` so the parent (render
//! handlers, keybindings) and its `tests` child keep calling them.

use super::*;

impl Workspace {
    /// Switch to the terminal pane, creating the terminal tab on first focus.
    /// The terminal runs in the workspace's cwd with the user's shell.
    pub fn focus_terminal(&mut self, cx: &mut Context<Self>) {
        if self.terminal_view.is_none() {
            let id = uuid::Uuid::new_v4().to_string();
            let pty = match manox_terminal::pty::default_source(&self.cwd, 80, 24) {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(error = ?e, "failed to open terminal pty");
                    return;
                }
            };
            let terminal = match Terminal::spawn(id, self.cwd.clone(), 80, 24, pty) {
                Ok(t) => cx.new(|cx| TerminalProxy::new(t, cx)),
                Err(e) => {
                    tracing::error!(error = ?e, "failed to spawn terminal");
                    return;
                }
            };
            self.terminal_view = Some(TerminalView::new(terminal, cx));
        }
        self.view_mode = ViewMode::Terminal;
        cx.notify();
    }

    pub(super) fn toggle_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor_open {
            self.close_editor(window, cx);
        } else {
            self.open_editor(window, cx);
        }
    }

    /// Whether the right pane is actually on screen: the visibility gate is
    /// up AND at least one tab exists. Everything layout-related (main view
    /// sub-columns, composer/rail suppression, width math) keys off this.
    pub(super) fn right_pane_open(&self) -> bool {
        self.right_pane_visible && !self.right_tabs.is_empty()
    }

    /// Closing the last tab hides the pane; the TitleBar toggle restores the
    /// surviving tabs on the next open.
    pub(super) fn hide_right_pane_if_empty(&mut self, cx: &mut Context<Self>) {
        if self.right_tabs.is_empty() {
            self.right_pane_visible = false;
        }
        if self
            .hovered_right_tab
            .is_some_and(|ix| ix >= self.right_tabs.len())
        {
            self.hovered_right_tab = None;
        }
        self.persist_right_pane(cx);
    }

    /// Snapshot the live pane into its persistable form: subagent tabs drop
    /// out (ephemeral) and the active index remaps into the filtered list
    /// (falling back to the head when the active tab was one).
    pub(super) fn persisted_right_pane(&self, cx: &App) -> PersistedRightPane {
        let mut tabs: Vec<PersistedRightTab> = Vec::new();
        let mut active = 0usize;
        for (ix, tab) in self.right_tabs.iter().enumerate() {
            let persisted = match tab {
                RightTab::Editor => Some(PersistedRightTab::Editor),
                RightTab::Launcher => Some(PersistedRightTab::Launcher),
                RightTab::Browser(id) => {
                    let url = self
                        .browser_views
                        .get(id)
                        .map(|v| v.read(cx).url().to_string())
                        .unwrap_or_default();
                    Some(PersistedRightTab::Browser { url })
                }
                RightTab::Session(id) => Some(PersistedRightTab::Session { id: id.clone() }),
                RightTab::Subagent(_) => None,
            };
            if let Some(p) = persisted {
                if ix == self.active_right_tab {
                    active = tabs.len();
                }
                tabs.push(p);
            }
        }
        PersistedRightPane {
            visible: self.right_pane_visible,
            active,
            tabs,
        }
    }

    /// Write the foreground thread's live pane state to `threads.db`. The
    /// mutation primitives (tab activate/close, visibility toggle) call this,
    /// so the persisted row tracks the pane on every change.
    pub(super) fn persist_right_pane(&mut self, cx: &mut Context<Self>) {
        let thread_id = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.id.0.clone())
            .expect("foreground store present");
        let persisted = self.persisted_right_pane(cx);
        let json = match serde_json::to_string(&persisted) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "serialize right pane failed");
                return;
            }
        };
        let db = manox_agent::thread_store_global().read(|s| s.db().clone());
        if let Err(e) = db.upsert_right_pane(&thread_id, &json) {
            tracing::warn!(error = %e, thread_id = %thread_id, "persist right pane failed");
        }
    }

    /// Per-thread isolation: move the outgoing thread's live pane (tabs,
    /// active index, visibility) into the in-session stash and persist it to
    /// `threads.db`, then reset the pane to the empty state for the incoming
    /// thread.
    pub(super) fn stash_right_pane(&mut self, thread_id: String, cx: &mut Context<Self>) {
        let persisted = self.persisted_right_pane(cx);
        let json = serde_json::to_string(&persisted)
            .inspect_err(|e| tracing::warn!(error = %e, "serialize right pane failed"))
            .ok();
        if let Some(json) = json {
            let db = manox_agent::thread_store_global().read(|s| s.db().clone());
            if let Err(e) = db.upsert_right_pane(&thread_id, &json) {
                tracing::warn!(error = %e, thread_id = %thread_id, "persist right pane failed");
            }
        }
        // The cascade belongs to the outgoing thread's launcher row; a
        // leftover popup would resurface stale (and with a stale tab index)
        // when the pane restores.
        self.close_launcher_menu();
        self.right_pane_by_thread.insert(
            thread_id,
            RightPaneSnapshot {
                tabs: std::mem::take(&mut self.right_tabs),
                active: self.active_right_tab,
                visible: self.right_pane_visible,
            },
        );
        self.active_right_tab = 0;
        self.right_pane_visible = false;
        self.hovered_right_tab = None;
        self.editor_open = false;
    }

    /// Restore the incoming thread's pane: the in-session stash first (live
    /// tabs keep their webview/panel entities), else the `threads.db`
    /// snapshot re-materialized; a thread with neither gets the empty hidden
    /// pane.
    pub(super) fn restore_right_pane(
        &mut self,
        thread_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.right_pane_by_thread.remove(thread_id) {
            Some(snapshot) => {
                // A stashed tab may outlive its backing entity: browser
                // views can be closed and external sessions can exit while
                // another thread is foreground. Drop the orphaned tabs.
                let mut tabs = snapshot.tabs;
                tabs.retain(|t| match t {
                    RightTab::Browser(id) => self.browser_views.contains_key(id),
                    RightTab::Session(id) => self.external_sessions.iter().any(|s| s.id == *id),
                    _ => true,
                });
                self.right_tabs = tabs;
                self.right_pane_visible = snapshot.visible;
                self.hovered_right_tab = None;
                if self.right_tabs.is_empty() {
                    self.active_right_tab = 0;
                    self.right_pane_visible = false;
                    self.editor_open = false;
                } else {
                    let active = snapshot.active.min(self.right_tabs.len() - 1);
                    self.set_active_right_tab(active, cx);
                }
            }
            None => {
                let loaded =
                    manox_agent::thread_store_global().read(|s| s.db().load_right_pane(thread_id));
                let persisted = match loaded {
                    Ok(Some(json)) => match serde_json::from_str::<PersistedRightPane>(&json) {
                        Ok(p) => Some(p),
                        Err(e) => {
                            tracing::warn!(error = %e, thread_id = %thread_id, "decode right pane snapshot failed");
                            None
                        }
                    },
                    Ok(None) => None,
                    Err(e) => {
                        tracing::warn!(error = %e, thread_id = %thread_id, "load right pane snapshot failed");
                        None
                    }
                };
                self.right_tabs.clear();
                self.hovered_right_tab = None;
                if let Some(persisted) = persisted {
                    self.materialize_right_pane(persisted, window, cx);
                } else {
                    self.active_right_tab = 0;
                    self.right_pane_visible = false;
                    self.editor_open = false;
                }
            }
        }
    }

    /// Rebuild a persisted pane into live tabs. Browser tabs recreate their
    /// webview from the stored URL (the app-restart path); session tabs
    /// restore only while the external session is still alive — everything
    /// else drops silently.
    pub(super) fn materialize_right_pane(
        &mut self,
        persisted: PersistedRightPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for tab in persisted.tabs {
            match tab {
                PersistedRightTab::Editor => self.right_tabs.push(RightTab::Editor),
                PersistedRightTab::Launcher => self.right_tabs.push(RightTab::Launcher),
                PersistedRightTab::Browser { url } => {
                    let id = self.restore_browser_tab(&url, window, cx);
                    self.right_tabs.push(RightTab::Browser(id));
                }
                PersistedRightTab::Session { id } => {
                    if self.external_sessions.iter().any(|s| s.id == id) {
                        self.right_tabs.push(RightTab::Session(id));
                    }
                }
            }
        }
        self.right_pane_visible = persisted.visible && !self.right_tabs.is_empty();
        if self.right_tabs.is_empty() {
            self.active_right_tab = 0;
            self.editor_open = false;
        } else {
            let active = persisted.active.min(self.right_tabs.len() - 1);
            self.set_active_right_tab(active, cx);
        }
    }

    /// Recreate a persisted browser tab's webview (app-restart path): build
    /// the view, register it in the host routing table, arm the title poll —
    /// tab-list placement is the caller's.
    pub(super) fn restore_browser_tab(
        &mut self,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> BrowserTabId {
        let url = if url.is_empty() {
            crate::views::browser_view::DEFAULT_URL
        } else {
            url
        };
        let tab_id = crate::views::browser_view::allocate_tab_id();
        let view =
            cx.new(|cx| crate::views::browser_view::BrowserView::new(tab_id, url, window, cx));
        self.browser_views.insert(tab_id, view);
        if let Some(host) = crate::browser_host::WorkspaceBrowserHost::concrete() {
            host.register_ui_tab(tab_id);
        }
        self.spawn_browser_title_ticker(cx);
        tab_id
    }

    /// Toggle the right pane's visibility from the TitleBar button. Hiding
    /// keeps every tab alive; showing with an empty tab list lands on a fresh
    /// Launcher tab so the button always opens something real.
    pub(super) fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        if self.right_pane_open() {
            self.right_pane_visible = false;
        } else {
            self.right_pane_visible = true;
            if self.right_tabs.is_empty() {
                self.right_tabs.push(RightTab::Launcher);
                self.active_right_tab = 0;
                self.editor_open = false;
            }
        }
        self.persist_right_pane(cx);
        cx.notify();
    }

    /// Open (or focus) an empty Launcher tab.
    pub(super) fn open_launcher_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self
            .right_tabs
            .iter()
            .position(|t| matches!(t, RightTab::Launcher))
        {
            self.right_pane_visible = true;
            self.set_active_right_tab(ix, cx);
            return;
        }
        self.right_pane_visible = true;
        self.right_tabs.push(RightTab::Launcher);
        self.set_active_right_tab(self.right_tabs.len() - 1, cx);
    }

    /// The Launcher tab's body: five centered integration shortcuts, with the
    /// open provider→model cascade anchored under its CLI-agent row.
    pub(super) fn render_launcher_content(
        &mut self,
        launcher_ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let weak = cx.weak_entity();
        let on_pick = move |pick: LauncherPick, window: &mut Window, cx: &mut App| {
            if let Some(ws) = weak.upgrade() {
                ws.update(cx, |this, cx| {
                    this.launcher_pick(pick, launcher_ix, window, cx)
                });
            }
        };
        let dropdown = self.launcher_menu_kind.zip(self.launcher_menu.clone());
        crate::views::launcher::render_launcher_tab(&theme, Rc::new(on_pick), dropdown)
    }

    /// Route a launcher shortcut to its view, opening it on the launcher's own
    /// tab. Terminal and the three CLI agents spawn at the active thread's
    /// cwd; CLI agents pick their provider/model through the cascade first.
    pub(super) fn launcher_pick(
        &mut self,
        pick: LauncherPick,
        launcher_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match pick {
            LauncherPick::Browser => {
                if matches!(self.right_tabs.get(launcher_ix), Some(RightTab::Launcher)) {
                    self.right_tabs.remove(launcher_ix);
                    self.reseat_active_after_close(launcher_ix, cx);
                }
                let tab_id =
                    self.open_browser_tab(crate::views::browser_view::DEFAULT_URL, window, cx);
                // Keep the browser view on the launcher's own tab slot.
                if let Some(new_ix) = self
                    .right_tabs
                    .iter()
                    .position(|t| matches!(t, RightTab::Browser(id) if *id == tab_id))
                {
                    let insert_at = launcher_ix.min(self.right_tabs.len() - 1);
                    if new_ix != insert_at {
                        let tab = self.right_tabs.remove(new_ix);
                        self.right_tabs.insert(insert_at, tab);
                    }
                    self.set_active_right_tab(insert_at, cx);
                }
            }
            LauncherPick::Terminal => {
                let cwd = self.launcher_thread_cwd(cx);
                self.spawn_plain_session(
                    SessionKind::Terminal,
                    cwd,
                    SessionPlacement::RightPane {
                        replace_tab: launcher_ix,
                    },
                    window,
                    cx,
                );
            }
            LauncherPick::Agent(kind) => {
                self.open_launcher_cascade(kind, launcher_ix, window, cx);
            }
        }
    }

    /// The active thread's cwd for launcher spawns (`None` when unset — the
    /// spawn paths fall back to the workspace cwd, parity with the
    /// Conversations-header spawns).
    pub(super) fn launcher_thread_cwd(&self, cx: &App) -> Option<PathBuf> {
        let cwd = self
            .store
            .as_ref()
            .map(|s| std::path::PathBuf::from(s.read(cx).store.cwd.clone()))
            .expect("foreground store present");
        if cwd.as_os_str().is_empty() {
            None
        } else {
            Some(cwd)
        }
    }

    /// Open the provider→model cascade for a launcher CLI-agent row. Mirrors
    /// the model-selector popup pattern: entity + dismiss subscription,
    /// dropped on close.
    pub(super) fn open_launcher_cascade(
        &mut self,
        kind: SessionKind,
        launcher_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.launcher_menu_kind == Some(kind) {
            self.close_launcher_menu();
            cx.notify();
            return;
        }
        self.close_launcher_menu();
        let cwd = self.launcher_thread_cwd(cx);
        // The menu-build closure below runs EAGERLY inside the click
        // handler's Workspace update lease (launcher_pick is still on
        // the stack), so the models are read here, up front — reading
        // the Workspace entity from inside the closure double-leases it
        // and aborts the app (the launcher twin of the sidebar's
        // acceptance-run crash; every CLI-agent row click was a hard
        // crash until this hoist). The multiplexer is a separate entity,
        // so this read is lease-clean.
        let models: Vec<manox_protocol::ModelInfo> = self.multiplexer.read(cx).models().to_vec();
        let ws = cx.entity().downgrade();
        let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
            // U2 cross-domain #4: the cascade projects the multiplexer's
            // wire models (the provider_glue direct read retired).
            crate::views::model_cascade::build_model_cascade(
                menu,
                kind.agent_id(),
                &models,
                window,
                cx,
                move |provider, model, wire, window, cx| {
                    if let Some(ws) = ws.upgrade() {
                        ws.update(cx, |this, cx| {
                            this.close_launcher_menu();
                            this.spawn_external_session(
                                ExternalSpawn {
                                    kind,
                                    provider_name: provider,
                                    model_id: model,
                                    wire_api: wire,
                                    project_cwd: cwd.clone(),
                                },
                                SessionPlacement::RightPane {
                                    replace_tab: launcher_ix,
                                },
                                window,
                                cx,
                            );
                            cx.notify();
                        });
                    }
                },
            )
        });
        let sub = cx.subscribe(&menu, |this, _menu, _: &DismissEvent, cx| {
            this.close_launcher_menu();
            cx.notify();
        });
        self.launcher_menu = Some(menu);
        self.launcher_menu_sub = Some(sub);
        self.launcher_menu_kind = Some(kind);
        cx.notify();
    }

    pub(super) fn close_launcher_menu(&mut self) {
        self.launcher_menu = None;
        self.launcher_menu_sub = None;
        self.launcher_menu_kind = None;
    }

    /// Index of the Editor tab, if present.
    pub(super) fn editor_tab_ix(&self) -> Option<usize> {
        self.right_tabs
            .iter()
            .position(|t| matches!(t, RightTab::Editor))
    }

    /// Make `ix` the active right-pane tab and sync `editor_open` to whether it
    /// is the Editor tab (the Editor tab hides the inline composer; a Member
    /// tab leaves it usable).
    pub(super) fn set_active_right_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.right_tabs.len() {
            self.active_right_tab = ix;
        }
        self.editor_open = self
            .right_tabs
            .get(self.active_right_tab)
            .is_some_and(|t| matches!(t, RightTab::Editor));
        self.persist_right_pane(cx);
        cx.notify();
    }

    /// Keep `active_right_tab` pointing at the same tab after the tab at
    /// `removed_ix` was just removed from `right_tabs`. Tabs after
    /// `removed_ix` shift left by one, so a still-live active tab to the right
    /// must decrement; the active tab itself being removed (and being the
    /// last) falls back to the new last tab.
    pub(super) fn reseat_active_after_close(&mut self, removed_ix: usize, cx: &mut Context<Self>) {
        if self.active_right_tab > removed_ix {
            self.active_right_tab -= 1;
        } else if self.active_right_tab >= self.right_tabs.len() {
            self.active_right_tab = self.right_tabs.len().saturating_sub(1);
        }
        // Tab indices shifted — the cached hover index would name the wrong
        // tab until the next mouse move recomputes it.
        self.hovered_right_tab = None;
        self.persist_right_pane(cx);
    }

    /// Open the markdown editor: hide the inline composer and transfer its draft
    /// into the editor so writing continues there. If an Editor tab is already
    /// present, just focus it. Submit from the editor with Cmd-Enter; close with
    /// Ctrl-G / Cmd-W to move the draft back.
    pub(super) fn open_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Close any open inline menus so they don't linger behind the hidden footer.
        self.close_completion(cx);
        self.close_plus_menu();
        self.right_pane_visible = true;
        if let Some(ix) = self.editor_tab_ix() {
            self.set_active_right_tab(ix, cx);
            return;
        }
        let draft = self.input_state.read(cx).value().to_string();
        self.right_tabs.push(RightTab::Editor);
        let ix = self.right_tabs.len() - 1;
        self.editor_preview = false;
        self.editor_preview_md = None;
        self.editor_state.update(cx, |s, cx| {
            s.set_value(draft, window, cx);
            s.focus(window, cx);
        });
        self.input_state
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.set_active_right_tab(ix, cx);
    }

    /// Close the Editor tab without submitting: move the draft back into the
    /// inline composer and reveal it again. No-op when no Editor tab is present.
    pub(super) fn close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.editor_tab_ix() else {
            return;
        };
        let draft = self.editor_state.read(cx).value().to_string();
        self.right_tabs.remove(ix);
        self.editor_preview = false;
        self.editor_preview_md = None;
        self.input_state.update(cx, |s, cx| {
            s.set_value(draft, window, cx);
            s.focus(window, cx);
        });
        self.editor_state
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.reseat_active_after_close(ix, cx);
        self.hide_right_pane_if_empty(cx);
        self.editor_open = self
            .right_tabs
            .get(self.active_right_tab)
            .is_some_and(|t| matches!(t, RightTab::Editor));
        cx.notify();
    }

    /// Close a right-pane tab by index. The Editor tab routes through
    /// `close_editor` (draft-transfer semantics); a Session tab routes
    /// through `close_external_session` (kill + teardown, whose removal path
    /// drops the tab); the rest remove in place.
    pub(super) fn close_right_tab(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.right_tabs.get(ix).cloned() {
            Some(RightTab::Editor) => self.close_editor(window, cx),
            Some(RightTab::Browser(id)) => self.close_browser_tab(id, cx),
            Some(RightTab::Session(id)) => self.close_external_session(&id, cx),
            Some(RightTab::Subagent(id)) => {
                self.subagent_panels.remove(&id);
                self.right_tabs.remove(ix);
                self.reseat_active_after_close(ix, cx);
                self.hide_right_pane_if_empty(cx);
                cx.notify();
            }
            Some(RightTab::Launcher) | None => {
                if ix < self.right_tabs.len() {
                    self.close_launcher_menu();
                    self.right_tabs.remove(ix);
                    self.reseat_active_after_close(ix, cx);
                    self.hide_right_pane_if_empty(cx);
                    cx.notify();
                }
            }
        }
    }

    /// Close the active right-pane tab if it is a browser tab. No-op
    /// otherwise (the keybinding is global; it should not close an Editor
    /// tab that happens to be active).
    pub fn close_active_browser_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(RightTab::Browser(id)) = self.right_tabs.get(self.active_right_tab).cloned() {
            self.close_browser_tab(id, cx);
        }
    }

    /// Open (or focus) a sub-agent observation panel in the right pane. The
    /// tab label is the subagent's address (`id`); the panel's banner shows
    /// `topic` — the shared `subagent_topic` derivation the rail and
    /// conversation rows use.
    pub(crate) fn open_subagent_tab(
        &mut self,
        id: &str,
        subagent_type: &str,
        topic: &str,
        status: manox_agent::ToolCallStatus,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self
            .right_tabs
            .iter()
            .position(|t| matches!(t, RightTab::Subagent(i) if i == id))
        {
            self.set_active_right_tab(ix, cx);
            return;
        }
        let backfill = self
            .subagent_transcripts
            .get(id)
            .cloned()
            .unwrap_or_default();
        let final_text = if backfill.is_empty() {
            self.agent_final_text(id, cx)
                .or_else(|| self.subagent_final_text.get(id).cloned())
        } else {
            None
        };
        let banner = if topic.is_empty() {
            id.to_string()
        } else {
            topic.to_string()
        };
        let recipient = if subagent_type.is_empty() {
            "sub-agent".to_string()
        } else {
            subagent_type.to_string()
        };
        // Transcript rows name the model that runs the child: the child reports
        // its resolved model at dispatch (`SubagentChildEvent::Model`), and the
        // parent's live label stands in until one is known.
        let role = backfill
            .iter()
            .find_map(|event| match event {
                manox_agent::SubagentChildEvent::Model(model) => Some(model.clone()),
                _ => None,
            })
            .unwrap_or_else(|| self.model_label(cx));
        let prompt = self.subagent_prompts.get(id).cloned();
        let panel = crate::views::subagent_panel::SubagentPanel::new(
            banner,
            role,
            recipient,
            status,
            &backfill,
            prompt.map(|p| (p.text, p.dispatched_at)),
            final_text,
            cx.weak_entity(),
            cx,
        );
        self.subagent_panels.insert(id.to_string(), panel);
        self.right_pane_visible = true;
        self.right_tabs.push(RightTab::Subagent(id.to_string()));
        self.set_active_right_tab(self.right_tabs.len() - 1, cx);
        cx.notify();
    }

    /// Final answer of a finished Agent call, for panels opened after a
    /// reload when no live transcript was accumulated. T10c: reads the v2
    /// display fold (the message rows ARE the transcript).
    pub(super) fn agent_final_text(&self, id: &str, cx: &App) -> Option<String> {
        use manox_agent::language_model::MessageContent;
        self.store
            .as_ref()
            .map(|s| s.read(cx).store.derived_messages())
            .expect("foreground store present")
            .iter()
            .flat_map(|m| m.content.iter())
            .find_map(|c| match c {
                MessageContent::ToolResult(r) if r.tool_use_id == id => Some(r.content.clone()),
                _ => None,
            })
    }

    /// Drop per-thread sub-agent observation state and close its tabs.
    pub(super) fn clear_subagent_observation(&mut self, cx: &mut Context<Self>) {
        self.subagent_panels.clear();
        self.subagent_transcripts.clear();
        let before = self.right_tabs.len();
        // Bulk removal shifts surviving tabs left by the number of dropped
        // tabs that sat before the active index; a plain OOB clamp would land
        // on the wrong tab when several sub-agent tabs precede it.
        let removed_before_active = self
            .right_tabs
            .iter()
            .take(self.active_right_tab.min(before))
            .filter(|t| matches!(t, RightTab::Subagent(_)))
            .count();
        self.right_tabs
            .retain(|t| !matches!(t, RightTab::Subagent(_)));
        if self.right_tabs.len() != before {
            self.active_right_tab = self
                .active_right_tab
                .saturating_sub(removed_before_active)
                .min(self.right_tabs.len().saturating_sub(1));
            self.editor_open = self
                .right_tabs
                .get(self.active_right_tab)
                .is_some_and(|t| matches!(t, RightTab::Editor));
        }
        self.hide_right_pane_if_empty(cx);
    }

    /// Rebuild the per-thread sub-agent observation state (rail rows + panel
    /// prompt/final-text) from the restored transcript. Live rows are fed by
    /// `SubagentProgress` events, which die with the process; this recovers
    /// the settled rows after a restart or a thread switch-back so the rail
    /// and panels are not left empty. Idempotent: rows upsert by address.
    /// Takes precomputed rows (derived inside the store-read closure) so the
    /// callers never clone the full transcript just for this scan.
    pub(super) fn apply_subagent_rows(
        &mut self,
        rows: Vec<manox_agent::subagent_restore::RestoredSubagent>,
        cx: &mut Context<Self>,
    ) {
        for row in rows {
            let first_line = manox_agent::steer_bus::first_line(&row.prompt);
            self.subagent_prompts.insert(
                row.address.clone(),
                SubagentPrompt {
                    text: row.prompt.clone(),
                    dispatched_at: row.dispatched_at,
                },
            );
            if let Some(text) = &row.final_text {
                self.subagent_final_text
                    .insert(row.address.clone(), text.clone());
            }
            self.context_rail.update(cx, |r, cx| {
                r.apply_subagent_progress(
                    &row.address,
                    &row.subagent_type,
                    first_line.as_deref(),
                    row.status,
                    None,
                    cx,
                );
            });
        }
    }

    /// Toggle the right-side composer between plain-text edit and rendered
    /// markdown preview. No-op when the panel is closed.
    pub(super) fn toggle_editor_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_editor_preview(!self.editor_preview, window, cx);
    }

    /// Switch the editor panel to preview (`Write` tab) or rendered markdown
    /// (`Preview` tab). No-op when the panel is closed or already in that mode.
    /// Returning to `Write` focuses the editor so typing works immediately.
    pub(super) fn set_editor_preview(
        &mut self,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editor_open || self.editor_preview == preview {
            return;
        }
        self.editor_preview = preview;
        if !preview {
            self.editor_state.update(cx, |s, cx| s.focus(window, cx));
        }
        cx.notify();
    }

    /// Submit the editor text to the thread, then close the panel and return
    /// focus to the inline input.
    pub(super) fn submit_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor_state.read(cx).value().to_string();
        if !editor_can_submit(
            // T10c: the v1 `history_phase` loading gate retired with the
            // fold (the restore boundary is now the §D.1 snapshot).
            false,
            self.store
                .as_ref()
                .map(|s| s.read(cx).store.running)
                .expect("foreground store present"),
            self.pending_ask.is_some(),
            &text,
        ) {
            return;
        }
        let meta = self.user_turn_meta(cx);
        let weak = cx.weak_entity();
        self.conversation.update(cx, |c, cx| {
            c.push_user(text.clone(), Vec::new(), meta, weak, cx)
        });
        self.sync_list_count(cx);
        self.follow_message_tail();
        let _ = self.send_submit_v2(text.clone(), Vec::new(), cx);
        self.multiplexer.update(cx, |m, _| m.fetch_thread_list());
        self.editor_state.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        // Drop the Editor tab (the turn is submitted); re-anchor to any
        // surviving Member tab and clear the draft-backed editor state.
        if let Some(ix) = self.editor_tab_ix() {
            self.right_tabs.remove(ix);
        }
        if self.active_right_tab >= self.right_tabs.len() {
            self.active_right_tab = self.right_tabs.len().saturating_sub(1);
        }
        self.editor_open = self
            .right_tabs
            .get(self.active_right_tab)
            .is_some_and(|t| matches!(t, RightTab::Editor));
        self.editor_preview = false;
        self.editor_preview_md = None;
        self.input_state.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }
}
