//! The composer turn surface (U9b cluster 3): submit and its turn
//! builders, the steer/queued follow-up lifecycle (parked stash flush,
//! pending bubbles, stranded-steer healing, drop/undo), and the recall
//! walk over sent turns. Split from `workspace.rs` — `super` is the
//! workspace module; private methods are lifted `pub(super)` so the
//! parent (render/keyboard handlers) and its `tests` child keep
//! calling them. The generic wire helpers they ride (`send_note`,
//! `send_submit_v2`) stay in the parent.

use super::*;

/// Decode one wire message content block into a protocol image attachment
/// (round 4 §5.1): the base64 `Image { data, mime_type }` shape four call
/// sites used to repeat verbatim. Non-image blocks are `None`; a failed
/// decode is dropped (an undecodable attachment cannot ride the wire).
fn wire_image_attachment(
    c: &manox_agent::language_model::MessageContent,
) -> Option<manox_protocol::ImageAttachment> {
    use base64::Engine as _;
    match c {
        manox_agent::language_model::MessageContent::Image { data, mime_type } => {
            base64::engine::general_purpose::STANDARD
                .decode(data.as_bytes())
                .ok()
                .map(|bytes| manox_protocol::ImageAttachment {
                    data: bytes,
                    mime_type: mime_type.clone(),
                })
        }
        _ => None,
    }
}

impl Workspace {
    /// Flip the parked thread's `SteerPending` cards whose steers an aborted
    /// turn never drained to `Failed`, mirroring the foreground
    /// `mark_stranded_steers_failed` against the per-thread stash. A parked
    /// thread has no live conversation bubble to roll back (the optimistic
    /// bubble died with the conversation entity on switch-away), so only the
    /// queue state changes; the retry affordance renders once the stash is
    /// restored on switch-back.
    pub(super) fn mark_parked_stranded_steers_failed(
        &mut self,
        thread_id: &str,
        stranded_steer_ids: &[String],
    ) {
        let stranded: std::collections::HashSet<&str> =
            stranded_steer_ids.iter().map(String::as_str).collect();
        if stranded.is_empty() {
            return;
        }
        let Some(queue) = self.queued_follow_ups_by_thread.get_mut(thread_id) else {
            return;
        };
        for item in queue.iter_mut() {
            if let FollowUpState::SteerPending { message_id } = &item.state
                && stranded.contains(message_id.as_str())
            {
                // Keep the id so a later `SteerInjected` can still heal this
                // provisional rollback (via `consume_background_steer`).
                let message_id = message_id.clone();
                item.state = FollowUpState::Failed { message_id };
            }
        }
    }

    /// Drain the stashed `Queued` follow-ups of a parked thread whose turn
    /// just settled: the follow-up turn runs on the parked thread itself,
    /// mirroring the foreground flush without a conversation bubble (a
    /// switch-back rebuild shows it through the thread mirror). U1-flush:
    /// the flush rides the gateway — all but the last drain as
    /// `AppendUserMessage` notes (accept without a run) and the last as the
    /// v2 `Submit` that starts the next turn, matching the foreground
    /// flush's batching. The pre-migration direct facade write
    /// (`insert_user_message` + `run_turn`) bypassed the journal's
    /// accept-time persistence (K5) and the server's queue/drain merge; the
    /// parked thread has no optimistic bubble, so no echo is pushed — the
    /// origin_rpc still rides the Submit for the entry correlation.
    /// `SteerPending`/`Failed` cards stay parked for the user.
    pub(super) fn flush_parked_follow_ups(&mut self, thread_id: &str, _cx: &mut Context<Self>) {
        let Some(queue) = self.queued_follow_ups_by_thread.get_mut(thread_id) else {
            return;
        };
        let mut retain: std::collections::VecDeque<QueuedFollowUp> =
            std::collections::VecDeque::new();
        let mut drained: Vec<DeferredUserTurn> = Vec::new();
        while let Some(item) = queue.pop_front() {
            if matches!(&item.state, FollowUpState::Queued) {
                drained.push(item.turn);
            } else {
                retain.push_back(item);
            }
        }
        if retain.is_empty() {
            self.queued_follow_ups_by_thread.remove(thread_id);
        } else {
            *queue = retain;
        }
        if drained.is_empty() {
            return;
        }
        let n = drained.len();
        for (i, turn) in drained.into_iter().enumerate() {
            let attachments: Vec<manox_protocol::ImageAttachment> = turn
                .images
                .iter()
                .filter_map(wire_image_attachment)
                .collect();
            if i + 1 < n {
                // All but the last: accept without starting a run.
                self.client
                    .send_note(manox_protocol::ClientNote::AppendUserMessage {
                        session_id: thread_id.to_string(),
                        text: turn.text.clone(),
                        images: attachments,
                    });
            } else {
                // Last: accept + start the turn (K5 persists at acceptance;
                // the server's queue/drain merges anything racing in).
                self.client.send_call(manox_protocol::ClientCall::Submit {
                    session_id: thread_id.to_string(),
                    text: turn.text,
                    images: attachments,
                    origin_rpc: Some(uuid::Uuid::new_v4().to_string()),
                });
            }
        }
    }

