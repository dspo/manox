//! The composer + overlay render helpers (U9b cluster 8): the composer
//! element (and its queued-follow-ups / plan-chip / access-placeholder /
//! plus-menu / send-button / completion-overlay / attachments /
//! project-chip faces), the browser-suite toggles, the file picker, the
//! blank-project family, the pending-auth overlay, and the project
//! registration leg. Split from `workspace.rs` — `super` is the
//! workspace module; the bare-private methods lift `pub(super)` for the
//! parent's render face and the `tests` child.

use super::*;

impl Workspace {
    /// Rendered bare — no card border, fill, or rounding — so it shares the
    /// page background with the message list and reads as the same layer.
    /// The `Input` has no appearance of its own; the only visual separator
    /// from the messages above is the hairline injected by the footer caller.
    pub(super) fn render_composer(
        &mut self,
        running: bool,
        window: &mut Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Flip the composer placeholder only on mode transitions, so render
        // doesn't churn the InputState every frame.
        let followup_mode = running
            && self.pending_plan_review.is_none()
            && self.pending_ask.is_none()
            && self.pending_auth.is_none();
        let placeholder_mode = if self.pending_ask.is_some() {
            ComposerPlaceholderMode::Ask
        } else if followup_mode {
            ComposerPlaceholderMode::FollowUp
        } else {
            ComposerPlaceholderMode::Normal
        };
        if placeholder_mode != self.composer_placeholder_mode {
            self.composer_placeholder_mode = placeholder_mode;
            let key = match placeholder_mode {
                ComposerPlaceholderMode::Normal => "workspace-input-placeholder",
                ComposerPlaceholderMode::FollowUp => "composer-placeholder-followup",
                ComposerPlaceholderMode::Ask => "workspace-ask-supplement-placeholder",
            };
            self.input_state.update(cx, |state, cx| {
                state.set_placeholder(i18n::t(key), window, cx);
            });
        }
        let queue = self.render_queued_follow_ups(theme, cx);
        let plus = self.render_plus_button(cx);
        let project_chip = self.render_project_chip_pi(theme, cx);
        let goal_chip = self.render_goal_chip(theme, cx);
        let plan_chip = self.render_plan_chip(theme, cx);
        let access = self.render_access_placeholder(theme, cx);
        let model = self.render_model_selector_pi(theme, cx);
        let send = self.render_send_button(
            running
                && self.pending_plan_review.is_none()
                && self.pending_ask.is_none()
                && self.pending_auth.is_none(),
            cx,
        );
        // The completion popover overlays the composer; anchoring it on the
        // composer's own v_flex keeps it glued to the input bar in both hero
        // and footer, with a single mount point and ElementId.
        let completion_overlay = self.render_completion_overlay(cx);

        v_flex()
            .w_full()
            .gap_2()
            .relative()
            .children(queue)
            .children(completion_overlay)
            // Own paste at the capture phase so a clipboard image becomes a
            // pending attachment instead of letting `InputState::paste` insert
            // the image's alt-text. `stop_propagation` keeps the inner input's
            // text-paste handler from also running; text is inserted via the
            // public `replace` so the completion popover re-sync still fires.
            .capture_action(cx.listener(|this, _: &Paste, window, cx| {
                cx.stop_propagation();
                let Some(clipboard) = cx.read_from_clipboard() else {
                    return;
                };
                let entries = clipboard.entries();
                let has_image = entries
                    .iter()
                    .any(|e| matches!(e, gpui::ClipboardEntry::Image(_)));
                if has_image {
                    for entry in entries {
                        if let gpui::ClipboardEntry::Image(image) = entry {
                            this.handle_pasted_image(image.clone(), cx);
                        }
                    }
                    cx.notify();
                } else {
                    let text = clipboard.text().unwrap_or_default();
                    if !text.is_empty() {
                        this.input_state
                            .update(cx, |state, cx| state.replace(text, window, cx));
                        this.sync_completion(window, cx);
                    }
                }
            }))
            .when(self.pending_ask.is_some(), |this| {
                this.child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(i18n::t("workspace-ask-supplement-label")),
                )
            })
            .child(
                // Composer input is message content in the mono family (Lilex)
                // at Light weight — the message-list body typeface. The `Input`
                // component forces `text_sm()` internally (its default
                // `Size::Medium` maps through `input_text_size`), so the host
                // pins the body size back with an instance-level
                // `.text_size(MESSAGE_BODY_SIZE)` (13px, one step below chrome
                // `text_base`); family + weight are applied from the wrapper
                // context.
                {
                    let wrap = gpui::div()
                        .font_family(theme.mono_font_family.clone())
                        .font_weight(gpui::FontWeight::LIGHT)
                        // The wrapper carries exactly one key context: the open
                        // completion popover takes `completion = open` (its
                        // `completion == open > Input` bindings shadow the
                        // Input's own up/down/enter/tab/escape), otherwise plain
                        // `composer` — which is what the recall bindings
                        // (`composer > Input` on alt-up / alt-down) hang off.
                        // The bare arrows stay with the Input's native
                        // MoveUp/MoveDown in every composer state.
                        .key_context(composer_key_context(self.completion.is_some()));
                    wrap.child(
                        Input::new(&self.input_state)
                            .appearance(false)
                            .text_size(crate::views::message::MESSAGE_BODY_SIZE),
                    )
                },
            )
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(
                        // `min_w_0` lets this group flex-shrink when the row is
                        // narrow; `overflow_hidden` is deliberately NOT set so
                        // the chips' popovers (project picker, permission menu, `+`
                        // menu) can overflow upward. `MIN_WINDOW_W` keeps the row
                        // wide enough that the chips themselves never overflow.
                        h_flex()
                            .items_center()
                            .gap_1()
                            .min_w_0()
                            .child(plus)
                            .child(project_chip)
                            .when_some(goal_chip, |el, chip| el.child(chip))
                            .when_some(plan_chip, |el, chip| el.child(chip))
                            .child(access),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .flex_shrink_0()
                            .child(model)
                            .child(send),
                    ),
            )
            .into_any_element()
    }

    /// Render the compact queue above the composer. Pending steers live in the
    /// message list, so this area contains only ordinary queued rows and failed
    /// steers that need an explicit retry or deletion.
    pub(super) fn render_queued_follow_ups(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::with_capacity(self.queued_follow_ups.len());
        for (idx, item) in self.queued_follow_ups.iter().enumerate() {
            if matches!(item.state, FollowUpState::SteerPending { .. }) {
                continue;
            }
            let summary = truncate_follow_up(&item.turn.text);
            let delete_btn = Button::new(format!("queue-delete-{idx}"))
                .ghost()
                .xsmall()
                .icon(Icon::default().path("icons/trash-2.svg"))
                .tooltip(i18n::t("queued-delete-action"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.delete_follow_up(idx, cx);
                }));
            let more_btn = Button::new(format!("queue-more-{idx}"))
                .ghost()
                .xsmall()
                .icon(IconName::Ellipsis)
                .tooltip(i18n::t("queued-more-action"));

            let (action_btn, danger): (AnyElement, bool) = match &item.state {
                FollowUpState::Queued => {
                    let steer_btn = Button::new(format!("queue-steer-{idx}"))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Redo2)
                        .label(i18n::t("queued-steer-action"))
                        .tooltip(i18n::t("queued-steer-action"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.steer_follow_up(idx, cx);
                        }));
                    (steer_btn.into_any_element(), false)
                }
                FollowUpState::Failed { .. } => {
                    let retry_btn = Button::new(format!("queue-steer-{idx}"))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Redo2)
                        .label(i18n::t("queued-steer-retry-action"))
                        .tooltip(i18n::t("queued-steer-retry-action"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.steer_follow_up(idx, cx);
                        }));
                    (retry_btn.into_any_element(), true)
                }
                FollowUpState::SteerPending { .. } => unreachable!(),
            };

            let summary_color = if danger {
                theme.danger
            } else {
                theme.foreground
            };

            let left = h_flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .flex_1()
                .child(
                    Icon::default()
                        .path("icons/corner-right-up.svg")
                        .xsmall()
                        .text_color(if danger {
                            theme.danger
                        } else {
                            theme.muted_foreground
                        }),
                )
                .child(
                    gpui::div()
                        .flex_1()
                        .min_w_0()
                        .overflow_x_hidden()
                        .text_xs()
                        .text_color(summary_color)
                        .child(summary),
                );

            let right = h_flex()
                .items_center()
                .gap_0p5()
                .flex_shrink_0()
                .child(action_btn)
                .child(delete_btn)
                .child(more_btn);

            rows.push(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border.opacity(0.6))
                    .when(danger, |row| row.bg(theme.danger.opacity(0.08)))
                    .child(left)
                    .child(right)
                    .into_any_element(),
            );
        }
        rows
    }

    /// Plan-mode indicator chip: visible while the session plans (read-only
    /// research + plan-file writes), so the state is never silent. Clicking
    /// it leaves plan mode — the escape hatch when a review card is missed
    /// or the model stalls in research.
    pub(super) fn render_plan_chip(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.plan_mode)
            .expect("foreground store present")
        {
            return None;
        }
        Some(
            h_flex()
                .id("plan-mode-chip")
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .bg(theme.warning.opacity(0.12))
                .hover(|s| s.bg(theme.warning.opacity(0.22)))
                .cursor_pointer()
                .tooltip(move |window, cx| {
                    Tooltip::new(i18n::t("plan-chip-exit-tooltip")).build(window, cx)
                })
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.set_thread_plan_mode(false, cx);
                    this.add_info_message(
                        i18n::t("plan-mode-off-notice").to_string(),
                        NoticeAnchor::TurnEnd,
                        None,
                        cx,
                    );
                }))
                .child(
                    Icon::new(IconName::LayoutDashboard)
                        .xsmall()
                        .text_color(theme.warning),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(i18n::t("plan-chip-label")),
                )
                .into_any_element(),
        )
    }

    /// Access chip + permission-mode popover.
    ///
    /// The chip is a mode-aware pill rendered next to the composer send button.
    /// Each `PermissionMode` gets its own icon + accent color (amber eye for
    /// Read Only, green folder for Workspace Write, red triangle for Full
    /// Access) so the current permission posture is legible at a glance — a
    /// 1-line summary of what the model is allowed to do.
    ///
    /// Clicking the chip opens the popover: a question row with a "Learn
    /// more" link and three selectable rows (icon + title + subtitle, check
    /// on the right). The popover is `w(360)` to fit the longest bilingual
    /// subtitle without wrapping.
    pub(super) fn render_access_placeholder(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Selector read face (§J11): the composer's access chip derives its
        // permission mode through `ClientStore::with` instead of reaching the
        // field directly.
        let mode = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.with(|st| st.permission_mode))
            .expect("foreground store present");
        let open = self.access_open;
        // Pre-extract chip visuals so the click handler closure doesn't
        // capture `theme` (which only lives for the method body) — closures
        // passed to `cx.listener` must be `'static`.
        let (chip_label, chip_color, chip_icon) = mode_chip_visual(mode, theme);
        let workspace = cx.entity().downgrade();

        let trigger = h_flex()
            .id("access-chip")
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .min_w(px(96.))
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.accent.opacity(0.08)))
            .cursor_pointer()
            .child(Icon::new(chip_icon).xsmall().text_color(chip_color))
            .child(
                gpui::div()
                    .flex_1()
                    .text_xs()
                    .text_color(chip_color)
                    .child(chip_label),
            )
            .child(
                Icon::new(if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(theme.muted_foreground),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                if this.access_open {
                    this.close_access_menu();
                } else {
                    this.access_open = true;
                }
                cx.notify();
            }));

        if !open {
            return trigger.into_any_element();
        }

        // The popover is a plain `div` with `popover_style` (opaque card
        // chrome: bg + border + shadow + rounded). We don't route it through
        // `PopupMenu` because `PopupMenuItem::element` wraps every row in
        // `h_flex().flex_1().min_h(26)`, which both leaked vertical space
        // and — in the single-item case — clipped the v_flex content to
        // 26px. Doing it ourselves gives a content-sized, opaque popover.
        //
        // `w(360)` (not `max_w`) — with `min_w_0` on every text div, the
        // v_flex's intrinsic min-content is tiny (just icon widths + padding),
        // so `max_w` alone leaves the popover at ~140px and the subtitles
        // wrap into single-word lines. A fixed 360px width gives the
        // subtitles room to wrap at word boundaries.
        let content = build_permission_content(workspace.clone(), mode, cx);
        gpui::div()
            .relative()
            .child(trigger)
            .child(
                deferred(
                    gpui::div()
                        .id("access-dropdown")
                        .absolute()
                        .bottom_full()
                        .left_0()
                        .occlude()
                        .w(gpui::px(360.))
                        .popover_style(cx)
                        .child(content)
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.close_access_menu();
                            cx.notify();
                        })),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    /// Composer `+` button: attachment entry points (files / goal). Closed,
    /// the bare trigger; open, a PopupMenu anchored above it.
    pub(super) fn render_plus_button(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let trigger = Button::new("composer-plus")
            .ghost()
            .xsmall()
            .icon(IconName::Plus)
            .tooltip(i18n::t("composer-add-label"))
            .on_click(cx.listener(|this, _, window, cx| {
                if this.plus_open {
                    this.close_plus_menu();
                } else {
                    this.open_plus_menu(window, cx);
                }
                cx.notify();
            }));

        if !self.plus_open {
            return trigger.into_any_element();
        }
        let Some(menu) = self.plus_menu.clone() else {
            return trigger.into_any_element();
        };
        gpui::div()
            .relative()
            .child(trigger)
            .child(
                deferred(
                    gpui::div()
                        .id("plus-dropdown")
                        .absolute()
                        .bottom_full()
                        .left_0()
                        .occlude()
                        .child(menu),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    /// Build the `+` menu: "Files and folders" opens the native picker into
    /// pending attachments; "Goal" seeds the `/goal` slash command into the
    /// composer.
    pub(super) fn open_plus_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let theme = cx.theme().clone();
        let ws = cx.entity().downgrade();
        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            let ws_files = ws.clone();
            let ws_goal = ws.clone();
            let ws_chrome = ws.clone();
            let ws_internal = ws.clone();
            build_plus_menu(
                menu,
                &theme,
                move |window, cx| {
                    let _ = ws_files.update(cx, |this, cx| {
                        this.close_plus_menu();
                        this.pick_files(window, cx);
                        cx.notify();
                    });
                },
                move |window, cx| {
                    let _ = ws_goal.update(cx, |this, cx| {
                        this.close_plus_menu();
                        this.input_state.update(cx, |state, cx| {
                            state.set_value("/goal ".to_string(), window, cx);
                        });
                        cx.notify();
                    });
                },
                move |_window, cx| {
                    let _ = ws_chrome.update(cx, |this, cx| {
                        this.close_plus_menu();
                        this.activate_browser_tool_suite(
                            manox_agent::engine::BrowserSuite::ChromeUse,
                            cx,
                        );
                    });
                },
                move |_window, cx| {
                    let _ = ws_internal.update(cx, |this, cx| {
                        this.close_plus_menu();
                        this.activate_browser_tool_suite(
                            manox_agent::engine::BrowserSuite::WebExplore,
                            cx,
                        );
                    });
                },
            )
        });
        let sub = cx.subscribe(&menu, |this, _menu, _: &DismissEvent, cx| {
            this.close_plus_menu();
            cx.notify();
        });
        self.plus_open = true;
        self.plus_menu = Some(menu);
        self.plus_menu_sub = Some(sub);
    }

    /// Activate a browser tool suite on the bound thread. The chip is
    /// derived state: it rides the `BrowserSuitesChanged` echo of the
    /// facade mirror, so a landing thread's pre-engine toggle survives until
    /// the engine materializes. The engine merges the suite names atomically
    /// against the session's authoritative active-tool set.
    pub(super) fn activate_browser_tool_suite(
        &mut self,
        suite: manox_agent::engine::BrowserSuite,
        _cx: &mut Context<Self>,
    ) {
        // U6b①: the toggle rides the gateway (the setter-note family, like
        // SetPlanMode/SetModel) — the server arm lands it on the session's
        // facade, whose BrowserSuitesChanged echo drives the chip exactly
        // as the retired direct facade write did.
        if !self.send_note(|sid| manox_protocol::ClientNote::SetBrowserSuite {
            session_id: sid.to_string(),
            suite: suite.wire().to_string(),
            enable: true,
        }) {
            // Landing thread (no session yet): park the toggle in the
            // facade mirror — `ensure_engine` replays it on materialization
            // (the designed landing-park path, not a dual-source write).
            self.thread.with_mut(|t| t.set_browser_suite(suite, true));
        }
    }

    /// Deactivate a browser tool suite on the bound thread; the chip follows
    /// the mirror echo (see `activate_browser_tool_suite`).
    pub(super) fn deactivate_browser_tool_suite(
        &mut self,
        suite: manox_agent::engine::BrowserSuite,
        _cx: &mut Context<Self>,
    ) {
        // U6b①: the gateway leg (see `activate_browser_tool_suite`); the
        // landing fallback parks in the facade mirror.
        if !self.send_note(|sid| manox_protocol::ClientNote::SetBrowserSuite {
            session_id: sid.to_string(),
            suite: suite.wire().to_string(),
            enable: false,
        }) {
            self.thread.with_mut(|t| t.set_browser_suite(suite, false));
        }
    }

    /// Open the native file picker and add chosen paths as pending
    /// attachments (images render as chips + inline blocks, other files ride
    /// the attachment chip row).
    pub(super) fn pick_files(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let result = paths.await;
            this.update(cx, |this, cx| {
                if let Ok(Ok(Some(paths))) = result {
                    for path in paths {
                        this.pending_attachments.push(PendingAttachment::new(path));
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn close_plus_menu(&mut self) {
        self.plus_open = false;
        self.plus_menu = None;
        self.plus_menu_sub = None;
    }

    /// Circular icon-only send/stop button.
    ///
    /// The composer's primary action control, reused across the hero and footer
    /// layouts. The box is pinned to `SEND_BTN_SIZE` so the icon, spinner, hover
    /// border, and disabled tint never perturb the composer row's geometry.
    ///
    /// States are kept visually disjoint: while a turn is running (and no
    /// plan/ask awaits input) the button is a stop control — Pause glyph, danger
    /// tint, always enabled so cancel stays reachable. When idle it is a send
    /// control — ArrowUp glyph, accent tint — and goes inert (`disabled`) the
    /// moment the composer has no text and no pending attachments, so an empty
    /// input never reads as a ready-to-fire primary. The follow-up queue is
    /// driven by Enter, not by this button, so running never disables stop.
    pub(super) fn render_send_button(&self, running: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let disabled = !self.composer_can_submit(running, cx);

        // Matches the composer chip row height (px_2/py_1 + text_xs ≈ 20px),
        // so the send control shares the effort/model chips' rhythm instead of
        // towering over them. The disc corner radius is half the box => circle.
        const SEND_BTN_SIZE: Pixels = px(24.);
        const SEND_BTN_RADIUS: Pixels = px(12.);

        // Accent/danger-tinted transparent fills that strengthen on hover/active,
        // mirroring the chip family's accent.opacity(0.08) hover rather than a
        // heavy solid disc. Custom variant computes bg as color@~0.2, hover
        // color@~0.3, active color@~0.4; disabled falls back to color@0.15 +
        // muted_foreground@0.5 automatically.
        let variant = if running {
            ButtonCustomVariant::new(cx)
                .color(theme.danger)
                .foreground(theme.danger)
                .hover(theme.danger.opacity(0.18))
                .active(theme.danger.opacity(0.28))
        } else {
            ButtonCustomVariant::new(cx)
                .color(theme.accent)
                .foreground(theme.accent_foreground)
                .hover(theme.accent.opacity(0.18))
                .active(theme.accent.opacity(0.28))
        };

        Button::new("send-btn")
            .custom(variant)
            .with_size(Size::Size(SEND_BTN_SIZE))
            .rounded(SEND_BTN_RADIUS)
            .icon(if running {
                IconName::Pause
            } else {
                IconName::ArrowUp
            })
            .disabled(disabled)
            .on_click(cx.listener(|this, _, window, cx| {
                if this
                    .store
                    .as_ref()
                    .map(|s| s.read(cx).store.running)
                    .expect("foreground store present")
                    && this.pending_plan_review.is_none()
                    && this.pending_ask.is_none()
                {
                    this.cancel_turn(cx);
                } else {
                    this.submit_input(window, cx);
                }
            }))
            .into_any_element()
    }

    /// The completion popover overlaid above the composer while a trigger token
    /// (`/` or `@`) is active at the caret. Uses [`gpui::anchored`] (the same
    /// mechanism gpui-component's `Popover` and zed's completion menu use) so the
    /// popover escapes ancestor `overflow_hidden` clipping and avoids window-edge
    /// overflow — `div().absolute().bottom_full()` inside `deferred` does not
    /// position correctly and gets clipped by the body wrapper's `overflow_hidden`.
    /// A click on a row confirms it.
    pub(super) fn render_completion_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.completion.as_ref()?;
        let theme = cx.theme().clone();
        let on_select = cx.listener(|this, ix: &usize, window, cx| {
            this.completion_confirm(*ix, window, cx);
        });
        let on_select: SelectHandler =
            std::rc::Rc::new(move |ix, window, cx| on_select(&ix, window, cx));
        Some(
            deferred(
                anchored()
                    .anchor(Anchor::BottomLeft)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        gpui::div()
                            .id("completion-dropdown")
                            .occlude()
                            .child(render_completion(state, &theme, on_select)),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    /// Attachment + browser-suite chips shown above the composer, each
    /// removable. File attachments clear on submit; browser suites persist.
    pub(super) fn render_attachments(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.pending_attachments.is_empty() && self.active_browser_suites.is_empty() {
            return None;
        }
        let mut col = v_flex().w_full().gap_1();
        if !self.pending_attachments.is_empty() {
            let on_remove = cx.listener(|this, ix: &usize, _window, cx| {
                if *ix < this.pending_attachments.len() {
                    this.pending_attachments.remove(*ix);
                    cx.notify();
                }
            });
            col = col.child(centered(render_attachment_chips(
                &self.pending_attachments,
                theme,
                move |ix, window, cx| on_remove(&ix, window, cx),
            )));
        }
        if !self.active_browser_suites.is_empty() {
            let on_remove_suite = cx.listener(|this, ix: &usize, _window, cx| {
                if let Some(suite) = this.active_browser_suites.get(*ix).copied() {
                    this.deactivate_browser_tool_suite(suite, cx);
                }
            });
            col = col.child(centered(render_browser_chips(
                &self.active_browser_suites,
                theme,
                move |ix, window, cx| on_remove_suite(&ix, window, cx),
            )));
        }
        Some(col.into_any_element())
    }

    /// The pi-harness project chip: bound-project indicator + dropdown with
    /// recent projects (store `known_projects` plus session-cwd backfill),
    /// blank-project creation and folder selection. Selection is only
    /// allowed on empty threads (same guard as the manox chip). Data source
    /// is the pi thread store only.
    pub(super) fn render_project_chip_pi(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let project = self.store.as_ref().and_then(|s| {
            s.read(cx)
                .store
                .project
                .clone()
                .map(std::path::PathBuf::from)
        });
        let open = self.project_chip_open;
        let workspace = cx.entity().downgrade();

        let (icon, label): (Option<IconName>, SharedString) = match &project {
            Some(dir) => {
                let name = dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("project")
                    .to_string();
                (Some(IconName::FolderOpen), name.into())
            }
            None => (
                Some(IconName::FolderOpen),
                i18n::t("workspace-project-choose"),
            ),
        };

        let trigger = h_flex()
            .id("project-chip")
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.accent.opacity(0.08)))
            .cursor_pointer()
            .when_some(icon.clone(), |el, ic| {
                el.child(Icon::new(ic).xsmall().text_color(theme.muted_foreground))
            })
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.foreground)
                    .child(label),
            )
            .child(
                Icon::new(if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(theme.muted_foreground),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                if this.project_chip_open {
                    this.close_project_chip_menu();
                    cx.notify();
                    return;
                }
                // Only allow project selection on empty threads.
                let can_set = this
                    .store
                    .as_ref()
                    .map(|s| s.read(cx).store.derived_messages().is_empty())
                    .expect("foreground store present");
                if !can_set {
                    return;
                }
                this.project_chip_open = true;

                let ws = workspace.clone();
                let theme = cx.theme().clone();
                let ws_blank = ws.clone();
                let ws_folder = ws.clone();
                // U2: the chip's recency source is the pushed decoration
                // cache (the same snapshot the sidebar groups by), never a
                // kernel store read.
                let known = this.known_projects.clone();
                let bound = this.thread_projects.clone();

                let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
                    let mut menu = menu.max_w(gpui::px(320.)).scrollable(true);
                    menu = menu.label(i18n::t("sidebar-section-projects"));

                    // Recent projects: registered folders first (newest
                    // first), then session cwds not yet registered.
                    let mut recent_projects: Vec<String> = Vec::new();
                    let mut seen = std::collections::HashSet::new();
                    for path in known.iter().rev() {
                        if seen.insert(path.clone()) {
                            recent_projects.push(path.clone());
                        }
                        if recent_projects.len() >= 20 {
                            break;
                        }
                    }
                    if recent_projects.len() < 20 {
                        for path in &bound {
                            if path.is_empty() || !seen.insert(path.clone()) {
                                continue;
                            }
                            recent_projects.push(path.clone());
                            if recent_projects.len() >= 20 {
                                break;
                            }
                        }
                    }

                    let ws_recent = ws.clone();
                    let theme_recent = theme.clone();
                    for path_str in &recent_projects {
                        let path = std::path::PathBuf::from(path_str);
                        let name = path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or(path_str)
                            .to_string();
                        let display_path = path_str.clone();
                        let click_path = path_str.clone();
                        let ws_sel = ws_recent.clone();
                        let themed = theme_recent.clone();
                        menu = menu.item(
                            PopupMenuItem::element(move |_window, _cx| {
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Icon::new(IconName::FolderOpen)
                                            .xsmall()
                                            .text_color(themed.muted_foreground),
                                    )
                                    .child(
                                        gpui::div()
                                            .text_sm()
                                            .text_color(themed.foreground)
                                            .child(name.clone()),
                                    )
                                    .child(
                                        gpui::div()
                                            .flex_1()
                                            .text_xs()
                                            .text_color(themed.muted_foreground)
                                            .child(display_path.clone()),
                                    )
                            })
                            .on_click(
                                move |_, _, cx: &mut gpui::App| {
                                    let p = std::path::PathBuf::from(&click_path);
                                    let _ = ws_sel.update(cx, |this, cx| {
                                        this.close_project_chip_menu();
                                        let _ = this.send_note(|sid| {
                                            manox_protocol::ClientNote::SetCwd {
                                                session_id: sid.into(),
                                                cwd: p.to_str().unwrap_or_default().into(),
                                            }
                                        });
                                        Self::register_project_in_store(&p, cx);
                                        cx.notify();
                                    });
                                },
                            ),
                        );
                    }

                    menu = menu.separator();
                    menu = menu.label(i18n::t("workspace-project-new"));

                    let themed_blank = theme.clone();
                    menu = menu.item(
                        PopupMenuItem::element(move |_window, _cx| {
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::Plus)
                                        .xsmall()
                                        .text_color(themed_blank.muted_foreground),
                                )
                                .child(
                                    gpui::div()
                                        .text_sm()
                                        .text_color(themed_blank.foreground)
                                        .child(i18n::t("workspace-project-blank")),
                                )
                        })
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            let _ = ws_blank.update(cx, |this, cx| {
                                this.close_project_chip_menu();
                                this.open_blank_project(cx);
                            });
                        }),
                    );

                    let themed_folder = theme.clone();
                    menu = menu.item(
                        PopupMenuItem::element(move |_window, _cx| {
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::FolderOpen)
                                        .xsmall()
                                        .text_color(themed_folder.muted_foreground),
                                )
                                .child(
                                    gpui::div()
                                        .text_sm()
                                        .text_color(themed_folder.foreground)
                                        .child(i18n::t("workspace-project-select-folder")),
                                )
                        })
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            let _ = ws_folder.update(cx, |this, cx| {
                                this.close_project_chip_menu();
                                this.choose_project_inner(cx);
                            });
                        }),
                    );
                    menu
                });
                let sub = cx.subscribe(
                    &menu,
                    |this: &mut Workspace,
                     _menu: Entity<PopupMenu>,
                     _: &DismissEvent,
                     cx: &mut Context<Workspace>| {
                        this.close_project_chip_menu();
                        cx.notify();
                    },
                );
                this.project_chip_menu = Some(menu);
                this.project_chip_menu_sub = Some(sub);
                cx.notify();
            }));

        if !open {
            return trigger.into_any_element();
        }

        let menu = self
            .project_chip_menu
            .clone()
            .expect("project_chip_menu exists when open");

        gpui::div()
            .relative()
            .child(trigger)
            .child(
                deferred(
                    gpui::div()
                        .id("project-chip-dropdown")
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

    /// Register a bound project on the active variant's thread store so the
    /// sidebar keeps its folder (persisted; survives restarts and archives).
    pub(super) fn register_project_in_store(path: &std::path::Path, _cx: &mut Context<Self>) {
        let path = path.to_string_lossy().to_string();
        manox_agent::thread_store_global().with_mut(|s| s.register_project(path));
    }

    /// Open the blank-project flow: pick a parent directory, then prompt for name.
    pub(super) fn open_blank_project(&mut self, cx: &mut Context<Self>) {
        if self.project_picker_pending {
            return;
        }
        self.project_picker_pending = true;
        let dir = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let result = dir.await;
            this.update(cx, |this, cx| {
                this.project_picker_pending = false;
                if let Ok(Ok(Some(paths))) = result
                    && let Some(parent) = paths.into_iter().next()
                {
                    this.blank_project_parent = Some(parent);
                    this.blank_project_name_input = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Lazily create the blank-project name input (needs a Window).
    pub(super) fn ensure_blank_project_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.blank_project_parent.is_none() {
            return;
        }
        if self.blank_project_name_input.is_some() {
            return;
        }
        self.blank_project_name_input = Some(cx.new(|cx| InputState::new(window, cx)));
    }

    /// Submit the blank project: create the directory and bind it.
    pub(super) fn confirm_blank_project(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(parent) = self.blank_project_parent.take() else {
            return;
        };
        let name = self
            .blank_project_name_input
            .as_ref()
            .map(|s| s.read(cx).value().trim().to_string())
            .unwrap_or_default();
        if name.is_empty() {
            self.blank_project_parent = Some(parent);
            return;
        }
        let new_path = parent.join(&name);
        if let Err(e) = std::fs::create_dir_all(&new_path) {
            tracing::warn!(error = %e, "failed to create project directory");
            cx.notify();
            return;
        }
        let _ = self.send_note(|sid| manox_protocol::ClientNote::SetCwd {
            session_id: sid.into(),
            cwd: new_path.to_str().unwrap_or_default().into(),
        });
        Self::register_project_in_store(&new_path, cx);
        self.blank_project_name_input = None;
        cx.notify();
    }

    /// Cancel the blank project overlay.
    pub(super) fn cancel_blank_project(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.blank_project_parent = None;
        self.blank_project_name_input = None;
        cx.notify();
    }

    /// Shared inner logic for "Select folder" (directory picker → bind project).
    pub(super) fn choose_project_inner(&mut self, cx: &mut Context<Self>) {
        if self.project_picker_pending {
            return;
        }
        self.project_picker_pending = true;
        let dir = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let result = dir.await;
            this.update(cx, |this, cx| {
                tracing::info!(?result, "project picker completed");
                this.project_picker_pending = false;
                if let Ok(Ok(Some(paths))) = result
                    && let Some(path) = paths.into_iter().next()
                {
                    let sent = this.send_note(|sid| manox_protocol::ClientNote::SetCwd {
                        session_id: sid.into(),
                        cwd: path.to_str().unwrap_or_default().into(),
                    });
                    tracing::info!(sent, path = %path.display(), "project pick SetCwd note");
                    Self::register_project_in_store(&path, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Overlay prompting for the blank-project folder name.
    /// The generic approval card: a non-question authorization (a
    /// `sandbox_permissions` escalation, or an ask whose payload failed to
    /// parse) parked on the user's decision. Without it the pending call
    /// blocks the thread invisibly — the user sees a stuck tool, not a
    /// decision that belongs to them.
    pub(super) fn render_pending_auth_overlay(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let auth = self.pending_auth.as_ref()?;
        let detail = if auth.summary.trim().is_empty() {
            format!("{} · {}", auth.tool_name, i18n::t("pending-auth-waiting"))
        } else {
            format!("{} · {}", auth.tool_name, auth.summary)
        };
        Some(
            gpui::div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.foreground.opacity(0.6))
                .child(
                    v_flex()
                        .w(px(480.))
                        .p_4()
                        .gap_3()
                        .rounded(theme.radius)
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .shadow_lg()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Icon::new(IconName::Eye)
                                        .small()
                                        .text_color(theme.accent_foreground),
                                )
                                .child(
                                    gpui::div()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child(i18n::t("pending-auth-title")),
                                ),
                        )
                        .child(
                            gpui::div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(detail),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("pending-auth-deny")
                                        .ghost()
                                        .small()
                                        .label(i18n::t("pending-auth-deny"))
                                        .on_click(cx.listener(move |this, _, _window, cx| {
                                            this.resolve_auth(PermissionDecision::Deny, cx);
                                        })),
                                )
                                .child(
                                    Button::new("pending-auth-allow")
                                        .primary()
                                        .small()
                                        .label(i18n::t("pending-auth-allow"))
                                        .on_click(cx.listener(move |this, _, _window, cx| {
                                            this.resolve_auth(PermissionDecision::AllowOnce, cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    pub(super) fn render_blank_project_overlay(
        &self,
        _window: &mut Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.pending_ask.is_some()
            || self.pending_plan_review.is_some()
            || self.pending_auth.is_some()
        {
            return None;
        }
        self.blank_project_parent.as_ref()?;
        let input = self.blank_project_name_input.as_ref()?;
        let parent_name = self
            .blank_project_parent
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("…")
            .to_string();

        Some(
            gpui::div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                // Scrim must use the dark foreground, not `background`. A white
                // veil over a white conversation does not dim, so the page shows
                // through and the modal reads as transparent.
                .bg(theme.foreground.opacity(0.6))
                .child(
                    v_flex()
                        .w(px(480.))
                        .p_4()
                        .gap_3()
                        .rounded(theme.radius)
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .shadow_lg()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Icon::new(IconName::FolderOpen)
                                        .small()
                                        .text_color(theme.accent_foreground),
                                )
                                .child(
                                    gpui::div()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child(i18n::t("workspace-project-blank")),
                                ),
                        )
                        .child(
                            gpui::div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(format!(
                                    "{}: {}",
                                    i18n::t("workspace-project-name-prompt"),
                                    parent_name
                                )),
                        )
                        .child(Input::new(input))
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("blank-project-cancel")
                                        .ghost()
                                        .small()
                                        .label(i18n::t("workspace-cancel"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.cancel_blank_project(window, cx);
                                        })),
                                )
                                .child(
                                    Button::new("blank-project-confirm")
                                        .primary()
                                        .small()
                                        .label(i18n::t("workspace-rename-confirm"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.confirm_blank_project(window, cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}
