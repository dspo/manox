//! The plan-review verdict surface (U9b cluster 2): the plan-mode
//! mirror read, the plan-mode toggle, the verdict reply/cancel legs,
//! and the four-choice `respond_plan_review` orchestration (refine /
//! execute / execute-fresh via the U6b③ create-intent +
//! PlanSeedExecution wire sequence / compact). Split from
//! `workspace.rs` — `super` is the workspace module; private methods
//! are lifted `pub(super)` so the parent (render handlers) and its
//! `tests` child keep calling them.

use super::*;

impl Workspace {
    /// Resolve the pending plan review with the user's three-way verdict.
    /// Implement (with or without a context clear) delegates to the thread,
    /// which exits Plan mode and re-injects the plan as the implement turn's
    /// seed; the rail's plan overview seeds later from the model's first
    /// `UpdatePlan` call. Staying in Plan mode is not a verdict — the user
    /// simply keeps typing.
    /// Plan mode active on the current thread (drives the composer chip).
    pub(crate) fn thread_plan_mode(&self, cx: &mut Context<Self>) -> bool {
        self.store
            .as_ref()
            .map(|s| s.read(cx).store.plan_mode)
            .expect("foreground store present")
    }

    /// Toggle plan mode on the current thread (persisted by the engine).
    pub(crate) fn set_thread_plan_mode(&mut self, enabled: bool, _cx: &mut Context<Self>) {
        let _ = self.send_note(|sid| manox_protocol::ClientNote::SetPlanMode {
            session_id: sid.into(),
            enabled,
        });
    }

    /// The user's verdict on a proposed plan — four options: execute in
    /// fresh context / compact then execute / keep context and
    /// execute / refine. Execute verdicts exit plan mode and run the rendered
    /// execution seed referencing the plan file; refine keeps plan mode on
    /// and waits for the user's feedback turn.
    /// Answer the captured `PlanVerdict` delivery (GW3/U3b): the `Reply`
    /// rides the captured MsgId, and the server's verdict arms consume the
    /// review flag, clear the pending-plan badge, and seed an execution.
    /// Returns false when the correlation is absent (the Request never
    /// reached the leaf) — the caller falls back to the in-process facade
    /// path.
    pub(super) fn reply_plan_verdict_choice(
        &self,
        plan_file: &str,
        choice: &str,
        cx: &App,
    ) -> bool {
        let Some(msg_id) = self.store.as_ref().and_then(|s| {
            s.read(cx)
                .store
                .pending_plan_verdict
                .get(plan_file)
                .cloned()
        }) else {
            return false;
        };
        self.client
            .send_reply(msg_id, Ok(serde_json::json!({ "choice": choice })));
        true
    }

    /// Withdraw the captured `PlanVerdict` delivery (GW3): a
    /// `CancelDelivery` retires the server waterfall at once — the GW9
    /// cancel path converges it (never executes). Used when the verdict
    /// ABANDONS this thread's plan (`ExecuteFresh` continues on a fresh
    /// thread), where a stale delivery would otherwise hang until the 300s
    /// expire and converge as a rejection against the archived thread.
    /// Returns false when the correlation is absent.
    pub(super) fn cancel_plan_verdict_delivery(&self, plan_file: &str, cx: &App) -> bool {
        let Some(delivery_id) = self.store.as_ref().and_then(|s| {
            s.read(cx)
                .store
                .pending_plan_delivery
                .get(plan_file)
                .cloned()
        }) else {
            return false;
        };
        self.client
            .send_call(manox_protocol::ClientCall::CancelDelivery { delivery_id });
        true
    }

