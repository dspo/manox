//! The workspace attach lifecycle (U9b cluster 1): attach/park/reclaim,
//! the create-intent launch paths, and the archive gestures. Split from
//! `workspace.rs` — `super` is the workspace module, so the parent's
//! imports and `Workspace`'s private fields resolve unchanged; the methods
//! are `pub(super)` so the parent module (and its `tests` child) keep
//! calling them.

use super::*;

impl Workspace {
    /// Minimal subscription for a thread parked in `background_threads`. Unlike
    /// `subscribe_thread`, this only coordinates running state and the parked
    /// follow-up stash. It never touches `conversation` or `self.thread`, so a
    /// background thread's streaming deltas and tool events cannot be
    /// misrouted into the foreground view.
    /// The parked entry is left in `background_threads` (reclaimed on a later
    /// `open_thread`); self-removal from within the callback would drop the very
    /// subscription running it.
    pub(super) fn subscribe_background_thread(
        &self,
        store: &gpui::Entity<ClientStoreHandle>,
        id: String,
        cx: &mut Context<Self>,
    ) -> Subscription {
        let store = store.clone();
        cx.subscribe(&store, move |this, _store, ev: &ThreadEvent, cx| match ev {
            // U3a+U3b: the parked badges are the server pump's store
            // writes + §D.5 deltas (single writer), and the pending-auth
            // CLEAR is the server's verdict-time clear (U3b) — the
            // tool-traffic heuristics this replaces only ran in-proc and
            // raced the actual verdict. Tool traffic falls to the
            // catch-all.
            ThreadEvent::SteerInjected { message_id } => {
                this.consume_background_steer(&id, message_id);
            }
            ThreadEvent::PlanReady { plan_file, title } => {
                // A plan proposed while the thread was parked never reaches
                // the foreground handler: stash the review so the
                // switch-back restore (`attach_thread` drains `pending_plans`)
                // re-surfaces the verdict card, and persist the sidecar flag
                // so a restart re-emits PlanReady on Ready. The sidebar
                // keeps the blue-static wait until the verdict lands.
                let content = std::fs::read_to_string(plan_file).unwrap_or_default();
                this.pending_plans.insert(
                    id.clone(),
                    PendingPlanReview {
                        plan_file: plan_file.clone(),
                        title: title.clone(),
                        content,
                    },
                );
                // U3a: the pending-plan badge rides the pump's PlanReady
                // store write + delta (single writer).
            }
            ThreadEvent::TurnFinished {
                cancelled,
                failed,
                stranded_steer_ids,
                ..
            } => {
                // A cancelled/failed turn voids a stashed plan review
                // (mirroring the foreground demote); a normal settle keeps
                // it so the switch-back restore re-surfaces the card. The
                // pending-plan badge stays up while a review is stashed —
                // the card is not visible until the user switches back.
                if *cancelled || *failed {
                    this.pending_plans.remove(&id);
                }
                // U3a: the settle flags are the pump's store writes (single
                // writer); the stashed-plan nuance was already overridden by
                // the pump's unconditional clear in-proc — the stash itself
                // (UI state) is untouched.
                // GW5: the parked settle's unread rise rides the leaf
                // mirror (the client-owned badge source), not the
                // server-side store mirror.
                this.multiplexer.update(cx, |m, cx| m.note_unread(&id, cx));
                // Mirror the foreground terminal bookkeeping: steers the
                // aborted turn never drained flip to `Failed` in the
                // parked stash (a late `SteerInjected` can still heal
                // them), and queued follow-ups only become the next turn
                // on a natural settle — never after a cancel.
                this.mark_parked_stranded_steers_failed(&id, stranded_steer_ids);
                if !*cancelled {
                    this.flush_parked_follow_ups(&id, cx);
                }
            }
            ThreadEvent::Error(_) => {
                // An errored run voids a stashed plan review (the verdict
                // is moot once the loop bailed out).
                this.pending_plans.remove(&id);
                // U3b: the idle + full badge clear is the server Error
                // arm's store write + the single delta carrying the whole
                // set (the same five flags this mirror block wrote).
                // GW5: the parked error's unread rise rides the leaf mirror
                // (the server's Error delta carries no unread flag).
                this.multiplexer.update(cx, |m, cx| m.note_unread(&id, cx));
            }
            ThreadEvent::BackgroundTaskUpdated { .. } => {
                // U3a: the background-work flag is the pump's store write +
                // delta (single writer; the pump computes the same
                // thread_has_running_tasks outside the store lock).
                // GW5: a parked background-task update lights the badge
                // through the leaf mirror, not the server-side store.
                this.multiplexer.update(cx, |m, cx| m.note_unread(&id, cx));
            }
            _ => {}
        })
    }

