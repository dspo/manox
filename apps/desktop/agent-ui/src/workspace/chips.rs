//! The chip + interaction-card families (U9b cluster 6): the ask/auth
//! card surface (resurface, snapshots, option toggle/prev/next,
//! resolve-with-response, the auth resolve leg), the model-selector pi
//! face (wire tag/color, the canonical identity/display resolvers, the
//! selector render + popup builder), and the goal chip (popover open,
//! the edit/budget/rounds/replace/new begins, the chip render). Split
//! from `workspace.rs` — `super` is the workspace module; the
//! bare-private methods lift `pub(super)` for the parent's render
//! handlers and the `tests` child.

use super::*;

impl Workspace {
    /// Re-surface any pending interaction on the current thread that was
    /// emitted while the thread was in the background (no subscription). Called
    /// after switching threads so the question card appears without requiring
    /// the user to wait for the next event.
    pub(super) fn resurface_pending_auths(&mut self, cx: &mut Context<Self>) {
        // Query the thread for any pending interaction metadata that was
        // stored when the auth event was originally emitted. If the thread was
        // parked waiting for a user answer while in the background, re-surface
        // the events so the question card appears immediately upon switching
        // back. Every interaction surfaces as the AskUserQuestion card.
        let entries: Vec<(String, String, String, serde_json::Value)> = self
            .thread
            .read(|t| t.pending_auth_entries())
            .into_iter()
            .map(|(id, meta)| (id, meta.tool_name, meta.summary, meta.input))
            .collect();
        for (id, tool_name, summary, input) in entries {
            self.pending_ask = parse_pending_ask(id.clone(), input.clone());
            self.pending_auth = self.pending_ask.is_none().then(|| PendingAuth {
                id: id.clone(),
                tool_name,
                summary: summary.clone(),
            });
            self.ask_step = 0;
            self.ask_transition_gen = self.ask_transition_gen.wrapping_add(1);
            // The card renders on a top-level `ToolCall` item. The rebuilt
            // conversation may lack it (a parked thread never saw the live
            // `ToolCall` event, or a stale mirror missed the ask). Synthesize
            // the card the gate would have created live so the interactive
            // drawer renders.
            if self.pending_ask.is_some() {
                self.ensure_ask_tool_item(&id, &summary, input, cx);
            }
        }
        if self.pending_ask.is_some() || self.pending_auth.is_some() {
            cx.notify();
        }
    }

    /// Synthesize the top-level AskUserQuestion card when the rebuilt
    /// conversation lacks the matching `ToolCall` item. The live card is
    /// created by the gate's `ToolCall` event, which a parked thread never
    /// sees; the rebuild can miss the mirror's sync window. Without the item
    /// the ask snapshot cannot attach, so the interaction UI never renders.
    pub(super) fn ensure_ask_tool_item(
        &mut self,
        id: &str,
        summary: &str,
        input: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        if self.conversation.read(cx).find_tool(id, cx).is_some() {
            return;
        }
        let title = if summary.trim().is_empty() {
            i18n::t("workspace-clarify-title").to_string()
        } else {
            summary.to_string()
        };
        let role = self.model_label(cx);
        let weak = cx.weak_entity();
        self.conversation.update(cx, |conversation, cx| {
            conversation.push_tool_call(
                crate::conversation::ToolCallItem {
                    id: id.to_string(),
                    name: manox_agent::tools::ASK_USER_QUESTION.to_string(),
                    title,
                    status: manox_agent::thread::ToolCallStatus::PendingApproval,
                    output: String::new(),
                    is_error: false,
                    input,
                    streaming: false,
                    collapsed: false,
                    user_toggled: false,
                    panel: None,
                },
                role,
                weak,
                cx,
            );
        });
        self.sync_list_count(cx);
        self.list_state.set_follow_mode(FollowMode::Tail);
    }