    pub(crate) fn submit_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        tracing::info!("composer submit fired");
        // T10c (§D.6): the v1 `history_phase` loading gate is gone — the
        // follow stream's Snapshot → window Replace is the restore boundary,
        // and HEAD already carried an unwritten (default `Ready`) phase.
        // Successor gate (when §K.5 lands): a "no snapshot yet" flag on the
        // v2 fold.
        let text = self.input_state.read(cx).value().to_string();
        let attachments = std::mem::take(&mut self.pending_attachments);
        if self.pending_ask.is_some() {
            self.pending_attachments = attachments;
            if !text.trim().is_empty() || self.pending_ask_has_selection() {
                self.input_state
                    .update(cx, |state, cx| state.set_value("", window, cx));
                // Submitting ends the walk: nothing is left to return to.
                self.end_recall_walk();
                self.close_completion(cx);
                self.resolve_ask_with_response(Some(text), cx);
            }
            return;
        }
        // Block submit on empty input or while the project picker is open.
        // Setting the project after a message lands is a no-op (`set_project`
        // guards on `!messages.is_empty()`), so the project would be silently
        // dropped. A message submitted while a turn is running is *not* dropped
        // here — it is routed through `send_user_turn`, which enqueues it as a
        // follow-up instead of interrupting the running turn.
        if (text.trim().is_empty() && attachments.is_empty()) || self.project_picker_pending {
            tracing::info!(
                pending_picker = self.project_picker_pending,
                "submit swallowed by composer guard"
            );
            self.pending_attachments = attachments;
            return;
        }
        self.input_state
            .update(cx, |state, cx| state.set_value("", window, cx));
        // Submitting ends the walk: nothing is left to return to.
        self.end_recall_walk();
        self.close_completion(cx);

        // Slash commands (line-initial `/name [args]`) are intercepted before
        // sending a normal user turn. A recognized command fully handles the
        // input (Handled), asks to inject text as a user turn (InjectUserTurn),
        // or declines (NoOp → fall through to the normal path). Slash parsing
        // only applies to text-only input; attachments force the normal path.
        // Slash commands only dispatch while idle — a `/name [args]` typed
        // while a turn is running is parked in the follow-up queue as raw text
        // rather than interrupting the run (e.g. `/clear` mid-turn would race
        // the streaming conversation). The queued text flushes at turn end.
        if !self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .expect("foreground store present")
            && attachments.is_empty()
            && let Some(parsed) = crate::slash_command::parse(&text)
        {
            let result = crate::slash_command::dispatch(&parsed, self, window, cx);
            match result {
                crate::slash_command::SlashResult::Handled => return,
                crate::slash_command::SlashResult::InjectUserTurn(msg) => {
                    self.send_user_turn(msg, Vec::new(), cx);
                    return;
                }
                crate::slash_command::SlashResult::NoOp => {}
            }
        }

        if attachments.is_empty() {
            self.send_user_turn(text, Vec::new(), cx);
            return;
        }