    /// New thread for the pi harness: no provider reload (registration is
    /// one-shot; actors await readiness themselves). A project binds at
    /// construction time — a single session creation, no orphaned session
    /// file.
    ///
    /// T6: the double path is deleted. The workspace no longer pre-builds a
    /// local `Thread` and then attaches the AgentServer session to the same
    /// id (two `Thread::new_*` calls on one id, both racing to materialize the
    /// session file). It now sends the §D.2 `CreateSession` intent (the full
    /// `{cwd, project?, initial_model?}` so the server owns creation) and,
    /// when the server's `{session_id}` receipt lands, binds a detached
    /// rendering mirror to that server-minted id and opens the follow stream.
    /// Creation happens exactly once — on the server.
    pub(super) fn start_new_thread(
        &mut self,
        project: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The park/draft bookkeeping is identical to `attach_thread`'s switch
        // arm, but there is no outgoing-thread double-bind to preserve: the
        // outgoing thread is detached on the park, the incoming id is unknown
        // until the receipt.
        //
        // Inheritance (L8/L9): the foreground session's wire identity is the
        // projection store — `model` key carries `{provider, modelId}` and
        // `project` the bound folder. The legacy thread mirror is only a
        // pre-snapshot fallback (the #765 round-2 repro: reading the mirror
        // alone lost model AND project on every new thread).
        let inherited_project = project.or_else(|| {
            self.store.as_ref().and_then(|s| {
                s.read(cx)
                    .store
                    .with(|st| st.project.clone())
                    .filter(|p| !p.is_empty())
                    .map(PathBuf::from)
            })
        });
        let project = inherited_project;
        let model = self
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.with(|st| st.model.clone()))
            .and_then(|m| {
                let provider = m.get("provider").and_then(|v| v.as_str())?;
                let id = m.get("modelId").and_then(|v| v.as_str())?;
                (!provider.is_empty() && !id.is_empty()).then(|| format!("{provider}/{id}"))
            })
            .or_else(|| {
                self.thread
                    .read(|t| t.model().map(|m| format!("{}/{}", m.provider, m.id)))
            });
        let approval = serde_json::to_value(self.store.as_ref().map_or_else(
            || self.thread.read(|t| t.permission_mode()),
            |s| s.read(cx).store.with(|st| st.permission_mode),
        ))
        .ok()
        .and_then(|v| v.as_str().map(str::to_string));
        let effort = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.with(|st| st.reasoning_effort))
            .unwrap_or_else(|| self.thread.read(|t| t.reasoning_effort()));
        let effort_str = match effort {
            manox_agent::language_model::ReasoningEffort::High => Some("high".to_string()),
            manox_agent::language_model::ReasoningEffort::Max => Some("max".to_string()),
        };
        let cwd = project
            .as_ref()
            .unwrap_or(&self.cwd)
            .to_string_lossy()
            .to_string();
        let project_str = project.as_ref().map(|p| p.to_string_lossy().to_string());
        let ws = cx.weak_entity();
        let dir_for_store = project.clone();
        self.multiplexer.update(cx, |m, _| {
            m.create_session_intent(
                (project.is_none()).then(|| cwd.clone()),
                project_str.clone(),
                model,
                approval,
                effort_str,
                Box::new(move |done, cx| {
                    let sid = match done {
                        crate::multiplexer::CreateSessionDone::Created { session_id, .. } => {
                            tracing::info!(session_id = %session_id, "new-thread intent created");
                            session_id
                        }
                        crate::multiplexer::CreateSessionDone::Failed { message } => {
                            tracing::warn!(error = %message, "CreateSession intent failed");
                            return;
                        }
                    };
                    // Defer the workspace bind out of the multiplexer's
                    // update borrow: the intent callback runs inside the mux
                    // pump, and `attach_created_session` re-enters the mux
                    // (`open_or_create`), so it must land on a later tick.
                    cx.spawn(async move |_, cx| {
                        if let Some(dir) = &dir_for_store {
                            let _ = ws.update(cx, |_ws, cx| {
                                Workspace::register_project_in_store(dir, cx);
                            });
                        }
                        match ws.update_in(cx, |this, window, cx| {
                            this.attach_created_session(&sid, window, cx);
                        }) {
                            Ok(()) => {
                                tracing::info!(session_id = %sid, "new-thread attach landed")
                            }
                            Err(err) => {
                                tracing::warn!(session_id = %sid, error = %err, "new-thread attach FAILED")
                            }
                        }
                    })
                    .detach();
                }),
            );
        });
        let _ = window;
    }

    /// Attach the workspace to a freshly created session (whose id the server
    /// minted): reuse the standard switch bookkeeping (`attach_thread` parks
    /// the outgoing thread, restores drafts/queues) against a thread handle
    /// for the new id, then re-open — `open_or_create(reopen = true)`
    /// idempotently re-runs `OpenSession` and binds the follow stream that
    /// the create intent already opened. U6b②: the handle is ALWAYS the
    /// detached *rendering mirror* (a landing thread, no engine — the
    /// server owns the transcript, so no second actor ever races the
    /// session file); the former live_thread/load_thread store reads were
    /// the U6 dual source and are gone.
    pub(super) fn attach_created_session(
        &mut self,
        session_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread = Thread::landing_with_id(ThreadId(session_id.to_string()), self.cwd.clone());
        self.attach_thread(thread, true, window, cx);
    }

    /// Switch to a new thread: persist the current one, build/load the new
    /// one, re-subscribe, and rebuild the conversation view. Each thread gets
    /// its own AgentServer session (`reopen` = `OpenSession` on an existing
    /// thread, else `CreateSession` on a fresh one); parking a running thread
    /// keeps its store/connection so the session stays alive and is never
    /// cancelled.
    pub(super) fn attach_thread(
        &mut self,
        new_thread: manox_agent::thread::ThreadHandle,
        reopen: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_turn_navigator(window, cx);
        let old_thread = self.thread.clone();
        let old_id = old_thread.read(|t| t.id.0.clone());
        let new_id = new_thread.read(|t| t.id.0.clone());

        // Sub-agent observation is per-thread ephemeral state; drop the
        // outgoing thread's panels and transcripts before rebinding.
        self.clear_subagent_observation(cx);
        // The right pane belongs to a thread: stash the outgoing pane (live
        // tabs, active index, visibility) to the in-session map + threads.db
        // before the incoming thread rebinds. Subagent tabs were already
        // stripped above, so the stash never carries ephemeral entries.
        self.stash_right_pane(old_id.clone(), cx);

        // Save the outgoing thread's unsent composer text before switching, so
        // a draft survives a round-trip through another thread (Bug 1). A
        // thread that just submitted already cleared its input, storing "".
        // While the walk runs the composer holds borrowed history — a recall
        // step's turn or a navigator fill — so the user's draft is the walk's
        // working line, not what is on screen.
        let outgoing = match self.recall_draft.as_deref() {
            Some(draft) if self.recall_index >= 0 => draft.to_string(),
            _ => self.input_state.read(cx).value().to_string(),
        };
        self.drafts.insert(old_id.clone(), outgoing);
        self.end_recall_walk();
        // The editor pane is a right-side resource of the outgoing thread:
        // stash its text so a switch-back restores the draft (mirrors the
        // composer `drafts` stash above).
        self.editor_drafts.insert(
            old_id.clone(),
            self.editor_state.read(cx).value().to_string(),
        );

        // Stash the outgoing thread's pending plan verdict so it survives a
        // round-trip through another thread. The plan text lives only in
        // `pending_plan_review` (never in persisted messages), so switching
        // away would otherwise drop it — and the verdict card with it.
        if let Some(review) = self.pending_plan_review.take() {
            self.pending_plans.insert(old_id.clone(), review);
        }

        // Queue state is session-local but belongs to a thread, not to the
        // currently visible workspace. Move it aside before rebinding.
        let outgoing_follow_ups = std::mem::take(&mut self.queued_follow_ups);
        if outgoing_follow_ups.is_empty() {
            self.queued_follow_ups_by_thread.remove(&old_id);
        } else {
            self.queued_follow_ups_by_thread
                .insert(old_id.clone(), outgoing_follow_ups);
        }
        // Restore the incoming thread's stashed follow-up queue (moved aside
        // when it was last switched away); without this the queue silently
        // vanishes on switch-back.
        if let Some(queue) = self.queued_follow_ups_by_thread.remove(&new_id) {
            self.queued_follow_ups = queue;
        }

        // Park a still-running old thread in the background (U6b⑤: the
        // turn runs server-side and survives the switch on its own —
        // parking keeps the session ATTACHED so the reclaim is an in-place
        // re-attach with no reopen, and the parked leaf keeps receiving
        // the §D.5 deltas that drive the sidebar badges). The running
        // truth is the foreground LEAF's wire mirror: the facade
        // `is_running` this replaces is a dead flag on the detached
        // mirrors the attach path builds since U6b②. The store-flag seeds
        // retired with the U3b single-writer discipline — the server's
        // pump marked running/background_work when the turn/task started,
        // and its deltas already fed every mirror. An idle old thread's
        // session has no activity to preserve, so it detaches.
        let old_running = (self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .unwrap_or(false)
            || manox_agent::background_task::thread_has_running_tasks(&old_id))
            && old_id != new_id;
        if old_running {
            let old_store = self.store.take();
            let old_sid = self.session_id.take();
            let sub = self.subscribe_background_thread(
                old_store
                    .as_ref()
                    .expect("a parked running thread always has a store"),
                old_id.clone(),
                cx,
            );
            self.background_threads.push(BackgroundThread {
                id: old_id.clone(),
                store: old_store,
                session_id: old_sid,
                _sub: sub,
            });
        } else if old_id != new_id {
            // Idle switch: the outgoing session has nothing in flight. Detach
            // it from the shared connection so the server releases the owner
            // (the session itself survives for a later `OpenSession`), then
            // drop the local handle. No owner leak — the pre-multiplex path
            // leaked because the in-process transport never signalled drop.
            self.client
                .send_note(manox_protocol::ClientNote::DetachSession {
                    session_id: old_id.clone(),
                });
            self.multiplexer.update(cx, |m, _| {
                m.forget(&old_id);
            });
            self.store = None;
            self.session_id = None;
        }

        // If the new thread was previously parked in the background, reclaim it
        // (entity + store) so it becomes the foreground thread and is no longer
        // double-held. The parked session stayed attached to the shared
        // connection, so no `OpenSession` is needed on reclaim.
        if let Some(pos) = self.background_threads.iter().position(|b| b.id == new_id) {
            let bg = self.background_threads.remove(pos);
            self.store = bg.store;
            self.session_id = bg.session_id;
        }

        // A brand-new foreground thread gets its own session on the shared
        // connection: `reopen` binds an existing thread via `OpenSession`
        // (history replays as the follow-stream `Snapshot`), otherwise a
        // fresh one via `CreateSession`. The multiplexer registers the leaf
        // handle before sending so the server's reply routes straight to it.
        if self.store.is_none() {
            let new_sid = new_id.clone();
            let cwd = thread_cwd(&new_thread, &None, cx)
                .unwrap_or_default()
                .to_string();
            let store = self.multiplexer.update(cx, |m, cx| {
                let handle = m.open_or_create(&new_sid, &cwd, reopen, cx);
                // GW5: focus follows the attach — the new leaf goes active
                // (clearing its unread/errored mirrors), the old one inert.
                m.set_focused(Some(&new_sid), cx);
                handle
            });
            self.store = Some(store);
            self.session_id = Some(new_sid);
        }

        // Persist the old thread's current state before switching away. The
        // spawned-task save backstop in `run_turn` will persist again when the
        // turn actually finishes, capturing the final assistant messages.

        self.thread = new_thread;
        // The chips derive from the bound thread's authoritative mirror; the
        // previous thread's suites never bleed across the switch.
        self.active_browser_suites = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.browser_suites.clone())
            .expect("foreground store present");
        let id = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.id.0.clone())
            .expect("foreground store present");
        // Derive the transcript's plan and the sub-agent rows inside one
        // store read — the display fold's message rows ARE the messages
        // (L6, mechanical transcription), and cloning the whole transcript
        // here would copy it on the main thread on every switch.
        let (plan_from_messages, subagent_rows) = self
            .store
            .as_ref()
            .map(|s| {
                let msgs = s.read(cx).store.derived_messages();
                (
                    manox_agent::plan::rebuild_from_messages(&msgs),
                    manox_agent::subagent_restore::rebuild_from_messages(&msgs),
                )
            })
            .expect("foreground store present");
        let display: Vec<manox_agent::db::HistoryEntry> = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.display.clone())
            .expect("foreground store present");
        let usage = self
            .store
            .as_ref()
            .map(|s| {
                s.read(cx)
                    .store
                    .per_request_usage
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            manox_agent::TokenUsage {
                                input_tokens: v.input,
                                output_tokens: v.output,
                                cache_creation_input_tokens: v.cache_creation,
                                cache_read_input_tokens: v.cache_read,
                            },
                        )
                    })
                    .collect()
            })
            .expect("foreground store present");
        let background_tasks = self
            .store
            .as_ref()
            .map(|s| {
                s.read(cx)
                    .store
                    .background_tasks
                    .iter()
                    .filter_map(|t| {
                        serde_json::from_value::<manox_agent::background_task::TaskSnapshot>(
                            t.clone(),
                        )
                        .ok()
                    })
                    .collect::<Vec<_>>()
            })
            .expect("foreground store present");
        let role = self.model_label(cx);
        let recipient = self.recipient_author();
        let weak = cx.weak_entity();
        let running = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .expect("foreground store present");
        let cwd = thread_cwd(&self.thread, &self.store, cx);
        let new_conv = cx.new(|cx| {
            let mut conversation = ConversationState::rebuild_from_display(
                &display,
                &usage,
                &role,
                recipient,
                running,
                crate::conversation::ApplyCtx {
                    weak: weak.clone(),
                    cwd,
                },
                cx,
            );
            conversation.restore_background_tasks(&background_tasks, &role, weak.clone(), cx);
            conversation
        });
        self.conversation = new_conv;
        self.observe_conversation(cx);
        // Restore the incoming thread's saved draft, or clear the input if it
        // has none — without this the previous thread's text would bleed into
        // the new one (Bug 1). `set_value` is silent (no Change event), so
        // re-sync the slash menu by hand in case the draft begins with `/`.
        let saved = self.drafts.remove(&new_id).unwrap_or_default();
        self.input_state
            .update(cx, |s, cx| s.set_value(saved, window, cx));
        self.sync_completion(window, cx);
        // Restore the incoming thread's stashed editor draft, or clear the
        // pane so the previous thread's text never bleeds into this one.
        // `set_value` is silent (no Change event), so the editor's submit
        // binding is unaffected.
        let editor_saved = self.editor_drafts.remove(&new_id).unwrap_or_default();
        self.editor_state
            .update(cx, |s, cx| s.set_value(editor_saved, window, cx));
        // Restore the incoming thread's right pane: the in-session stash,
        // else the threads.db snapshot; a thread with neither gets the empty
        // hidden pane.
        self.restore_right_pane(&new_id, window, cx);
        // Reveal the latest turn for the new thread: `reset` drops the old
        // thread's measured heights and scroll position, then reveal the latest
        // turn once. Both running and completed threads arm `FollowMode::Tail`:
        // the list pins to the end on each layout while following, and GPUI
        // auto-disengages follow the moment the user scrolls up. Using
        // `Normal` here would leave a one-shot scroll that never re-pins if a
        // late delta or notice lands after the user scrolls.
        let count = self.conversation.read(cx).items().len();
        self.list_state.reset(count);
        self.list_count = count;
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.pending_ask = None;
        self.pending_auth = None;
        // Restore the incoming thread's stashed pending plan, if any.
        self.pending_plan_review = self.pending_plans.remove(&new_id);
        if let Some(review) = self.pending_plan_review.as_ref() {
            let title = review.title.clone();
            let content = review.content.clone();
            let role = self.model_label(cx);
            let weak = cx.weak_entity();
            self.conversation.update(cx, |c, cx| {
                c.push_plan_review(title, content, role, weak, cx);
            });
            // The card is appended after the earlier `reset`, so splice the
            // list count and re-pin so the restored drawer is in view.
            self.sync_list_count(cx);
            self.list_state.set_follow_mode(FollowMode::Tail);
        }
        let (thread_events, store_changes) = self.subscribe_thread(cx);
        self.thread_sub = Some(thread_events);
        self.store_observe = Some(store_changes);
        // The thinking ticker belongs to the outgoing thread: bump its
        // generation so the old ticker self-terminates, then mirror the incoming
        // thread's running state. A parked thread resumed mid-turn keeps the
        // "for Xs" counter live; a completed history thread is idle.
        self.turn_active = running;
        if running {
            self.spawn_thinking_ticker(cx);
        }
        // Cockpit state is per-thread: the outgoing thread's plan,
        // running-tool title, and per-model counter state do not apply to the
        // incoming one. The execution plan, unlike the proposed-plan review,
        // IS recoverable: re-derived from the transcript's `UpdatePlan` tool
        // calls, falling back to the independent sidecar snapshot (the facade
        // mirrors the persisted copy on every `PlanUpdated` / `Ready`).
        let restored_plan = plan_from_messages.or_else(|| {
            self.store
                .as_ref()
                .and_then(|s| s.read(cx).store.persisted_plan.as_ref())
                .and_then(|v| {
                    serde_json::from_value::<manox_agent::plan::PlanSnapshot>(v.clone()).ok()
                })
        });
        let rail_leaf = self.store.clone();
        self.context_rail.update(cx, |r, cx| {
            // Rail-freeze fix (the visual-acceptance report): the store is
            // the rail's only read face (U7b), and the SessionStatus deltas
            // and info-fetch responses feeding it only reach the ATTACHED
            // session's leaf — re-bind to the incoming leaf (already swapped
            // on `self.store` above) so the status row and the usage
            // sections track the live thread instead of the
            // construction-time one.
            r.bind_store(rail_leaf, cx);
            // U7b: the rail's reads are store-only; the switch reset below
            // clears the per-thread cockpit state.
            r.reset_for_thread_switch(running, cx);
            if let Some(snapshot) = restored_plan {
                r.set_plan(snapshot, cx);
            }
            cx.notify();
        });
        // Recover the settled sub-agent observation rows for the incoming
        // thread after the rail reset above (a restoring thread has no
        // messages yet — its rows land with the `HistoryRestored` rebuild).
        self.apply_subagent_rows(subagent_rows, cx);
        // If the new thread has pending interactions (e.g. it was parked
        // waiting for a user answer), re-surface them so the question card
        // appears immediately upon switching back.
        self.resurface_pending_auths(cx);
        self.sidebar
            .update(cx, |s, cx| s.set_selected(Some(id.clone()), cx));
        // The user is now viewing this thread: clear any unread red dot it
        // carried from a prior background completion, and any pending-auth
        // badge (the verdict card re-surfaced below if one is outstanding).
        let store = manox_agent::thread_store_global();
        store.with_mut(|s| {
            s.set_unread(&id, false);
            s.mark_pending_auth(&id, false);
        });
        // The incoming thread's cwd / worktree may differ from the outgoing
        // one; refresh the rail's git stats/branch display for it.
        self.spawn_git_status_refresh(cx);
        // Returning to a thread leaves the external-session view: without this
        // the render still takes the ExternalSession branch and the swapped-in
        // thread is invisible behind the terminal TUI.
        self.view_mode = ViewMode::Workspace;
        self.active_external = None;
        cx.notify();
    }

    /// Archive the active thread and navigate to a fresh empty one. Shared
    /// by the `/exit` slash command and the `cmd-;` keybinding. No-op while
    /// a turn is running (a parked thread would strand pending verdicts).
    pub(crate) fn archive_current_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.archive_active_thread_if_idle(cx) {
            return;
        }
        self.start_new_thread(None, window, cx);
    }

    /// Archive the active thread if it is idle; `false` when a turn is
    /// running (attaching would park the thread and clear `pending_ask`,
    /// stranding a user verdict). Marks the thread archived and persists via
    /// the store, which refreshes the sidebar list.
    pub(super) fn archive_active_thread_if_idle(&mut self, cx: &mut Context<Self>) -> bool {
        if self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .expect("foreground store present")
        {
            return false;
        }
        let id = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.id.0.clone())
            .expect("foreground store present");
        let _ = self.send_note(|sid| manox_protocol::ClientNote::ArchiveThread {
            session_id: sid.into(),
            archived: true,
        });
        let store = manox_agent::thread_store_global();
        store.with_mut(|s| s.archive_thread(&id, true));
        true
    }

    /// Archive the active thread and open a fresh one that inherits the
    /// outgoing thread's project, model, permission mode, and reasoning effort —
    /// `/new` starts a clean conversation without dropping the working
    /// context. No-op while a turn is running (see
    /// `archive_active_thread_if_idle`).
    pub(crate) fn archive_current_thread_inheriting(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.archive_active_thread_if_idle(cx) {
            return;
        }
        let old = self.thread.clone();
        let cwd = self
            .store
            .as_ref()
            .map(|s| std::path::PathBuf::from(s.read(cx).store.cwd.clone()))
            .unwrap_or_else(|| old.read(|t| t.cwd().to_path_buf()));
        let project = self
            .store
            .as_ref()
            .and_then(|s| {
                s.read(cx)
                    .store
                    .project
                    .clone()
                    .map(std::path::PathBuf::from)
            })
            .or_else(|| old.read(|t| t.project().cloned()));
        let model = old.read(|t| t.model().cloned());
        let effort = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.reasoning_effort)
            .unwrap_or_else(|| old.read(|t| t.reasoning_effort()));
        let permission = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.permission_mode)
            .unwrap_or_else(|| old.read(|t| t.permission_mode()));
        // U6b③: the inherited state rides the v2 CreateSession intent —
        // the compat-note create this replaces carried only the cwd, so
        // the parked model/project/effort/permission never reached the
        // server (the #765 round-2 inheritance defect's shape); the
        // receipt's minted id attaches through the standard
        // created-session path.
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
                        crate::multiplexer::CreateSessionDone::Created { session_id, .. } => {
                            session_id
                        }
                        crate::multiplexer::CreateSessionDone::Failed { message } => {
                            tracing::warn!(error = %message, "inheriting create intent failed");
                            return;
                        }
                    };
                    // Defer the workspace bind out of the multiplexer's
                    // update borrow: the attach re-enters the mux
                    // (`open_or_create`), so it must land on a later tick.
                    cx.spawn(async move |_, cx| {
                        if let Some(dir) = &dir_for_store {
                            let _ = ws.update(cx, |_ws, cx| {
                                Workspace::register_project_in_store(dir, cx);
                            });
                        }
                        let _ = ws.update_in(cx, |this, window, cx| {
                            this.attach_created_session(&sid, window, cx);
                        });
                    })
                    .detach();
                }),
            );
        });
        let _ = window;
    }

    /// Park the active thread into the background (preserving its run + event
    /// subscriptions) and open a fresh empty thread in the same project — the
    /// explicit "background this task, switch to a new one" gesture, bound to
    /// `ctrl-b`. No-op when the active thread is idle so the shortcut can't
    /// spam empty threads. `attach_thread` does the actual parking: a running
    /// thread is moved into `background_threads` with a terminal-`Stop`/`Error`
    /// subscription that flips its sidebar row to idle + unread when it lands.
    pub(super) fn background_current_thread(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .expect("foreground store present")
        {
            return;
        }
        let project = self.store.as_ref().and_then(|s| {
            s.read(cx)
                .store
                .project
                .clone()
                .map(std::path::PathBuf::from)
        });
        self.start_new_thread(project, window, cx);
    }

    pub(super) fn open_thread(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        // If the thread is already running in the background, reclaim it
        // instead of loading a stale snapshot from the db.
        // U6b⑤: the reclaim re-attaches a fresh landing mirror for the
        // parked id — `attach_thread`'s own reclaim branch finds the park
        // by id and restores its leaf/session, so no reopen is needed
        // (the parked session never detached).
        if self.background_threads.iter().any(|b| b.id == id) {
            let thread = Thread::landing_with_id(ThreadId(id), self.cwd.clone());
            self.attach_thread(thread, true, window, cx);
            return;
        }
        // U6b②: the attach read is the landing mirror — the SERVER owns
        // the session and the restore rides the wire reopen flow
        // (OpenSession + the follow stream's Snapshot + the §D.5 mirrors),
        // exactly like the create path. The kernel-side `load_thread` (the
        // U6 dual source: a db-restored facade racing the live wire state)
        // is gone; the row this click came from is itself a wire item, so
        // the id is server-known by construction.
        let thread = Thread::landing_with_id(ThreadId(id), self.cwd.clone());
        self.attach_thread(thread, true, window, cx);
    }
}
