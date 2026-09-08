//! The external-view session family (U9b cluster 4): embedded terminal
//! tabs, CLI/ChatGPT/VSCode external sessions (spawn/resume/watch/
//! attach/place/mount/title/close/remove), and browser tabs (open/
//! title-ticker/visibility/close). Split from `workspace.rs` — `super`
//! is the workspace module; the bare-private methods are lifted
//! `pub(super)` so the parent (render handlers, sidebar subscriptions)
//! and its `tests` child keep calling them.

use super::*;

impl Workspace {
    /// Open a fresh terminal tab (cmd-t). If one already exists it is reused
    /// rather than replaced, so an in-flight session isn't killed.
    pub fn open_terminal_tab(&mut self, cx: &mut Context<Self>) {
        self.focus_terminal(cx);
    }

    /// Close the terminal tab and return to the conversation pane. Dropping
    /// the `TerminalView` drops the underlying `Terminal`, whose `PtySource`
    /// kills the child and detaches the reader/waiter threads.
    pub fn close_terminal_tab(&mut self, cx: &mut Context<Self>) {
        self.terminal_view = None;
        self.focus_conversation(cx);
    }

    /// Launch a new external agent CLI session (`claude` / `codex` / `copilot`)
    /// with a user-picked provider + model (from the sidebar `+` wizard
    /// cascade). The shared `SessionHandle` backs a `CxSessionSource` PTY
    /// source that drives a `Terminal`/`TerminalView`, and is also held by the
    /// `ExternalSession` so the close path can `kill` the agent explicitly. A
    /// spawn failure (binary missing / apikey parse / unsupported combo) pushes
    /// an error notification and leaves the sidebar untouched.
    ///
    /// `project_cwd` is `Some(path)` when launched from a project folder's `+`
    /// button — the CLI runs in that project's directory. `None` uses the
    /// workspace's default cwd.
    pub(crate) fn spawn_external_session(
        &mut self,
        spawn: ExternalSpawn,
        placement: SessionPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ExternalSpawn {
            kind,
            provider_name,
            model_id,
            wire_api,
            project_cwd,
        } = spawn;
        let agent_id = kind.agent_id();
        let Some(agent) = kind.agent() else {
            tracing::warn!(
                agent_id,
                "spawn_external_session called with a plain PTY kind"
            );
            return;
        };
        // The agent process runs in the project directory when spawned from a
        // project folder's `+` button, else the workspace cwd. cx's
        // AgentBuilder.cwd forwards to both the PTY-relay and direct spawn
        // paths; without it the agent inherits manox's own cwd (the user's
        // home dir), so a project-scoped session would operate on the wrong
        // tree.
        let cwd = project_cwd.clone().unwrap_or_else(|| self.cwd.clone());
        // claude's conversation id is assigned here (up front) so the sidecar
        // targets it on resume without racing any on-disk discovery; codex /
        // copilot name their own sessions (codex is captured by the watcher
        // while live).
        let spawn_cli_session_id =
            (kind == SessionKind::ClaudeCode).then(|| uuid::Uuid::new_v4().to_string());
        let mut spawn_args: Vec<String> = Vec::new();
        if let Some(sid) = &spawn_cli_session_id {
            spawn_args.push("--session-id".into());
            spawn_args.push(sid.clone());
        }
        let mut builder = manox_ext_agents::AgentBuilder::new()
            .agent(agent)
            .pty(true)
            .provider(provider_name.clone())
            .model(model_id.clone())
            .cwd(cwd.clone())
            .passthrough(spawn_args);
        if let Some(w) = &wire_api {
            builder = builder.wire_api(w.clone());
        }
        let handle = match builder.spawn() {
            Ok(h) => Arc::new(h),
            Err(e) => {
                tracing::error!(error = %e, agent = agent_id, "external agent spawn failed");
                window.push_notification(
                    Notification::error(format!(
                        "{}: {e}",
                        i18n::t("external-session-start-failed")
                    )),
                    cx,
                );
                return;
            }
        };
        let id = format!("external:{}:{}", agent_id, uuid::Uuid::new_v4());
        let source = manox_terminal::cx_session::CxSessionSource::new(Arc::clone(&handle));
        let terminal = match Terminal::spawn(id.clone(), cwd.clone(), 80, 24, Box::new(source)) {
            Ok(t) => cx.new(|cx| TerminalProxy::new(t, cx)),
            Err(e) => {
                tracing::error!(error = %e, "failed to create terminal for external session");
                window.push_notification(
                    Notification::error(format!(
                        "{}: {e}",
                        i18n::t("external-session-start-failed")
                    )),
                    cx,
                );
                return;
            }
        };
        // Tear the session down when the CLI exits on its own (e.g. `/exit`),
        // without waiting for the user to click ×, and mirror the agent's OSC
        // title into the sidebar row + titlebar as it changes.
        let exit_sub = self.subscribe_session_terminal(&terminal, &id, cx);
        // The cx session id (and its socket path) are the traceable identity for
        // `~/.manox/sessions/<id>.sock`, surfaced in the sidebar tag +
        // clipboard copy. cx does not yet expose `SessionHandle::session_id()`,
        // so the id is recovered from the `<id>.sock` filename.
        let socket_path = handle.socket_path().map(std::path::Path::to_path_buf);
        let cx_session_id = handle
            .socket_path()
            .and_then(crate::external_session::cx_session_id_from_socket)
            .unwrap_or_default();
        let view = TerminalView::new(terminal, cx);
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Thread-bound (right-pane tab) sessions are resources of their
        // thread: no sidebar row and no sidecar, so a restart drops them with
        // the tab instead of resurfacing them as top-level resumable rows.
        let thread_bound = matches!(placement, SessionPlacement::RightPane { .. });
        // Persist the session as unclosed from the first frame: the sidecar
        // survives this process (crash or quit), is kept in sync while live,
        // and is deleted only on an explicit close — so the next launch offers
        // exactly the sessions the user never closed.
        let sidecar = (!thread_bound).then(|| ResumeSidecar {
            id: id.clone(),
            agent_id: agent_id.into(),
            cwd: cwd.to_string_lossy().into_owned(),
            project: project_cwd.clone(),
            created_at,
            provider: provider_name,
            model: model_id,
            wire_api: wire_api.clone(),
            title: None,
            cli_session_id: spawn_cli_session_id,
        });
        let watch_cli_session_id = sidecar.as_ref().and_then(|s| s.cli_session_id.clone());
        if let Some(sidecar) = &sidecar
            && let Err(e) = write_sidecar(sidecar)
        {
            tracing::warn!(error = %e, id, "external session sidecar write failed");
        }
        self.external_sessions.push(ExternalSession {
            id: id.clone(),
            kind,
            created_at,
            project: project_cwd,
            title: None,
            cx_session_id,
            socket_path,
            terminal_view: view,
            handle: Some(handle),
            _exit_sub: exit_sub,
            thread_bound,
            sidecar,
        });
        // The watcher's only job is capturing the CLI session id into the
        // sidecar; thread-bound sessions carry none, and starting it would
        // only claim ledger files a top-level watcher in the same cwd needs
        // to record its own sidecar's resume target.
        if !thread_bound {
            self.start_cli_session_watch(&id, kind, &cwd, watch_cli_session_id, cx);
        }
        self.sync_sidebar_external(cx);
        self.place_external_session(&id, placement, window, cx);
    }