    pub(crate) fn respond_plan_review(
        &mut self,
        choice: PlanReviewChoice,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = self.pending_plan_review.take() else {
            return;
        };
        // Every verdict consumes the card; the kernel review flag AND the
        // store badge both clear server-side (U3b, ledger-sanctioned):
        // every branch below answers the delivery (Refine/Execute Reply,
        // ExecuteFresh CancelDelivery), and the server's verdict arms drop
        // the store flag with the §D.5 delta and clear the shared facade —
        // the journal's `resolved` edge rides that write. The former local
        // facade clear was its redundant twin (a second engine cmd writing
        // a second `resolved` row).
        if matches!(choice, PlanReviewChoice::Refine) {
            // The refine verdict must reach the server: without the Reply
            // the PlanVerdict delivery hangs until the 300s expire, whose
            // GW9 convergence CANCELS the parked turn — the very turn
            // refine keeps alive for the feedback round. The server's
            // refine arm consumes the review flag and clears the badge.
            self.reply_plan_verdict_choice(&review.plan_file, "refine", cx);
            // Keep plan mode ON: demote the card and prompt for feedback.
            // The feedback turn runs under the plan-mode instructions; the
            // model updates the plan file and proposes again (fresh card).
            self.conversation
                .update(cx, |c, cx| c.consume_plan_review(cx));
            self.list_state.remeasure();
            self.add_info_message(
                i18n::t("plan-refine-notice").to_string(),
                NoticeAnchor::TurnEnd,
                None,
                cx,
            );
            cx.notify();
            return;
        }
        let lang = manox_agent::settings::load().resolve().agent;
        let seed_text = match manox_agent::collaboration_mode::render_plan_mode_approved(
            lang,
            &review.plan_file,
        ) {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(error = %err, "failed to render plan execution seed");
                format!(
                    "Read and implement the approved plan at {}",
                    review.plan_file
                )
            }
        };
        let mut meta = self.user_turn_meta(cx);
        // The expanded plan directive is written by the harness on the user's
        // behalf, so the bubble header names the harness, not the human and
        // not the agent that receives it.
        meta.author = Some(manox_agent::MessageAuthor::Harness);
        let ui = Self::message_ui_metadata(&meta);
        if matches!(choice, PlanReviewChoice::ExecuteFresh) {
            // This thread's plan is abandoned (the fresh thread executes
            // it): withdraw the verdict delivery so the server converges
            // at once instead of hanging to the 300s expire and rejecting
            // against the archived thread.
            self.cancel_plan_verdict_delivery(&review.plan_file, cx);
            // Fresh context: archive this thread and continue on a new one
            // seeded with the execution directive. The plan file persists on
            // disk, so the new thread reads it from the path — no inline
            // plan-text copy in the transcript.
            let old_id = self
                .store
                .as_ref()
                .map(|s| s.read(cx).store.id.0.clone())
                .expect("foreground store present");
            let cwd = self
                .store
                .as_ref()
                .map(|s| std::path::PathBuf::from(s.read(cx).store.cwd.clone()))
                .expect("foreground store present");
            let project = self.store.as_ref().and_then(|s| {
                s.read(cx)
                    .store
                    .project
                    .clone()
                    .map(std::path::PathBuf::from)
            });
            let model = self
                .foreground_model_identity(cx)
                .and_then(|(provider, id)| Self::resolve_model_identity(&provider, &id));
            let effort = self
                .store
                .as_ref()
                .map(|s| s.read(cx).store.reasoning_effort)
                .expect("foreground store present");
            let permission = self
                .store
                .as_ref()
                .map(|s| s.read(cx).store.permission_mode)
                .expect("foreground store present");
            // U6b③: the fresh thread rides the v2 CreateSession intent
            // (the inherited fields actually reach the server), and the
            // seed turn rides the PlanSeedExecution note: the server
            // renders the seed text (the same render_plan_mode_approved),
            // inserts it under the Harness author, and runs the turn on
            // the session's own engine. The desktop-side `Thread::new_*`
            // + facade seed this replaces was the last in-process
            // execution path — its `ensure_engine` spawned a SECOND
            // engine racing the gateway's for the same session file.
            let cwd_str = cwd.to_string_lossy().to_string();
            let project_str = project.as_ref().map(|p| p.to_string_lossy().to_string());
            let model_str = model.map(|m| format!("{}/{}", m.provider, m.id));
            let approval = serde_json::to_value(permission)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string));
            let effort_str = match effort {
                manox_agent::language_model::ReasoningEffort::High => Some("high".to_string()),
                manox_agent::language_model::ReasoningEffort::Max => Some("max".to_string()),
            };
            let plan_file = review.plan_file.clone();
            let ws = cx.weak_entity();
            let dir_for_store = project.clone();
            self.multiplexer.update(cx, |m, _| {
                m.create_session_intent(
                    (project.is_none()).then(|| cwd_str.clone()),
                    project_str,
                    model_str,
                    approval,
                    effort_str,
                    Box::new(move |done, cx| {
                        let sid = match done {
                            crate::multiplexer::CreateSessionDone::Created {
                                session_id, ..
                            } => session_id,
                            crate::multiplexer::CreateSessionDone::Failed { message } => {
                                tracing::warn!(
                                    error = %message,
                                    "ExecuteFresh create intent failed"
                                );
                                return;
                            }
                        };
                        let plan_file = plan_file.clone();
                        let old_id = old_id.clone();
                        cx.spawn(async move |_, cx| {
                            if let Some(dir) = &dir_for_store {
                                let _ = ws.update(cx, |_ws, cx| {
                                    Workspace::register_project_in_store(dir, cx);
                                });
                            }
                            let _ = ws.update_in(cx, |this, window, cx| {
                                this.attach_created_session(&sid, window, cx);
                                // The archive lands BEFORE the refetch
                                // (cross-domain #5: the answer's self-held
                                // rescan must see it).
                                manox_agent::thread_store_global()
                                    .with_mut(|s| s.archive_thread(&old_id, true));
                                this.multiplexer.update(cx, |m, _| m.fetch_thread_list());
                                // The seed turn: the server renders +
                                // inserts + runs it, FIFO behind the
                                // attach's OpenSession on this connection.
                                let _ = this.send_note(|s| {
                                    manox_protocol::ClientNote::PlanSeedExecution {
                                        session_id: s.to_string(),
                                        plan_file,
                                    }
                                });
                            });
                        })
                        .detach();
                    }),
                );
            });
        } else {
            // Compact/keep-context: the engine exits plan mode, optionally
            // compacts the planning context toward the plan file, then runs
            // the seed turn. Retire the card and push the verdict bubble so
            // live and rebuilt views match.
            let weak = cx.weak_entity();
            self.conversation.update(cx, |c, cx| {
                c.pop_plan_review_tail(cx);
            });
            self.conversation.update(cx, |c, cx| {
                c.push_user(seed_text.clone(), Vec::new(), meta, weak, cx);
            });
            self.sync_list_count(cx);
            let ix = self.conversation.read(cx).items().len().saturating_sub(1);
            self.list_state.remeasure_items(ix..ix + 1);
            self.list_state.set_follow_mode(FollowMode::Tail);
            let compact = matches!(choice, PlanReviewChoice::ExecuteCompact);
            let compact_instructions = compact.then(|| {
                manox_agent::collaboration_mode::plan_compact_instructions(lang, &review.plan_file)
            });
            let choice_str = match choice {
                PlanReviewChoice::ExecuteCompact => "execute_compact",
                PlanReviewChoice::ExecuteKeep => "execute_keep",
                _ => "execute_keep",
            };
            if !self.reply_plan_verdict_choice(&review.plan_file, choice_str, cx) {
                self.thread.with_mut(|thread| {
                    thread.approve_plan(compact, compact_instructions, seed_text, Some(ui));
                });
            }
        }
        cx.notify();
    }
}