    pub(crate) fn resolve_auth(&mut self, decision: PermissionDecision, cx: &mut Context<Self>) {
        // An AskUserQuestion card's "Cancel" button calls this; the generic
        // approval card's verdicts land here too. Whichever pending surface
        // is up owns the round-trip id.
        let id = match (self.pending_ask.as_ref(), self.pending_auth.as_ref()) {
            (Some(ask), _) => ask.id.clone(),
            (None, Some(auth)) => auth.id.clone(),
            (None, None) => return,
        };
        self.pending_ask = None;
        self.pending_auth = None;
        self.ask_step = 0;
        self.ask_transition_gen = self.ask_transition_gen.wrapping_add(1);
        let allow = matches!(decision, PermissionDecision::AllowOnce);
        if let Some(msg_id) = self
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.pending_auth.get(&id).cloned())
        {
            self.client
                .send_reply(msg_id, Ok(serde_json::json!({ "allow": allow })));
            return;
        }
        self.thread.with_mut(|thread| {
            thread.respond_authorization(
                &id,
                manox_agent::ToolAuthorizationResponse::Decision(decision),
            );
        });
        cx.notify();
    }

    /// Toggle an option in the pending ask card. Single-select questions reset
    /// siblings; multi-select toggles in place.
    pub(crate) fn ask_card_snapshot(&self, id: &str, _cx: &App) -> Option<AskCardSnapshot> {
        let ask = self.pending_ask.as_ref()?;
        if ask.id != id || ask.questions.is_empty() {
            return None;
        }
        let step = self.ask_step.min(ask.questions.len() - 1);
        let q = ask.questions.get(step)?;
        Some(AskCardSnapshot {
            id: ask.id.clone(),
            step,
            total: ask.questions.len(),
            transition_gen: self.ask_transition_gen,
            question: AskCardQuestion {
                question: q.question.clone(),
                header: q.header.clone(),
                multi_select: q.multi_select,
                options: q
                    .options
                    .iter()
                    .map(|o| AskCardOption {
                        label: o.label.clone(),
                        description: o.description.clone(),
                        recommended: o.recommended,
                    })
                    .collect(),
            },
            selections: ask.selections.get(step).cloned().unwrap_or_default(),
        })
    }

    /// Synchronize Workspace-derived ask state before the native list starts
    /// measuring. Updating a `MessageItem` from the row callback dirties the
    /// same entity tree whose height GPUI is currently caching, which can make
    /// the cached row and the painted subtree describe different layouts.
    pub(super) fn sync_ask_card_snapshots(&mut self, cx: &mut App) {
        let next = self.pending_ask.as_ref().and_then(|ask| {
            let snapshot = self.ask_card_snapshot(&ask.id, cx)?;
            let ix = self.conversation.read(cx).find_tool(&ask.id, cx)?;
            let item = self.conversation.read(cx).items().get(ix)?.clone();
            Some((item, snapshot))
        });

        if let Some(previous) = self.ask_snapshot_item.take()
            && next
                .as_ref()
                .is_none_or(|(current, _)| current != &previous)
            && previous.read(cx).ask_snapshot.is_some()
        {
            previous.update(cx, |item, cx| {
                item.ask_snapshot = None;
                cx.notify();
            });
        }

        if let Some((item, snapshot)) = next {
            if item.read(cx).ask_snapshot.as_ref() != Some(&snapshot) {
                item.update(cx, |item, cx| {
                    item.ask_snapshot = Some(snapshot);
                    cx.notify();
                });
            }
            self.ask_snapshot_item = Some(item);
        }
    }

    pub(super) fn pending_ask_has_selection(&self) -> bool {
        self.pending_ask
            .as_ref()
            .is_some_and(|ask| ask.selections.iter().flatten().any(|selected| *selected))
    }

    pub(super) fn composer_can_submit(&self, running: bool, cx: &App) -> bool {
        // T10c: the v1 `history_phase` loading gate retired with the fold
        // (the restore boundary is now the §D.1 snapshot; a pending-snapshot
        // gate lands with the §K.5 closeout).
        if running {
            return true;
        }
        let input_empty = self.input_state.read(cx).value().trim().is_empty();
        if self.pending_ask.is_some() {
            !input_empty || self.pending_ask_has_selection()
        } else {
            !input_empty || !self.pending_attachments.is_empty()
        }
    }

    pub(crate) fn toggle_ask_option(&mut self, qi: usize, oi: usize, cx: &mut Context<Self>) {
        if let Some(ask) = self.pending_ask.as_mut()
            && let Some(sel) = ask.selections.get_mut(qi)
        {
            let multi = ask
                .questions
                .get(qi)
                .map(|q| q.multi_select)
                .unwrap_or(false);
            let prev = sel.get(oi).copied().unwrap_or(false);
            if multi {
                if let Some(slot) = sel.get_mut(oi) {
                    *slot = !*slot;
                }
            } else {
                for s in sel.iter_mut() {
                    *s = false;
                }
                if let Some(slot) = sel.get_mut(oi) {
                    *slot = !prev;
                }
            }
        }
        cx.notify();
    }

    pub(crate) fn ask_prev(&mut self, cx: &mut Context<Self>) {
        if self.ask_step > 0 {
            self.ask_step -= 1;
            cx.notify();
        }
    }

    pub(crate) fn ask_next(&mut self, cx: &mut Context<Self>) {
        if let Some(ask) = self.pending_ask.as_ref()
            && self.ask_step < ask.questions.len() - 1
        {
            self.ask_step += 1;
            cx.notify();
        }
    }

    /// Submit the ask drawer: gather selected options plus an optional global
    /// supplemental note from the composer.
    pub(crate) fn resolve_ask_with_response(
        &mut self,
        response_override: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let ask = match self.pending_ask.take() {
            Some(a) => a,
            None => return,
        };
        let response_text = response_override.unwrap_or_default();
        let response = if response_text.trim().is_empty() {
            None
        } else {
            Some(response_text.trim().to_string())
        };
        let mut answers: Vec<(String, String)> = Vec::with_capacity(ask.questions.len());
        for (i, q) in ask.questions.iter().enumerate() {
            let sel = ask.selections.get(i).map(|s| s.as_slice()).unwrap_or(&[]);
            let selected: Vec<&str> = q
                .options
                .iter()
                .zip(sel.iter())
                .filter_map(|(o, &s)| s.then_some(o.label.as_str()))
                .collect();
            let answer = selected.join(", ");
            answers.push((q.question.clone(), answer));
        }
        let id = ask.id.clone();
        self.pending_ask = None;
        self.ask_step = 0;
        self.ask_transition_gen = self.ask_transition_gen.wrapping_add(1);
        if let Some(msg_id) = self
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.pending_auth.get(&id).cloned())
        {
            self.client.send_reply(
                msg_id,
                Ok(serde_json::json!({
                    "answers": answers,
                    "response": response,
                })),
            );
            return;
        }
        self.thread.with_mut(|thread| {
            thread.respond_authorization(
                &id,
                manox_agent::ToolAuthorizationResponse::AskUserQuestion { answers, response },
            );
        });
        cx.notify();
    }

    /// Wire api string → Tag variant + label for the pi model menu.
    pub(crate) fn pi_wire_tag_variant(api: &str) -> (TagVariant, &'static str) {
        match api {
            "anthropic" => (TagVariant::Color(ColorName::Blue), "Anthropic"),
            "openai_responses" => (TagVariant::Color(ColorName::Cyan), "Responses"),
            "openai_completions" => (TagVariant::Color(ColorName::Amber), "Completions"),
            _ => (TagVariant::Secondary, "N/A"),
        }
    }

    /// Wire api string → text color for the pi composer model label and the
    /// context rail's per-model usage rows. Blue/cyan/amber at 600 in light,
    /// 300 in dark, mirroring the VS Code webview's `--wire-*` tokens so both
    /// sides render identical hues.
    pub(crate) fn pi_wire_text_color(api: &str, theme: &Theme) -> gpui::Hsla {
        let step = if theme.is_dark() { 300 } else { 600 };
        match api {
            "anthropic" => ColorName::Blue.scale(step),
            "openai_responses" => ColorName::Cyan.scale(step),
            "openai_completions" => ColorName::Amber.scale(step),
            _ => theme.muted_foreground,
        }
    }

    /// The foreground session's model identity from the `model` projection:
    /// the canonical `{provider, modelId}` wire identity (L8) — never a full
    /// `Model` blob. Deserializing the projection into `Model` (an earlier
    /// iteration) always failed on the missing display fields, so the chip
    /// rendered "no model" no matter what the journal said.
    pub(crate) fn foreground_model_identity(&self, cx: &App) -> Option<(String, String)> {
        self.store.as_ref().and_then(|s| {
            s.read(cx).store.with(|st| {
                let v = st.model.clone()?;
                let provider = v.get("provider")?.as_str()?.to_string();
                let id = v.get("modelId")?.as_str()?.to_string();
                (!provider.is_empty() && !id.is_empty()).then_some((provider, id))
            })
        })
    }

    /// Exact registration match of a canonical model identity against the
    /// kernel provider registry, returning the kernel `Model`. U2 retired
    /// this from the DISPLAY surfaces (the chip and the menu resolve against
    /// the gateway's wire snapshot — [`Self::resolve_model_display`]); what
    /// remains is the `ExecuteFresh` facade seeding, which constructs a
    /// kernel `Thread` and needs a kernel `Model` (U6/attach surface). A
    /// stale id resolves to `None`, never a fuzzy look-alike.
    pub(crate) fn resolve_model_identity(
        provider: &str,
        id: &str,
    ) -> Option<manox_harness::types::Model> {
        Self::resolve_model_identity_in(&manox_agent::provider_glue::global(), provider, id)
    }

    /// The pure core of [`Self::resolve_model_identity`] against an explicit
    /// registry (tests construct one synchronously — the global builds on a
    /// background thread).
    pub(crate) fn resolve_model_identity_in(
        registry: &manox_harness::core::ProviderRegistry,
        provider: &str,
        id: &str,
    ) -> Option<manox_harness::types::Model> {
        registry
            .models()
            .into_iter()
            .find(|m| m.provider == provider && m.id == id)
    }

    /// Exact registration match of a canonical model identity against the
    /// gateway's model-registry snapshot (U2 display resolution): the chip
    /// and the picker menu read the wire `ModelInfo` the server projects, so
    /// a remote server's registry drives the desktop display. A stale id
    /// resolves to `None` and the caller renders the raw identity, never a
    /// fuzzy look-alike.
    pub(crate) fn resolve_model_display(
        &self,
        provider: &str,
        id: &str,
        cx: &App,
    ) -> Option<manox_protocol::ModelInfo> {
        Self::resolve_model_display_in(self.multiplexer.read(cx).models(), provider, id)
    }

    /// The pure core of [`Self::resolve_model_display`] against an explicit
    /// wire snapshot.
    pub(crate) fn resolve_model_display_in(
        models: &[manox_protocol::ModelInfo],
        provider: &str,
        id: &str,
    ) -> Option<manox_protocol::ModelInfo> {
        models
            .iter()
            .find(|m| m.provider == provider && m.id == id)
            .cloned()
    }

    /// The pi-harness model selector. Reads the gateway's model-registry
    /// snapshot (U2 — the server projects the pi registry the streaming side
    /// resolves against): closed, a ghost button showing
    /// `provider · model · effort` with the model name tinted by wire api;
    /// open, a PopupMenu of provider submenus.
    pub(super) fn render_model_selector_pi(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.model_open;
        let model_identity = self.foreground_model_identity(cx);
        let model = model_identity
            .as_ref()
            .and_then(|(provider, id)| self.resolve_model_display(provider, id, cx));
        let effort = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.reasoning_effort)
            .expect("foreground store present");

        let trigger = h_flex()
            .id("model-trigger")
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.accent.opacity(0.08)))
            .cursor_pointer()
            .children(if let Some(ref m) = model {
                let model_color = Self::pi_wire_text_color(&m.api, theme);
                let dot = || {
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("·")
                };
                vec![
                    gpui::div()
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(
                            m.provider_name
                                .clone()
                                .unwrap_or_else(|| m.provider.clone()),
                        )
                        .into_any_element(),
                    dot().into_any_element(),
                    gpui::div()
                        .text_xs()
                        .text_color(model_color)
                        .child(m.name.clone())
                        .into_any_element(),
                    dot().into_any_element(),
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(effort.wire_value().to_string())
                        .into_any_element(),
                ]
            } else if let Some((ref provider, ref id)) = model_identity {
                // The journal identity no longer resolves against the
                // gateway's model snapshot (provider/model deregistered) —
                // render it raw so the chip still tells the truth instead
                // of "no model".
                vec![
                    gpui::div()
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(provider.clone())
                        .into_any_element(),
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("·")
                        .into_any_element(),
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(id.clone())
                        .into_any_element(),
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("·")
                        .into_any_element(),
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(effort.wire_value().to_string())
                        .into_any_element(),
                ]
            } else {
                vec![
                    gpui::div()
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(i18n::t("workspace-no-model").to_string())
                        .into_any_element(),
                ]
            })
            .child(
                Icon::new(if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(theme.muted_foreground),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                if this.model_open {
                    this.model_open = false;
                    this.model_menu = None;
                    this.model_menu_sub = None;
                } else {
                    this.model_open = true;
                    let current_effort = this
                        .store
                        .as_ref()
                        .map(|s| s.read(cx).store.reasoning_effort)
                        .expect("foreground store present");
                    let workspace = cx.entity().downgrade();
                    // U2: the menu lists the gateway's model-registry
                    // snapshot (the server projects the same pi registry the
                    // streaming side resolves against). The open also
                    // re-pulls, so a settings-side provider reload (no
                    // server push yet — §D.5 cross-domain ask) converges by
                    // the next open at the latest.
                    this.multiplexer.update(cx, |m, _| m.fetch_models());
                    let models = this.multiplexer.read(cx).models().to_vec();
                    let menu = PopupMenu::build(window, cx, |menu, window, cx| {
                        Self::build_model_popup_menu_pi(
                            menu,
                            workspace,
                            models,
                            current_effort,
                            window,
                            cx,
                        )
                    });
                    let sub = cx.subscribe(
                        &menu,
                        |this: &mut Workspace,
                         _menu: Entity<PopupMenu>,
                         _: &DismissEvent,
                         cx: &mut Context<Workspace>| {
                            this.model_open = false;
                            this.model_menu = None;
                            this.model_menu_sub = None;
                            cx.notify();
                        },
                    );
                    this.model_menu = Some(menu);
                    this.model_menu_sub = Some(sub);
                }
                cx.notify();
            }));

        if !open {
            return trigger.into_any_element();
        }

        let menu = self
            .model_menu
            .clone()
            .expect("model_menu exists when open");

        gpui::div()
            .relative()
            .child(trigger)
            .child(
                deferred(
                    gpui::div()
                        .id("model-dropdown")
                        .absolute()
                        .bottom_full()
                        .right_0()
                        .occlude()
                        .child(menu),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    /// Model menu for the pi harness: grouped by provider display name;
    /// each row shows a wire-api Tag and selects through the gateway. U2:
    /// the rows are the server's wire `ModelInfo` snapshot (already deduped
    /// per registration at the source); a config model registered through
    /// several wire apis appears once per wire endpoint (registration names
    /// differ), so the responses and completions variants stay selectable
    /// alongside the anthropic one.
    pub(super) fn build_model_popup_menu_pi(
        menu: PopupMenu,
        workspace: WeakEntity<Workspace>,
        models: Vec<manox_protocol::ModelInfo>,
        current_effort: manox_agent::language_model::ReasoningEffort,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        // Group by DISPLAY name via lookup (not adjacency): the snapshot is
        // sorted by registration name, so same-display-name providers with
        // different registrations must still merge into one submenu.
        let mut providers: Vec<(String, Vec<manox_protocol::ModelInfo>)> = Vec::new();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for m in models {
            let prov = m
                .provider_name
                .clone()
                .unwrap_or_else(|| m.provider.clone());
            // Identity is the registration name (unique per wire endpoint),
            // so wire variants of one provider stay separate; only exact
            // duplicates collapse (the server already dedupes — this is the
            // defensive client-side parity).
            if !seen.insert((m.provider.clone(), m.id.clone())) {
                continue;
            }
            match providers.iter_mut().find(|(name, _)| *name == prov) {
                Some((_, models)) => models.push(m),
                None => providers.push((prov, vec![m])),
            }
        }
        let mut menu = menu;
        if providers.is_empty() {
            return menu.item(PopupMenuItem::Label("No models configured".into()));
        }
        for (prov_name, models) in providers {
            let ws = workspace.clone();
            menu = menu.submenu(prov_name, window, cx, move |submenu, _window, _cx| {
                let mut submenu = submenu;
                for m in &models {
                    let model = m.clone();
                    let model_name = model.name.clone();
                    let (variant, label) = Self::pi_wire_tag_variant(&model.api);
                    let ws = ws.clone();
                    submenu = submenu.item(
                        PopupMenuItem::element(move |_window, _cx| {
                            h_flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    Tag::new()
                                        .with_variant(variant)
                                        .outline()
                                        .small()
                                        .child(label),
                                )
                                .child(model_name.clone())
                        })
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            let model = model.clone();
                            let _ = ws.update(cx, |this, _cx| {
                                // L8 wire identity: the registration-qualified
                                // `{provider}/{model}` ref, so a pick pins the
                                // exact endpoint (wire variants of one model
                                // share the bare id).
                                let _ =
                                    this.send_note(|sid| manox_protocol::ClientNote::SetModel {
                                        session_id: sid.into(),
                                        id: format!("{}/{}", model.provider, model.id),
                                    });
                            });
                        }),
                    );
                }
                submenu
            });
        }
        // The reasoning-effort knob lives in the model dropdown, next to the
        // model switch it tunes. The current effort is checked; a click
        // applies to the next request (same mid-run semantics as a model
        // switch; the menu dismisses like a model row).
        let themed = cx.theme().clone();
        menu = menu.separator();
        menu = menu.label(i18n::t("workspace-reasoning-effort"));
        for effort in manox_agent::language_model::ReasoningEffort::ALL {
            let ws = workspace.clone();
            let themed = themed.clone();
            let label = match effort {
                manox_agent::language_model::ReasoningEffort::High => {
                    i18n::t("workspace-reasoning-high")
                }
                manox_agent::language_model::ReasoningEffort::Max => {
                    i18n::t("workspace-reasoning-max")
                }
            };
            let selected = effort == current_effort;
            menu = menu.item(
                PopupMenuItem::element(move |_window, _cx| {
                    h_flex()
                        .items_center()
                        .gap_1()
                        .child(
                            gpui::div()
                                .text_sm()
                                .text_color(themed.foreground)
                                .child(label.clone()),
                        )
                        .when(selected, |el| {
                            el.child(Icon::new(IconName::Check).small().text_color(themed.accent))
                        })
                })
                .on_click(move |_, _, cx: &mut gpui::App| {
                    let _ = ws.update(cx, |this, _cx| {
                        let effort_str = match effort {
                            manox_agent::language_model::ReasoningEffort::High => "high",
                            manox_agent::language_model::ReasoningEffort::Max => "max",
                        };
                        let _ =
                            this.send_note(|sid| manox_protocol::ClientNote::SetReasoningEffort {
                                session_id: sid.into(),
                                effort: effort_str.into(),
                            });
                    });
                }),
            );
        }
        menu
    }

    /// Open the goal status popover (from the bare `/goal` command).
    pub fn open_goal_popover(&mut self, cx: &mut Context<Self>) {
        self.goal_popover_open = true;
        cx.notify();
    }

    /// Prefill the composer with the durable objective so `/goal edit` is an
    /// explicit, inspectable update rather than an ephemeral popover field.
    pub fn begin_goal_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(objective) = self
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.goal.clone())
            .and_then(|v| serde_json::from_value::<manox_agent::goal::ThreadGoal>(v).ok())
            .map(|goal| goal.objective.clone())
        else {
            self.goal_popover_open = true;
            cx.notify();
            return;
        };
        self.input_state.update(cx, |state, cx| {
            state.set_value(format!("/goal edit {objective}"), window, cx);
        });
        cx.notify();
    }

    pub fn begin_goal_budget_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.goal.clone())
            .and_then(|v| serde_json::from_value::<manox_agent::goal::ThreadGoal>(v).ok())
            .and_then(|goal| goal.token_budget)
            .map(|budget| budget.to_string())
            .unwrap_or_else(|| "none".into());
        self.input_state.update(cx, |state, cx| {
            state.set_value(format!("/goal budget {value}"), window, cx);
        });
        cx.notify();
    }

    pub fn begin_goal_rounds_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.goal.clone())
            .and_then(|v| serde_json::from_value::<manox_agent::goal::ThreadGoal>(v).ok())
            .and_then(|goal| goal.max_rounds)
            .map(|max| max.to_string())
            .unwrap_or_else(|| "none".into());
        self.input_state.update(cx, |state, cx| {
            state.set_value(format!("/goal rounds {value}"), window, cx);
        });
        cx.notify();
    }

    /// Selecting Replace only prepares an explicit confirmation command; the
    /// persisted CAS replacement happens when the user submits it.
    pub fn begin_goal_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.begin_goal_replace_with_objective("", window, cx);
    }

    pub fn begin_goal_replace_with_objective(
        &mut self,
        objective: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input_state.update(cx, |state, cx| {
            state.set_value(format!("/goal replace {objective}"), window, cx);
        });
        cx.notify();
    }

    pub fn begin_goal_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input_state.update(cx, |state, cx| {
            state.set_value("/goal ".to_string(), window, cx);
        });
        cx.notify();
    }

    pub(super) fn render_goal_chip(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let g = self
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.goal.clone())
            .and_then(|v| serde_json::from_value::<manox_agent::goal::ThreadGoal>(v).ok())?;
        let accent = theme.accent;
        let muted = theme.muted_foreground;
        let fg = theme.foreground;
        let status_key = match g.status {
            manox_agent::goal::GoalStatus::Active => "goal-status-active",
            manox_agent::goal::GoalStatus::Paused => "goal-status-paused",
            manox_agent::goal::GoalStatus::Blocked => "goal-status-blocked",
            manox_agent::goal::GoalStatus::BudgetLimited => "goal-status-budget-limited",
            manox_agent::goal::GoalStatus::Complete => "goal-status-complete",
        };
        let elapsed = format_elapsed(std::time::Duration::from_secs(
            self.thread
                .read(|t| t.goal_elapsed_seconds())
                .unwrap_or_default(),
        ));
        let label: SharedString = format!("◎ {} · {}", i18n::t(status_key), elapsed).into();
        let open = self.goal_popover_open;

        let trigger = h_flex()
            .id("goal-chip")
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .bg(theme.secondary)
            .border_1()
            .border_color(accent)
            .cursor_pointer()
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.accent_foreground)
                    .child(label),
            )
            .child(
                Icon::new(if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(muted),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.goal_popover_open = !this.goal_popover_open;
                cx.notify();
            }));

        if !open {
            return Some(trigger.into_any_element());
        }

        let objective = g.objective.clone();
        let status = i18n::t(status_key);
        let reason = g
            .blocked_reason
            .as_ref()
            .map(|reason| reason.message.clone())
            .unwrap_or_else(|| "—".into());
        let tokens = g.tokens_used.to_string();
        let budget = g
            .token_budget
            .map(|value| value.to_string())
            .unwrap_or_else(|| "∞".into());
        let remaining = g
            .remaining_tokens()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "∞".into());
        let rounds = format!(
            "{}/{}",
            g.rounds_started,
            g.max_rounds
                .map(|max| max.to_string())
                .unwrap_or_else(|| "∞".into())
        );
        let goal_status = g.status;
        let objective_label = i18n::t("goal-popover-objective");
        let status_label = i18n::t("goal-popover-status");
        let elapsed_label = i18n::t("goal-popover-elapsed");
        let reason_label = i18n::t("goal-popover-reason");
        let tokens_label = i18n::t("goal-popover-tokens");
        let budget_label = i18n::t("goal-popover-budget");
        let remaining_label = i18n::t("goal-popover-remaining");
        let rounds_label = i18n::t("goal-popover-rounds");
        let clear_label = i18n::t("goal-popover-clear");
        let pause_label = i18n::t("goal-popover-pause");
        let resume_label = i18n::t("goal-popover-resume");
        let edit_label = i18n::t("goal-popover-edit");
        let edit_budget_label = i18n::t("goal-popover-edit-budget");
        let edit_rounds_label = i18n::t("goal-popover-edit-rounds");
        let replace_label = i18n::t("goal-popover-replace");
        let new_label = i18n::t("goal-popover-new");
        let title_label = i18n::t("goal-popover-title");
        let popover = v_flex()
            .w_full()
            .gap_1()
            .p_3()
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.accent_foreground)
                    .child(format!("◎ {title_label}")),
            )
            .child(goal_popover_row(&objective_label, &objective, fg, muted))
            .child(goal_popover_row(&status_label, &status, fg, muted))
            .child(goal_popover_row(&elapsed_label, &elapsed, fg, muted))
            .child(goal_popover_row(&reason_label, &reason, fg, muted))
            .child(goal_popover_row(&tokens_label, &tokens, fg, muted))
            .child(goal_popover_row(&budget_label, &budget, fg, muted))
            .child(goal_popover_row(&remaining_label, &remaining, fg, muted))
            .child(goal_popover_row(&rounds_label, &rounds, fg, muted))
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .when(
                        goal_status == manox_agent::goal::GoalStatus::Active,
                        |row| {
                            row.child(
                                Button::new("goal-pause")
                                    .small()
                                    .label(pause_label)
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, _cx| {
                                        let _ = this.send_note(|sid| {
                                            manox_protocol::ClientNote::Goal {
                                                session_id: sid.into(),
                                                action: "pause".into(),
                                                objective: None,
                                                budget: None,
                                                max_rounds: None,
                                            }
                                        });
                                    })),
                            )
                        },
                    )
                    .when(
                        matches!(
                            goal_status,
                            manox_agent::goal::GoalStatus::Paused
                                | manox_agent::goal::GoalStatus::Blocked
                        ),
                        |row| {
                            row.child(
                                Button::new("goal-resume")
                                    .small()
                                    .label(resume_label)
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, _cx| {
                                        let _ = this.send_note(|sid| {
                                            manox_protocol::ClientNote::Goal {
                                                session_id: sid.into(),
                                                action: "resume".into(),
                                                objective: None,
                                                budget: None,
                                                max_rounds: None,
                                            }
                                        });
                                    })),
                            )
                        },
                    )
                    .when(
                        matches!(
                            goal_status,
                            manox_agent::goal::GoalStatus::Active
                                | manox_agent::goal::GoalStatus::Paused
                                | manox_agent::goal::GoalStatus::Blocked
                        ),
                        |row| {
                            row.child(Button::new("goal-edit").small().label(edit_label).on_click(
                                cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.goal_popover_open = false;
                                    this.begin_goal_edit(window, cx);
                                }),
                            ))
                        },
                    )
                    .when(
                        matches!(
                            goal_status,
                            manox_agent::goal::GoalStatus::Active
                                | manox_agent::goal::GoalStatus::Paused
                                | manox_agent::goal::GoalStatus::Blocked
                        ),
                        |row| {
                            row.child(
                                Button::new("goal-edit-rounds")
                                    .small()
                                    .label(edit_rounds_label)
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.goal_popover_open = false;
                                            this.begin_goal_rounds_edit(window, cx);
                                        },
                                    )),
                            )
                        },
                    )
                    .when(
                        goal_status == manox_agent::goal::GoalStatus::BudgetLimited,
                        |row| {
                            row.child(
                                Button::new("goal-edit-budget")
                                    .small()
                                    .label(edit_budget_label)
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.goal_popover_open = false;
                                            this.begin_goal_budget_edit(window, cx);
                                        },
                                    )),
                            )
                        },
                    )
                    .when(
                        matches!(
                            goal_status,
                            manox_agent::goal::GoalStatus::Paused
                                | manox_agent::goal::GoalStatus::Blocked
                                | manox_agent::goal::GoalStatus::BudgetLimited
                        ),
                        |row| {
                            row.child(
                                Button::new("goal-replace")
                                    .small()
                                    .label(replace_label)
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.goal_popover_open = false;
                                            this.begin_goal_replace(window, cx);
                                        },
                                    )),
                            )
                        },
                    )
                    .when(
                        goal_status == manox_agent::goal::GoalStatus::Complete,
                        |row| {
                            row.child(Button::new("goal-new").small().label(new_label).on_click(
                                cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.goal_popover_open = false;
                                    this.begin_goal_new(window, cx);
                                }),
                            ))
                        },
                    )
                    .child(
                        Button::new("goal-clear")
                            .small()
                            .label(clear_label)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                let _ = this.send_note(|sid| manox_protocol::ClientNote::Goal {
                                    session_id: sid.into(),
                                    action: "clear".into(),
                                    objective: None,
                                    budget: None,
                                    max_rounds: None,
                                });
                                this.goal_popover_open = false;
                                cx.notify();
                            })),
                    ),
            );

        Some(
            gpui::div()
                .relative()
                .child(trigger)
                .child(
                    deferred(
                        gpui::div()
                            .id("goal-dropdown")
                            .absolute()
                            .bottom_full()
                            .left_0()
                            .occlude()
                            .w(gpui::px(360.))
                            .popover_style(cx)
                            .child(popover)
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.goal_popover_open = false;
                                cx.notify();
                            })),
                    )
                    .with_priority(1),
                )
                .into_any_element(),
        )
    }
}