    /// Launch a plain PTY session — the user's shell (`Terminal`) — with no
    /// cx provider/model injection. Mirrors the `spawn_external_session` flow
    /// (sidebar row, ChildExit teardown, OSC title mirroring, project
    /// grouping), but the PTY is a local `PtyHandle` and
    /// `ExternalSession.handle` stays `None` — closing drops the view, whose
    /// PTY teardown kills the child tree.
    ///
    /// `project_cwd` is `Some(path)` when launched from a project folder's
    /// `+` button — the session runs in that project's directory. `None`
    /// uses the workspace's default cwd.
    pub fn spawn_plain_session(
        &mut self,
        kind: SessionKind,
        project_cwd: Option<PathBuf>,
        placement: SessionPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = project_cwd.clone().unwrap_or_else(|| self.cwd.clone());
        let source: Box<dyn manox_terminal::pty_source::PtySource> = match kind {
            SessionKind::Terminal => match manox_terminal::pty::default_source(&cwd, 80, 24) {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(error = ?e, "failed to open terminal pty");
                    window.push_notification(
                        Notification::error(format!(
                            "{}: {e}",
                            i18n::t("plain-session-start-failed")
                        )),
                        cx,
                    );
                    return;
                }
            },
            _ => {
                tracing::warn!(
                    agent_id = kind.agent_id(),
                    "spawn_plain_session called with an agent kind"
                );
                return;
            }
        };
        let id = format!("external:{}:{}", kind.agent_id(), uuid::Uuid::new_v4());
        let terminal = match Terminal::spawn(id.clone(), cwd, 80, 24, source) {
            Ok(t) => cx.new(|cx| TerminalProxy::new(t, cx)),
            Err(e) => {
                tracing::error!(error = %e, "failed to create terminal for plain session");
                window.push_notification(
                    Notification::error(format!("{}: {e}", i18n::t("plain-session-start-failed"))),
                    cx,
                );
                return;
            }
        };
        let exit_sub = self.subscribe_session_terminal(&terminal, &id, cx);
        let view = TerminalView::new(terminal, cx);
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.external_sessions.push(ExternalSession {
            id: id.clone(),
            kind,
            created_at,
            project: project_cwd,
            title: None,
            cx_session_id: String::new(),
            socket_path: None,
            terminal_view: view,
            handle: None,
            _exit_sub: exit_sub,
            thread_bound: matches!(placement, SessionPlacement::RightPane { .. }),
            sidecar: None,
        });
        self.sync_sidebar_external(cx);
        self.place_external_session(&id, placement, window, cx);
    }

    /// Observe a session terminal's lifecycle, shared by the agent and plain
    /// spawn paths: `ChildExit` tears the session down on a natural exit
    /// (e.g. `/exit` or `exit`), and `Title` mirrors the TUI's OSC title into
    /// the sidebar row + titlebar as it changes. The subscription lives on
    /// the session so a later close detaches it before any spurious event.
    pub(super) fn subscribe_session_terminal(
        &self,
        terminal: &Entity<TerminalProxy>,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Subscription {
        let exit_id = id.to_string();
        cx.subscribe(
            terminal,
            move |this, _terminal, ev: &manox_terminal::event::TerminalEvent, cx| match ev {
                manox_terminal::event::TerminalEvent::ChildExit(_) => {
                    this.remove_external_session(&exit_id, cx);
                }
                manox_terminal::event::TerminalEvent::Title(title) => {
                    this.set_external_title(&exit_id, title.clone(), cx);
                }
                _ => {}
            },
        )
    }

    /// Launch ChatGPT.app through cx's injection path with the provider + model
    /// picked in the macOS `工具 → ChatGPT.app` menu cascade. The launch blocks
    /// (config load, model catalog build, CDP injection — up to ~20s), so it runs
    /// on a background thread and reports the outcome as a notification; the app
    /// itself detaches and keeps running independently of manox.
    pub fn launch_chatgpt_app(
        &mut self,
        provider: String,
        model: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let launch_provider = provider.clone();
            let launch_model = model.clone();
            let result = cx
                .background_spawn(async move {
                    manox_ext_agents::launch_chatgpt_app(&launch_provider, &launch_model)
                })
                .await;
            let _ = this.update_in(cx, |_, window, cx| match result {
                Ok(()) => {
                    window.push_notification(
                        Notification::success(i18n::t_str(
                            "chatgpt-app-launched",
                            &[("provider", &provider), ("model", &model)],
                        )),
                        cx,
                    );
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        provider = %provider,
                        model = %model,
                        "ChatGPT.app launch failed"
                    );
                    window.push_notification(
                        Notification::error(format!(
                            "{}: {e}",
                            i18n::t("chatgpt-app-launch-failed")
                        )),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// Launch VS Code with injections resolved from the persisted
    /// `vscode_app:` settings (Settings → 外部工具 → Visual Studio Code.app):
    /// Claude Code Extension block → ANTHROPIC_* env; Codex Extension block →
    /// CODEX_HOME + config.toml; both off → plain open. (工具 → VS Code menu
    /// entry and the sidebar new-session menu's VS Code item.) `folder` is
    /// `Some` when the launch should open a directory (the sidebar passes the
    /// project path or the workspace cwd); the 工具 menu passes `None` for a
    /// folder-less launch. Same background-spawn + notification shape as
    /// `launch_chatgpt_app`; the cx injection path may block on login-shell
    /// env resolution and — when VS Code is already running — on the restart
    /// confirmation + graceful quit wait.
    pub fn launch_vscode_app(
        &mut self,
        folder: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    manox_ext_agents::launch_vscode_app_from_settings(folder.as_deref())
                })
                .await;
            let _ = this.update_in(cx, |_, window, cx| match result {
                Ok(()) => {
                    window.push_notification(
                        Notification::success(i18n::t("vscode-app-launched")),
                        cx,
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, "VS Code launch failed");
                    window.push_notification(
                        Notification::error(format!(
                            "{}: {e}",
                            i18n::t("vscode-app-launch-failed")
                        )),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// Route a sidebar click on an external row. A live session attaches its
    /// running TUI; a resumable row (restored from a sidecar) re-spawns the
    /// CLI with its resume flag. Nothing is resumed at launch — only when the
    /// user picks the row, mirroring the manox thread contract.
    pub fn open_external_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.external_sessions.iter().any(|s| s.id == id) {
            self.attach_external_session(id, window, cx);
            return;
        }
        if self.resumable_external.iter().any(|s| s.id == id) {
            self.resume_external_session(id, window, cx);
        }
    }

    /// Re-spawn an unclosed external session's CLI with its resume flag so the
    /// CLI's own on-disk storage picks the conversation back up. The sidecar
    /// replays the original provider / model / cwd — no picker, and the resume
    /// command is never surfaced; the only feedback is a loading row until the
    /// TUI takes over. On failure the sidecar stays, so the row remains
    /// resumable for a retry.
    pub(super) fn resume_external_session(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(sidecar) = self.resumable_external.iter().find(|s| s.id == id).cloned() else {
            return;
        };
        let id = id.to_string();
        let Some(kind) = SessionKind::from_agent_id(&sidecar.agent_id) else {
            tracing::warn!(id, "resume sidecar carries a non-agent kind");
            return;
        };
        let Some(agent) = kind.agent() else {
            return;
        };
        // Guard and insert are adjacent on the UI thread, so a double-click
        // between them cannot double-spawn. Only ids that passed the
        // resolution above ever enter the set, so no early-return path can
        // strand a spinner.
        if !self.resuming_external.insert(id.clone()) {
            return;
        }
        self.sync_sidebar_external(cx);

        // The cx spawn (config load, keychain resolve, process spawn, socket
        // bind) is pure I/O, so it runs off the UI thread and the spinner
        // stays live while the CLI boots.
        let args = resume_args(&sidecar.agent_id, sidecar.cli_session_id.as_deref());
        let provider = sidecar.provider.clone();
        let model = sidecar.model.clone();
        let wire = sidecar.wire_api.clone();
        let cwd = PathBuf::from(&sidecar.cwd);
        let project = sidecar.project.clone();
        let created_at = sidecar.created_at;
        let this = cx.weak_entity();
        cx.spawn_in(window, async move |_window, cx| {
            let handle = match cx
                .background_spawn(async move {
                    let mut builder = manox_ext_agents::AgentBuilder::new()
                        .agent(agent)
                        .pty(true)
                        .provider(provider)
                        .model(model)
                        .cwd(cwd)
                        .passthrough(args);
                    if let Some(w) = wire {
                        builder = builder.wire_api(w);
                    }
                    builder.spawn()
                })
                .await
            {
                Ok(h) => Arc::new(h),
                Err(e) => {
                    tracing::error!(error = %e, id, "external session resume failed");
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.resuming_external.remove(&id);
                        this.sync_sidebar_external(cx);
                        window.push_notification(Notification::error(format!(
                            "{}: {e}",
                            i18n::t("external-session-resume-failed")
                        )), cx);
                    });
                    return;
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let source = manox_terminal::cx_session::CxSessionSource::new(Arc::clone(&handle));
                let terminal =
                    match manox_terminal::Terminal::spawn(id.clone(), PathBuf::from(&sidecar.cwd), 80, 24, Box::new(source))
                    {
                        Ok(t) => cx.new(|cx| TerminalProxy::new(t, cx)),
                        Err(e) => {
                            tracing::error!(error = %e, id, "failed to create resumed session terminal");
                            this.resuming_external.remove(&id);
                            this.sync_sidebar_external(cx);
                            window.push_notification(Notification::error(format!(
                                "{}: {e}",
                                i18n::t("external-session-resume-failed")
                            )), cx);
                            return;
                        }
                    };
                let exit_sub = this.subscribe_session_terminal(&terminal, &id, cx);
                let socket_path = handle.socket_path().map(std::path::Path::to_path_buf);
                let cx_session_id = handle
                    .socket_path()
                    .and_then(crate::external_session::cx_session_id_from_socket)
                    .unwrap_or_default();
                let view = TerminalView::new(terminal, cx);
                // Watch inputs captured before `sidecar` moves into the session.
                let watch_cwd = PathBuf::from(&sidecar.cwd);
                let watch_initial = sidecar.cli_session_id.clone();
                this.external_sessions.push(ExternalSession {
                    id: id.clone(),
                    kind,
                    created_at,
                    project,
                    title: None,
                    cx_session_id,
                    socket_path,
                    terminal_view: view,
                    handle: Some(handle),
                    _exit_sub: exit_sub,
                    // The resumed session keeps its sidecar (same identity):
                    // the disk record already represents it, and title sync
                    // keeps the row fresh for a possible later exit. Resumes
                    // are always top-level (sidebar row path), never
                    // thread-bound.
                    thread_bound: false,
                    sidecar: Some(sidecar),
                });
                this.start_cli_session_watch(&id, kind, &watch_cwd, watch_initial, cx);
                this.resuming_external.remove(&id);
                this.sync_sidebar_external(cx);
                this.attach_external_session(&id, window, cx);
            });
        })
        .detach();
    }

    /// Watch the CLI's on-disk conversation storage for this live session and
    /// record its native session id in the sidecar the moment it appears —
    /// the capture that lets a later resume target exactly this session's
    /// conversation. claude's id is assigned by manox at spawn
    /// (`--session-id`) and seeded into the snapshot + claims ledger
    /// synchronously, so the watcher there only tracks later forks
    /// (`/clear`, resume-forks); codex names its own sessions, so the watcher
    /// is the primary capture. Runs for fresh spawns and resumes alike. The
    /// task polls every 500ms, self-terminates when the session is removed,
    /// and releases its ledger claims on exit. No-op for kinds without
    /// capture support (copilot / plain terminal).
    pub(super) fn start_cli_session_watch(
        &mut self,
        id: &str,
        kind: SessionKind,
        cwd: &Path,
        initial_cli_session_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // Watch root + synchronous pre-session snapshot: any conversation
        // file appearing after this point belongs to this session.
        let (watch_dir, snapshot, nested, seed_claims) = match kind {
            SessionKind::ClaudeCode => {
                let Some(dir) = claude_project_dir_for_cwd(cwd) else {
                    return;
                };
                let mut snap = list_top_level_jsonl(&dir);
                let mut seeds: Vec<String> = Vec::new();
                if let Some(sid) = &initial_cli_session_id {
                    let name = format!("{sid}.jsonl");
                    snap.insert(name.clone());
                    // Seed the ledger synchronously: without this, a second
                    // same-cwd watcher started inside this watcher's first
                    // tick window (~500ms) could claim this session's file
                    // and record the wrong id in its own sidecar.
                    self.cli_session_claims
                        .entry(dir.clone())
                        .or_default()
                        .insert(name.clone());
                    seeds.push(name);
                }
                (dir, snap, false, seeds)
            }
            SessionKind::Codex => {
                let Some(dir) = codex_sessions_dir() else {
                    return;
                };
                // Full recursive walk of the sessions tree every tick — fine
                // at current scale; date-dir pruning or a slower cadence is
                // the lever if the history grows huge.
                let snap = list_nested_jsonl(&dir);
                (dir, snap, true, Vec::new())
            }
            SessionKind::GithubCopilot | SessionKind::Terminal => return,
        };
        // claude only: a newly appearing sibling project dir whose transcripts
        // carry the session's cwd wins over the computed slug (adoption).
        let canonical_cwd = std::fs::canonicalize(cwd)
            .ok()
            .and_then(|p| p.to_str().map(str::to_string));
        let mut root_dir_names: HashSet<String> = watch_dir
            .parent()
            .and_then(|root| std::fs::read_dir(root).ok())
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let session_id_key = id.to_string();
        let this = cx.weak_entity();
        cx.spawn(async move |_this, cx| {
            let mut watch_dir = watch_dir;
            let mut snapshot = snapshot;
            let mut claimed_any = initial_cli_session_id.is_some();
            let mut adoption_done = false;
            let mut my_claims: Vec<String> = seed_claims;
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let dir = watch_dir.clone();
                let snap = snapshot.clone();
                let roots = root_dir_names.clone();
                let canonical = canonical_cwd.clone();
                let scan_nested = nested;
                let do_adoption = !nested && !claimed_any && !adoption_done;
                let (candidates, adopted) = cx
                    .background_spawn(async move {
                        let current = if scan_nested {
                            list_nested_jsonl(&dir)
                        } else {
                            list_top_level_jsonl(&dir)
                        };
                        let candidates: Vec<(String, Option<String>)> =
                            new_file_names(&snap, &current)
                                .into_iter()
                                .map(|name| {
                                    let sid = if scan_nested {
                                        codex_session_id_from_rollout(&dir.join(&name))
                                    } else {
                                        claude_session_id_from_file_name(&name)
                                    };
                                    (name, sid)
                                })
                                .collect();
                        let mut adopted: Option<(PathBuf, HashSet<String>)> = None;
                        if do_adoption
                            && let Some(canonical) = &canonical
                            && let Some(root) = dir.parent()
                            && let Ok(entries) = std::fs::read_dir(root)
                        {
                            for e in entries.flatten() {
                                let name = e.file_name().to_string_lossy().into_owned();
                                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                                if !is_dir || roots.contains(&name) {
                                    continue;
                                }
                                let candidate_dir = e.path();
                                let listing = list_top_level_jsonl(&candidate_dir);
                                let matches = listing.iter().any(|f| {
                                    claude_cwd_from_file_head(&candidate_dir.join(f), 8 * 1024)
                                        .as_deref()
                                        == Some(canonical.as_str())
                                });
                                if matches {
                                    adopted = Some((candidate_dir, listing));
                                    break;
                                }
                            }
                        }
                        (candidates, adopted)
                    })
                    .await;
                // Candidates whose id resolved join the snapshot whatever the
                // claim outcome (claimed here, or owned by another watcher);
                // id-less codex rollouts stay out so the next tick retries.
                let resolved_names: Vec<String> = candidates
                    .iter()
                    .filter(|(_, sid)| sid.is_some())
                    .map(|(name, _)| name.clone())
                    .collect();
                let dir_for_claims = watch_dir.clone();
                let claimed_files = this
                    .update(cx, |this, _cx| -> Option<Vec<String>> {
                        if !this
                            .external_sessions
                            .iter()
                            .any(|s| s.id == session_id_key)
                        {
                            return None;
                        }
                        let ledger = this.cli_session_claims.entry(dir_for_claims).or_default();
                        let mut claimed = Vec::new();
                        for (file, sid) in candidates {
                            let Some(sid) = sid else { continue };
                            if !ledger.insert(file.clone()) {
                                continue;
                            }
                            if let Some(session) = this
                                .external_sessions
                                .iter_mut()
                                .find(|s| s.id == session_id_key)
                                && let Some(sidecar) = session.sidecar.as_mut()
                            {
                                sidecar.cli_session_id = Some(sid);
                                if let Err(e) = write_sidecar(sidecar) {
                                    tracing::warn!(
                                        error = %e,
                                        id = session_id_key,
                                        "external session sidecar cli-session-id update failed"
                                    );
                                }
                                claimed.push(file);
                            }
                        }
                        Some(claimed)
                    })
                    .unwrap_or(None);
                let Some(claimed_files) = claimed_files else {
                    break;
                };
                snapshot.extend(resolved_names);
                if !claimed_files.is_empty() {
                    claimed_any = true;
                    my_claims.extend(claimed_files);
                }
                if let Some((dir, _listing)) = adopted {
                    // The adopted dir did not exist at session start, so every
                    // file in it belongs to this session: reset the snapshot to
                    // empty and let the next tick claim them all.
                    watch_dir = dir;
                    snapshot = HashSet::new();
                    root_dir_names.clear();
                    adoption_done = true;
                }
            }
            // The session is gone: release this watcher's claims so the
            // ledger does not grow unboundedly (future watchers snapshot the
            // dirs afresh at their own spawn).
            if !my_claims.is_empty() {
                let _ = this.update(cx, |this, _cx| {
                    this.cli_session_claims.retain(|_dir, claimed| {
                        for name in &my_claims {
                            claimed.remove(name);
                        }
                        !claimed.is_empty()
                    });
                });
            }
        })
        .detach();
    }

    /// Display an already-running external session in the main area. Does not
    /// touch the foreground `Thread` (the thread entity stays mounted; only the
    /// view mode flips) — the session's terminal keeps running across switches
    /// because the `ExternalSession` owns the live `TerminalView` + handle.
    /// Focuses the terminal view on the next frame so the user can type
    /// immediately without clicking into the TUI.
    pub fn attach_external_session(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self
            .external_sessions
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.terminal_view.clone());
        if view.is_none() {
            return;
        }
        self.active_external = Some(id.to_string());
        self.view_mode = ViewMode::ExternalSession;
        self.sidebar
            .update(cx, |s, cx| s.set_selected(Some(id.to_string()), cx));
        cx.notify();
        if let Some(view) = view {
            self.focus_external_view(view, window, cx);
        }
    }

    /// Focus an external session's terminal view on the next frame. Deferred so
    /// the `TerminalView` element is mounted (the view-mode flip schedules a
    /// re-render) before the focus is set — GPUI can't focus an element that
    /// hasn't rendered its `track_focus` yet.
    pub(super) fn focus_external_view(
        &self,
        view: Entity<terminal_ui::TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.defer(cx, move |window, cx| {
            let handle = view.read(cx).focus_handle();
            window.focus(&handle, cx);
        });
    }

    /// Route a freshly spawned external session to its placement: full-window
    /// attach (the sidebar-row path) or a right-pane Session tab.
    pub(super) fn place_external_session(
        &mut self,
        id: &str,
        placement: SessionPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match placement {
            SessionPlacement::FullWindow => self.attach_external_session(id, window, cx),
            SessionPlacement::RightPane { replace_tab } => {
                self.mount_session_tab(id, replace_tab, window, cx)
            }
        }
    }

    /// Mount an external session's terminal on the right pane: replace the
    /// Launcher tab at `replace_tab` while it still holds one, else append a
    /// fresh tab. The terminal gains focus on the next frame so the TUI
    /// receives keystrokes immediately.
    pub(super) fn mount_session_tab(
        &mut self,
        id: &str,
        replace_tab: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.right_tabs.get(replace_tab), Some(RightTab::Launcher)) {
            self.right_tabs[replace_tab] = RightTab::Session(id.to_string());
            self.set_active_right_tab(replace_tab, cx);
        } else {
            self.right_tabs.push(RightTab::Session(id.to_string()));
            self.set_active_right_tab(self.right_tabs.len() - 1, cx);
        }
        self.right_pane_visible = true;
        if let Some(view) = self
            .external_sessions
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.terminal_view.clone())
        {
            self.focus_external_view(view, window, cx);
        }
        cx.notify();
    }

    /// Mirror an external agent's OSC title (`TerminalEvent::Title`) into its
    /// `ExternalSession`, keep the durable sidecar's title in sync, and
    /// refresh the sidebar projection + titlebar when a visible title
    /// changed. No-op when the session was already removed (a spurious title
    /// after close) or the title sanitizes to the stored value.
    pub(super) fn set_external_title(
        &mut self,
        id: &str,
        title: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let active = self.active_external.as_deref() == Some(id);
        // TUI-set titles arrive verbatim; sanitize so the stored form is a
        // single printable line (control bytes / line breaks would stretch
        // the sidebar row).
        let new = title
            .as_deref()
            .and_then(crate::external_session::sanitize_osc_title);
        let Some(session) = self.external_sessions.iter_mut().find(|s| s.id == id) else {
            return;
        };
        if session.title == new {
            return;
        }
        let thread_bound = session.thread_bound;
        session.title = new.clone();
        // Keep the durable sidecar's title in sync so a later exit offers
        // the resumable row under the latest OSC title rather than the one
        // captured at spawn / last resume.
        if let Some(sidecar) = session.sidecar.as_mut() {
            sidecar.title = new.clone();
            if let Err(e) = write_sidecar(sidecar) {
                tracing::warn!(error = %e, id, "external session sidecar title update failed");
            }
        }
        // Thread-bound sessions never project into the sidebar; rebuilding
        // the projection on their title churn would be pure waste.
        if !thread_bound {
            self.sync_sidebar_external(cx);
        }
        if active {
            cx.notify();
        }
    }

    /// Kill an external session and remove it (the sidebar `×` path). The
    /// explicit `handle.kill()` unblocks the reader thread even mid-`read`, so
    /// the terminal drains instead of hanging on a dead PTY; `kill` on an
    /// already-dead child is best-effort (warn-logged). Removal itself —
    /// including sidebar sync + fallback-to-conversation — is shared with the
    /// natural-exit path in [`remove_external_session`].
    pub fn close_external_session(&mut self, id: &str, cx: &mut Context<Self>) {
        let kill_handle = self
            .external_sessions
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.handle.as_ref().map(Arc::clone));
        if let Some(handle) = kill_handle
            && let Err(e) = handle.kill()
        {
            tracing::warn!(error = %e, id, "external session kill failed");
        }
        // Plain PTY sessions (no cx handle) tear down on drop: the dropped
        // TerminalView's PtyHandle kills the child tree in its own Drop.
        self.remove_external_session(id, cx);
    }

    /// Remove an external session without killing the child — the natural-exit
    /// path (the `ChildExit` subscription fired because the CLI already exited).
    /// Dropping the `ExternalSession` drops its `TerminalView` (and thus the
    /// `Terminal` + `CxSessionSource`); the last `Arc<SessionHandle>` ref then
    /// drops, and cx's `SessionHandle::Drop` does best-effort reap + socket
    /// cleanup. If the removed session was the active one, fall back to the
    /// conversation pane.
    pub(super) fn remove_external_session(&mut self, id: &str, cx: &mut Context<Self>) {
        let was_active = self.active_external.as_deref() == Some(id);
        self.external_sessions.retain(|s| s.id != id);
        // A Session tab embedding this terminal must not outlive the session
        // (a natural `/exit` closes its tab instead of stranding a dead PTY).
        if let Some(ix) = self
            .right_tabs
            .iter()
            .position(|t| matches!(t, RightTab::Session(sid) if *sid == id))
        {
            self.right_tabs.remove(ix);
            self.reseat_active_after_close(ix, cx);
            self.hide_right_pane_if_empty(cx);
            self.editor_open = self
                .right_tabs
                .get(self.active_right_tab)
                .is_some_and(|t| matches!(t, RightTab::Editor));
        }
        // The session is now explicitly closed (sidebar `×` or a natural CLI
        // exit): drop its sidecar — both the in-memory row and the disk record
        // — so it is no longer offered for resume.
        self.resumable_external.retain(|s| s.id != id);
        remove_sidecar(id);
        if was_active {
            self.active_external = None;
            self.focus_conversation(cx);
        }
        self.sync_sidebar_external(cx);
    }

    /// Push a fresh projection of the external sessions to the sidebar so its
    /// list reflects spawns / closes / resumes without owning the
    /// PTY-bearing structs. Live sessions render first, then the resumable
    /// rows restored from sidecars.
    pub(super) fn sync_sidebar_external(&mut self, cx: &mut Context<Self>) {
        // Thread-bound (right-pane) sessions are resources of their thread
        // and never surface in the top-level list.
        let live: Vec<_> = self
            .external_sessions
            .iter()
            .filter(|s| !s.thread_bound)
            .map(|s| s.summary())
            .collect();
        let mut summaries = merge_external_summaries(live, self.resumable_external.clone());
        let resuming = self.resuming_external.clone();
        for s in &mut summaries {
            s.resuming = resuming.contains(s.id.as_str());
        }
        self.sidebar
            .update(cx, |s, cx| s.set_external_sessions(summaries, cx));
    }

    /// Open a browser tab navigating to `url` (defaulting to
    /// [`crate::views::browser_view::DEFAULT_URL`] when empty) and focus it.
    /// Returns the allocated `BrowserTabId` so callers (the host, tool
    /// surface) can drive the tab afterwards. The webview is built untrusted:
    /// no Tauri command surface, only the notify/inbound bridges.
    pub fn open_browser_tab(
        &mut self,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> BrowserTabId {
        let tab_id = self.restore_browser_tab(url, window, cx);
        self.right_pane_visible = true;
        self.right_tabs.push(RightTab::Browser(tab_id));
        self.set_active_right_tab(self.right_tabs.len() - 1, cx);
        tab_id
    }

    /// Poll every browser tab's `<title>` through the host so right-pane tab
    /// labels track the loaded page. Mirrors the thinking-ticker pattern:
    /// bumping `browser_title_ticker_gen` (last browser tab closed, or a new
    /// ticker superseding this one) or an emptied `browser_views` map
    /// self-terminates the loop.
    pub(super) fn spawn_browser_title_ticker(&mut self, cx: &mut Context<Self>) {
        self.browser_title_ticker_gen = self.browser_title_ticker_gen.wrapping_add(1);
        let entity = cx.entity().clone();
        let ticker_gen = self.browser_title_ticker_gen;
        cx.spawn(async move |_this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                let alive = entity.read_with(cx, |this, _| {
                    this.browser_title_ticker_gen == ticker_gen && !this.browser_views.is_empty()
                });
                if !alive {
                    break;
                }
                let Some(host) = crate::browser_host::WorkspaceBrowserHost::concrete() else {
                    break;
                };
                // Only tabs on screen need fresh titles: hidden panes and
                // tabs stashed under other threads keep their (hidden)
                // webviews idle instead of polling JS into them every tick.
                let ids: Vec<BrowserTabId> = entity.read_with(cx, |this, _| {
                    if this.right_pane_open() {
                        this.right_tabs
                            .iter()
                            .filter_map(|t| match t {
                                RightTab::Browser(id) => Some(*id),
                                _ => None,
                            })
                            .collect()
                    } else {
                        Vec::new()
                    }
                });
                if ids.is_empty() {
                    continue;
                }
                for id in ids {
                    // A superseded / dead ticker must stop issuing evals.
                    let still =
                        entity.read_with(cx, |this, _| this.browser_title_ticker_gen == ticker_gen);
                    if !still {
                        break;
                    }
                    // `page_title` reads the Workspace inside `inject_script`,
                    // so it must not run under a Workspace lease — plain
                    // `AsyncApp::update`, not `entity.update`.
                    let task = cx.update(|cx| host.page_title(id, cx));
                    if let Ok(title) = task.await
                        && !title.is_empty()
                    {
                        entity.update(cx, |this, cx| {
                            if let Some(view) = this.browser_views.get(&id).cloned() {
                                view.update(cx, |v, cx| v.set_title(title.clone(), cx));
                            }
                        });
                    }
                }
            }
        })
        .detach();
    }

    /// Show only the active browser tab's native webview and hide the rest.
    /// No webview may draw while the pane is hidden or another view mode is
    /// up (the platform view is not a gpui element and ignores layout).
    pub(super) fn sync_browser_visibility(&mut self, cx: &mut Context<Self>) {
        let active_browser =
            if matches!(self.view_mode, ViewMode::Workspace) && self.right_pane_open() {
                match self.right_tabs.get(self.active_right_tab) {
                    Some(RightTab::Browser(id)) => Some(*id),
                    _ => None,
                }
            } else {
                None
            };
        for (id, view) in self.browser_views.iter() {
            let should_show = active_browser == Some(*id);
            view.update(cx, |v, cx| {
                let wv = v.webview().clone();
                match (should_show, wv.read(cx).visible()) {
                    (true, false) => wv.update(cx, |w, _| w.show()),
                    (false, true) => wv.update(cx, |w, _| w.hide()),
                    _ => {}
                }
            });
        }
    }

    /// Close and recycle a browser tab by id. Dropping the `BrowserView`
    /// drops the underlying native webview, whose `Drop` hides and detaches
    /// the platform view. No-op if the id is not live.
    pub fn close_browser_tab(&mut self, tab_id: BrowserTabId, cx: &mut Context<Self>) {
        if self.browser_views.remove(&tab_id).is_none() {
            return;
        }
        // Reclaim the host's routing entry so a late notify for this tab finds
        // no route (no orphaned oneshot). `close_tab` reclaims first then calls
        // us — in that direction `reclaim_routes` is a no-op; this call covers
        // the UI-close direction.
        if let Some(host) = crate::browser_host::WorkspaceBrowserHost::concrete() {
            host.reclaim_routes(tab_id);
        }
        if self.browser_views.is_empty() {
            // Last browser tab gone — retire the title ticker.
            self.browser_title_ticker_gen = self.browser_title_ticker_gen.wrapping_add(1);
        }
        if let Some(ix) = self
            .right_tabs
            .iter()
            .position(|t| matches!(t, RightTab::Browser(id) if *id == tab_id))
        {
            self.right_tabs.remove(ix);
            self.reseat_active_after_close(ix, cx);
            self.hide_right_pane_if_empty(cx);
            self.editor_open = self
                .right_tabs
                .get(self.active_right_tab)
                .is_some_and(|t| matches!(t, RightTab::Editor));
        }
        cx.notify();
    }
}