        // Reading attachment bytes is blocking IO; do it off the UI thread, then start the turn.
        cx.spawn(async move |this, cx| {
            let (text, extra, failed) = cx
                .background_spawn(async move {
                    let mut text = text;
                    let mut extra = Vec::new();
                    let mut failed = 0usize;
                    for att in &attachments {
                        match att {
                            PendingAttachment::ClipboardImage(img) => {
                                let format = match img.format {
                                    gpui::ImageFormat::Png => {
                                        Some(manox_agent::image::PastedImageFormat::Png)
                                    }
                                    gpui::ImageFormat::Jpeg => {
                                        Some(manox_agent::image::PastedImageFormat::Jpeg)
                                    }
                                    gpui::ImageFormat::Webp => {
                                        Some(manox_agent::image::PastedImageFormat::Webp)
                                    }
                                    gpui::ImageFormat::Gif => {
                                        Some(manox_agent::image::PastedImageFormat::Gif)
                                    }
                                    gpui::ImageFormat::Bmp => {
                                        Some(manox_agent::image::PastedImageFormat::Bmp)
                                    }
                                    gpui::ImageFormat::Tiff => {
                                        Some(manox_agent::image::PastedImageFormat::Tiff)
                                    }
                                    // SVG/ICO/PNM aren't raster-decodable; drop them.
                                    _ => None,
                                };
                                let content = format.and_then(|format| {
                                    manox_agent::image::pasted_image_to_message_content(
                                        &manox_agent::image::PastedImage {
                                            format,
                                            bytes: img.bytes.to_vec(),
                                        },
                                    )
                                });
                                match content {
                                    Some(content) => extra.push(content),
                                    None => failed += 1,
                                }
                            }
                            PendingAttachment::File { .. } => {
                                if let Some(content) = load_attachment(att, &mut text) {
                                    extra.push(content);
                                }
                            }
                        }
                    }
                    (text, extra, failed)
                })
                .await;
            this.update(cx, |this, cx| {
                this.send_user_turn(text, extra, cx);
                if failed > 0 {
                    this.add_info_message(
                        i18n::t("composer-image-process-failed").to_string(),
                        NoticeAnchor::TurnEnd,
                        None,
                        cx,
                    );
                }
            })
            .ok();
        })
        .detach();
    }

    /// Stage a clipboard image as a pending attachment chip. Resize happens
    /// off-thread on submit; here we only record the image and re-render.
    pub(super) fn handle_pasted_image(&mut self, image: gpui::Image, cx: &mut Context<Self>) {
        self.pending_attachments
            .push(PendingAttachment::ClipboardImage(image));
        cx.notify();
    }

    /// Run a markdown prompt-macro slash turn (`/gitwork:deliver args`).
    pub(crate) fn run_command_turn(&mut self, name: &str, args: &str, cx: &mut Context<Self>) {
        self.run_registry_turn(RegistryTurnKind::Command, name, args, cx);
    }

    /// Run a skill slash turn (`/plugin:skill args` or bare `/skill`).
    pub(crate) fn run_skill_turn(&mut self, key: &str, args: &str, cx: &mut Context<Self>) {
        self.run_registry_turn(RegistryTurnKind::Skill, key, args, cx);
    }

    /// Shared dispatch for registry-backed slash turns: push the compact
    /// `/key args` display bubble, then hand the expanded body to the thread
    /// (`submit_command` / `submit_skill` render the registry content and run
    /// the turn). A registry miss surfaces as a thread error event. The
    /// transcript stores the expanded body, so a reloaded thread shows the
    /// body rather than the compact bubble (parity with the retired manox
    /// harness).
    pub(super) fn run_registry_turn(
        &mut self,
        kind: RegistryTurnKind,
        key: &str,
        args: &str,
        cx: &mut Context<Self>,
    ) {
        let display_text = if args.is_empty() {
            format!("/{key}")
        } else {
            format!("/{key} {args}")
        };
        let meta = self.user_turn_meta(cx);
        // Persist the send-time chrome with the turn: the expanded macro/skill
        // body is the model-facing text, and `display_text` keeps the bubble
        // showing the compact `/key args` invocation after a reload — the same
        // form the live view shows at send time.
        let weak = cx.weak_entity();
        self.conversation.update(cx, |c, cx| {
            c.push_user(display_text, Vec::new(), meta, weak, cx)
        });
        self.sync_list_count(cx);
        // Re-engage tail-follow so the streaming reply stays in view.
        self.follow_message_tail();
        // U2: the registry hit check reads the gateway's command snapshot —
        // the server projects the same command/skill registries the macro
        // and skill adapters dispatch against, so a remote server's
        // registry decides the hit.
        let commands = self.multiplexer.read(cx).commands().clone();
        let hit = match kind {
            RegistryTurnKind::Command => wire_commands_has(&commands, key, "command"),
            RegistryTurnKind::Skill => wire_commands_has(&commands, key, "skill"),
        };
        if hit {
            let submit_text = format!("/{key} {args}");
            let _ = self.send_submit_v2(submit_text, Vec::new(), cx);
        } else {
            let i18n_key = match kind {
                RegistryTurnKind::Command => "workspace-unknown-command",
                RegistryTurnKind::Skill => "workspace-unknown-skill",
            };
            tracing::warn!("unknown {}", i18n::t_str(i18n_key, &[("name", key)]));
        }
        // Persist on submit so the sidebar shows the new entry immediately
        // (cross-domain #5: the wire refetch — the server self-holds the
        // rescan in its answer).
        self.multiplexer.update(cx, |m, _| m.fetch_thread_list());
        cx.notify();
    }

    /// Append the user turn (text plus any image content) to the thread and start the run.
    pub(super) fn send_user_turn(
        &mut self,
        text: String,
        images: Vec<manox_agent::language_model::MessageContent>,
        cx: &mut Context<Self>,
    ) {
        let meta = self.user_turn_meta(cx);
        let ui = Self::message_ui_metadata(&meta);
        let weak = cx.weak_entity();
        let user_images = Self::decode_user_images(&images);
        let turn = DeferredUserTurn {
            text,
            images,
            meta,
            ui,
            user_images,
        };
        // A turn is running — every new message first parks as a visible
        // follow-up. Steering is an explicit per-item action.
        if self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .expect("foreground store present")
        {
            self.queued_follow_ups.push_back(QueuedFollowUp {
                turn,
                state: FollowUpState::Queued,
            });
            cx.notify();
            return;
        }
        // The thread is idle (not running). If a plan review was awaiting a
        // verdict, a free-form message means the user is discussing or revising
        // rather than accepting — drop the stale verdict so the card hides
        // until the agent re-proposes via a fresh `PlanReady`. Without this the
        // lingering Implement button would act on the now-outdated plan text.
        let dismissed_plan = self.pending_plan_review.take();
        if let Some(review) = dismissed_plan.as_ref() {
            // U6b②/④: the DURABLE dismissal lives server-side — the
            // gateway's Submit arm runs the implicit dismissal (it clears
            // the session facade, whose engine journals the `resolved`
            // plan_review edge, and the store badge). The desktop's attach
            // mirror has no engine and the state is session-owned; the
            // UI-side half (card consume, collapsed record) stays here.
            self.conversation
                .update(cx, |c, cx| c.consume_plan_review(cx));
            // The demoted plan card stays in place but flips inactive —
            // remeasure so the list's cached height for it stays honest.
            self.list_state.remeasure();
            // Persist the dismissed plan as a UI note so the collapsed record
            // survives a thread switch / reload — the live card is UI-only and
            // never enters `Thread::messages`. The append lands before the
            // dismissing message's prompt dispatch below (single actor
            // queue), so the rebuilt conversation shows the card ahead of
            // this dismissing message — matching the live order.
            self.append_ui_note(
                manox_agent::db::UiNoteKind::PlanReview,
                review.content.clone(),
                None,
                cx,
            );
        }
        self.append_and_run_user_turn(turn, weak, cx);
        // The conversation exists the moment the message is sent: refetch
        // the sidebar list now (the transcript-side refetch on the user
        // MessageEnd notice then fills in the summary text).
        self.multiplexer.update(cx, |m, _| m.fetch_thread_list());
    }

    /// Hand a parked follow-up to the thread's steer queue — the running turn
    /// absorbs it at the next safe join point. No conversation bubble is pushed
    /// here. The caller immediately pushes an optimistic pending bubble; the
    /// later `SteerInjected` event confirms it. Returns the message id used to
    /// correlate the pending bubble and the canonical history message.
    pub(super) fn enqueue_steer_pending(
        &mut self,
        turn: &mut DeferredUserTurn,
        _cx: &mut Context<Self>,
    ) -> String {
        use manox_agent::language_model::MessageContent;
        let mut content = Vec::with_capacity(turn.images.len() + 1);
        if !turn.text.trim().is_empty() {
            content.push(MessageContent::Text(turn.text.clone()));
        }
        content.append(&mut turn.images);
        let ui = std::mem::take(&mut turn.ui);
        // The steered tag is applied only when the drain succeeds (in
        // `consume_steered_follow_up`), so a stranded steer that the user
        // later retries as an idle fresh turn carries no badge — it was never
        // actually injected.
        self.thread
            .with_mut(|thread| thread.enqueue_steer(content, Some(ui)))
    }

    pub(super) fn consume_background_steer(&mut self, thread_id: &str, message_id: &str) {
        let Some(queue) = self.queued_follow_ups_by_thread.get_mut(thread_id) else {
            return;
        };
        if let Some(index) = queue.iter().position(|item| {
            matches!(
                &item.state,
                FollowUpState::SteerPending { message_id: id }
                    | FollowUpState::Failed { message_id: id }
                    if id == message_id
            )
        }) {
            queue.remove(index);
        }
        if queue.is_empty() {
            self.queued_follow_ups_by_thread.remove(thread_id);
        }
    }

    /// Render the optimistic message-list bubble for a steer that is still
    /// waiting in `Thread::pending_steer`.
    pub(super) fn push_pending_steer_bubble(
        &mut self,
        turn: &DeferredUserTurn,
        message_id: &str,
        cx: &mut Context<Self>,
    ) {
        let weak = cx.weak_entity();
        self.conversation.update(cx, |conversation, cx| {
            conversation.push_pending_steer(
                turn.text.clone(),
                turn.user_images.clone(),
                turn.meta.clone(),
                message_id.to_string(),
                weak,
                cx,
            )
        });
        self.sync_list_count(cx);
        self.list_state.set_follow_mode(FollowMode::Tail);
    }

    pub(super) fn append_and_run_user_turn(
        &mut self,
        turn: DeferredUserTurn,
        weak: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) {
        // UI state tracking (always) — conversation bubble + list housekeeping.
        self.conversation.update(cx, |c, cx| {
            c.push_user(
                turn.text.clone(),
                turn.user_images.clone(),
                turn.meta.clone(),
                weak,
                cx,
            )
        });
        self.sync_list_count(cx);
        self.follow_message_tail();
        // Dual-path: protocol Submit (kernel inserts + runs) vs direct insert + run.
        let attachments: Vec<manox_protocol::ImageAttachment> = turn
            .images
            .iter()
            .filter_map(wire_image_attachment)
            .collect();
        let _ = self.send_submit_v2(turn.text.clone(), attachments, cx);
    }

    /// Drain every parked `Queued` follow-up into a single new turn. Multiple
    /// messages coalesce into one user block, keeping the request prefix stable
    /// (mirrors the team inbox flush). `Failed` cards stay parked for the user
    /// to retry; `SteerPending` cards are marked `Failed` by
    /// `mark_stranded_steers_failed` before this runs, so none reach here.
    pub(super) fn flush_queued_follow_ups(&mut self, cx: &mut Context<Self>) {
        if self.queued_follow_ups.is_empty() {
            return;
        }
        let weak = cx.weak_entity();
        let follow_tail = self.list_state.is_following_tail();
        let mut retain: Vec<QueuedFollowUp> = Vec::new();
        let mut drained_turns: Vec<DeferredUserTurn> = Vec::new();
        while let Some(item) = self.queued_follow_ups.pop_front() {
            match item.state {
                FollowUpState::Queued => {
                    self.conversation.update(cx, |c, cx| {
                        c.push_user(
                            item.turn.text.clone(),
                            item.turn.user_images.clone(),
                            item.turn.meta.clone(),
                            weak.clone(),
                            cx,
                        )
                    });
                    self.sync_list_count(cx);
                    if follow_tail {
                        self.follow_message_tail();
                    }
                    drained_turns.push(item.turn);
                }
                _ => retain.push(item),
            }
        }
        self.queued_follow_ups.extend(retain);
        if drained_turns.is_empty() {
            cx.notify();
            return;
        }
        let n = drained_turns.len();
        for (i, turn) in drained_turns.into_iter().enumerate() {
            let attachments: Vec<manox_protocol::ImageAttachment> = turn
                .images
                .iter()
                .filter_map(wire_image_attachment)
                .collect();
            if i + 1 < n {
                // All but the last: insert without running.
                let _ = self.send_note(|sid| manox_protocol::ClientNote::AppendUserMessage {
                    session_id: sid.into(),
                    text: turn.text.clone(),
                    images: attachments,
                });
            } else {
                // Last: insert + start the turn. The v2 Submit carries the
                // origin_rpc so the optimistic bubble retires when the durable
                // user row lands (batched predecessors insert without a turn
                // via the compat note and have no echo to retire).
                let _ = self.send_submit_v2(turn.text.clone(), attachments, cx);
            }
        }
        self.multiplexer.update(cx, |m, _| m.fetch_thread_list());
        cx.notify();
    }

    /// Mark every `SteerPending` card whose message id is still in the thread's
    /// steer queue as `Failed` — the running turn exited (terminal Stop/Error)
    /// before draining them, so they are stranded. Called at terminal states so
    /// the cards stop spinning and surface a retry instead of hanging forever.
    pub(super) fn mark_stranded_steers_failed(
        &mut self,
        stranded_steer_ids: &[String],
        cx: &mut Context<Self>,
    ) {
        let stranded: std::collections::HashSet<&str> =
            stranded_steer_ids.iter().map(String::as_str).collect();
        if stranded.is_empty() {
            return;
        }
        let mut any = false;
        for item in self.queued_follow_ups.iter_mut() {
            if let FollowUpState::SteerPending { message_id } = &item.state
                && stranded.contains(message_id.as_str())
            {
                self.conversation.update(cx, |conversation, cx| {
                    conversation.rollback_pending_steer(message_id, cx);
                });
                self.list_state.remeasure();
                // Keep the id so a later `SteerInjected` can still heal this
                // provisional rollback.
                let message_id = message_id.clone();
                item.state = FollowUpState::Failed { message_id };
                any = true;
            }
        }
        if any {
            cx.notify();
        }
    }

    /// Confirm the optimistic bubble whose canonical message was just drained.
    /// `Failed` also matches so a late drain can heal a provisional rollback.
    pub(super) fn consume_steered_follow_up(&mut self, message_id: &str, cx: &mut Context<Self>) {
        // Match either SteerPending or the prematurely-marked Failed card
        // (see the doc comment above) by the steer message id.
        let id_matches = |f: &QueuedFollowUp| match &f.state {
            FollowUpState::SteerPending { message_id: mid }
            | FollowUpState::Failed { message_id: mid } => mid == message_id,
            FollowUpState::Queued => false,
        };
        let Some(idx) = self.queued_follow_ups.iter().position(id_matches) else {
            return;
        };
        let Some(_) = self.queued_follow_ups.remove(idx) else {
            return;
        };
        self.conversation.update(cx, |c, cx| {
            c.confirm_pending_steer(message_id, cx);
        });
        self.list_state.remeasure();
        // Only re-engage tail-follow if the user hasn't scrolled away —
        // a steer injection is event-driven, not user-initiated, so it
        // should not yank the viewport back to the bottom.
        if self.list_state.is_following_tail() {
            self.list_state.scroll_to_end();
        }
        cx.notify();
    }

    /// Promote a parked follow-up to a steer. While running, enqueue it and
    /// immediately move an optimistic bubble into the message list. While idle,
    /// send it as a fresh ordinary turn.
    pub(super) fn steer_follow_up(&mut self, idx: usize, cx: &mut Context<Self>) {
        let running = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .expect("foreground store present");
        let Some(mut item) = self.queued_follow_ups.remove(idx) else {
            return;
        };
        match item.state {
            FollowUpState::Queued => {
                if running {
                    let id = self.enqueue_steer_pending(&mut item.turn, cx);
                    self.push_pending_steer_bubble(&item.turn, &id, cx);
                    item.state = FollowUpState::SteerPending { message_id: id };
                    self.queued_follow_ups.insert(idx, item);
                    cx.notify();
                } else {
                    let weak = cx.weak_entity();
                    self.append_and_run_user_turn(item.turn, weak, cx);
                }
            }
            FollowUpState::Failed { message_id } => {
                // Drop the stranded message so its id can never drain into a
                // later turn (no-op if the loop already cleared the queue).
                self.thread
                    .with_mut(|thread| thread.cancel_pending_steer(&message_id));
                if running {
                    let id = self.enqueue_steer_pending(&mut item.turn, cx);
                    self.push_pending_steer_bubble(&item.turn, &id, cx);
                    item.state = FollowUpState::SteerPending { message_id: id };
                    self.queued_follow_ups.insert(idx, item);
                    cx.notify();
                } else {
                    let weak = cx.weak_entity();
                    self.append_and_run_user_turn(item.turn, weak, cx);
                }
            }
            FollowUpState::SteerPending { .. } => {
                // Already in flight; restore untouched.
                self.queued_follow_ups.insert(idx, item);
            }
        }
    }

    pub(super) fn delete_follow_up(&mut self, idx: usize, cx: &mut Context<Self>) {
        if let Some(item) = self.queued_follow_ups.remove(idx) {
            // A SteerPending or Failed card has a message that may still sit in
            // the thread's steer queue (SteerPending: enqueued and pending
            // drain; Failed: stranded, pending the loop's end-of-turn clear).
            // Removing the UI card alone would let that message drain later with
            // no matching card, so the steer would fire invisibly. Cancel it in
            // the thread too (no-op if already drained/cleared).
            let steer_id = match &item.state {
                FollowUpState::SteerPending { message_id }
                | FollowUpState::Failed { message_id } => Some(message_id.clone()),
                FollowUpState::Queued => None,
            };
            if let Some(id) = steer_id {
                self.thread
                    .with_mut(|thread| thread.cancel_pending_steer(&id));
                self.conversation.update(cx, |conversation, cx| {
                    conversation.rollback_pending_steer(&id, cx);
                });
                self.list_state.remeasure();
            }
            cx.notify();
        }
    }

    pub(super) fn undo_last_queued(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = self.queued_follow_ups.pop_back() {
            let steer_id = match &item.state {
                FollowUpState::SteerPending { message_id }
                | FollowUpState::Failed { message_id } => Some(message_id.clone()),
                FollowUpState::Queued => None,
            };
            if let Some(id) = steer_id {
                self.thread
                    .with_mut(|thread| thread.cancel_pending_steer(&id));
                self.conversation.update(cx, |conversation, cx| {
                    conversation.rollback_pending_steer(&id, cx);
                });
                self.list_state.remeasure();
            }
            cx.notify();
        }
    }

    /// Decode provider-bound image contents into UI-preview `UserImage`s. The
    /// canonical `MessageContent::Image` bytes still go to the thread; this
    /// only rebuilds a gpui image for the user bubble.
    pub(super) fn decode_user_images(
        images: &[manox_agent::language_model::MessageContent],
    ) -> Vec<UserImage> {
        images
            .iter()
            .filter_map(|c| {
                let attachment = wire_image_attachment(c)?;
                let fmt = gpui::ImageFormat::from_mime_type(attachment.mime_type.as_str())?;
                Some(UserImage(std::sync::Arc::new(gpui::Image::from_bytes(
                    fmt,
                    attachment.data,
                ))))
            })
            .collect()
    }

    /// Pure history-recall step for the composer. `turns` is newest-first;
    /// `index` < 0 means no walk is running, and `draft` is the walk's working
    /// line — the text a `Down` past the newest turn returns to.
    ///
    /// Entering and continuing both take an explicit key (`alt-up` /
    /// `alt-down`), so no text or caret state has to be watched to leave a
    /// walk and nothing compares the input against the recalled text. Whenever
    /// a step replaces the input, the text being left behind becomes the
    /// working line: the draft from walk entry, or a recalled turn the user
    /// has since changed — readline's history slot at position 0, which is
    /// what keeps an edit alive across a round trip back down.
    pub(super) fn recall_step(
        direction: RecallDirection,
        value: &str,
        index: i64,
        draft: Option<&str>,
        turns: &[String],
    ) -> (i64, Option<String>, RecallStep) {
        let walked = usize::try_from(index)
            .ok()
            .and_then(|ix| turns.get(ix).map(|_| ix));
        let left = match walked {
            Some(ix) if turns[ix] != value => Some(value.to_string()),
            Some(_) => draft.map(str::to_string),
            None => (!value.is_empty()).then(|| value.to_string()),
        };
        match (direction, walked) {
            (RecallDirection::Up, Some(ix)) => match turns.get(ix + 1) {
                Some(older) => (ix as i64 + 1, left, RecallStep::Recall(older.clone())),
                None => (index, left, RecallStep::None),
            },
            (RecallDirection::Up, None) => match turns.first() {
                Some(newest) => (0, left, RecallStep::Recall(newest.clone())),
                None => (-1, draft.map(str::to_string), RecallStep::None),
            },
            (RecallDirection::Down, Some(0)) => (
                -1,
                None,
                match left {
                    Some(working) => RecallStep::Recall(working),
                    None => RecallStep::Clear,
                },
            ),
            // A walked index always has a newer slot: `walked` came from
            // `turns.get(ix)`, and `ix == 0` is the arm above.
            (RecallDirection::Down, Some(ix)) => (
                ix as i64 - 1,
                left,
                RecallStep::Recall(turns[ix - 1].clone()),
            ),
            (RecallDirection::Down, None) => (-1, draft.map(str::to_string), RecallStep::None),
        }
    }

    pub(super) fn composer_recall_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_recall_step(RecallDirection::Up, window, cx);
    }

    pub(super) fn composer_recall_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_recall_step(RecallDirection::Down, window, cx);
    }

    pub(super) fn apply_recall_step(
        &mut self,
        direction: RecallDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let turns = self.recall_turns(cx);
        let value = self.input_state.read(cx).value().to_string();
        let (index, draft, step) = Self::recall_step(
            direction,
            &value,
            self.recall_index,
            self.recall_draft.as_deref(),
            &turns,
        );
        self.recall_index = index;
        self.recall_draft = draft;
        match step {
            RecallStep::None => {}
            RecallStep::Recall(text) => {
                self.set_composer_text(text, window, cx);
                cx.stop_propagation();
            }
            RecallStep::Clear => {
                self.set_composer_text(String::new(), window, cx);
                cx.stop_propagation();
            }
        }
    }

    /// Replace the composer's whole content and park the caret at its end.
    /// `set_value` emits no `Change`, so whatever must react to the new text
    /// (completion re-sync, recall bookkeeping) is driven by the caller.
    pub(super) fn set_composer_text(
        &mut self,
        text: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input_state.update(cx, |state, cx| {
            state.set_value(text.into(), window, cx);
            let end = RopeExt::offset_to_position(state.text(), state.text().len());
            state.set_cursor_position(end, window, cx);
        });
        cx.notify();
    }

    /// End a running recall walk and drop its working line.
    pub(super) fn end_recall_walk(&mut self) {
        self.recall_index = -1;
        self.recall_draft = None;
    }

    /// Refill the composer with a past turn picked in the turn navigator
    /// (cmd/ctrl-Enter). A fill is a recall landing: the walk moves onto the
    /// filled turn, so `alt-down` off the newest end hands back whatever the
    /// fill displaced — `set_value` clears the input's undo history, and that
    /// working line is otherwise the only surviving copy of the user's draft.
    /// A turn with no text to recall is no fill at all, and a walk already
    /// running keeps the draft it started with.
    pub(super) fn fill_composer_from_turn(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let landed = self
            .recall_turns(cx)
            .iter()
            .position(|turn| *turn == text)
            .map(|ix| ix as i64);
        if let Some(index) = landed {
            let displaced = self.input_state.read(cx).value().to_string();
            if self.recall_index < 0 {
                self.recall_draft =
                    (!displaced.is_empty() && displaced != text).then(|| displaced.clone());
            }
            self.recall_index = index;
        } else {
            self.end_recall_walk();
        }
        self.close_completion(cx);
        self.set_composer_text(text, window, cx);
        self.input_state
            .update(cx, |state, cx| state.focus(window, cx));
    }

    /// Newest-first user-turn texts for recall, mirroring the navigator's
    /// `collect_user_turns` ordering minus image-only/empty turns.
    pub(super) fn recall_turns(&self, cx: &App) -> Vec<String> {
        collect_user_turns(
            self.conversation
                .read(cx)
                .items()
                .iter()
                .enumerate()
                .map(|(ix, item)| (ix, item.read(cx).kind())),
        )
        .into_iter()
        .filter(|t| !t.text.trim().is_empty())
        .map(|t| t.text)
        .collect()
    }
}
