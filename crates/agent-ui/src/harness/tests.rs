//! Repro tests for the two-concurrent-threads crash + live external-session
//! lifecycle. All are `#[ignore]`-gated: they need the real provider config at
//! `~/.config/cx/` and (for live variants) `MANOX_RUN_LIVE=1`. Run with
//! `cargo test -p agent-ui --features debug -- --ignored`.

#![cfg(all(test, feature = "debug"))]

use std::time::Duration;

use agent::language_model::{LanguageModelCompletionEvent, StopReason};
use agent::provider::registry;
use agent::{ThreadEvent, ToolCallStatus};

use serde_json::json;

use super::test_support::{ReplayModel, build_workspace, setup};
use super::{Harness, IdleState, await_idle_sync};
use crate::external_session::SessionKind;

#[gpui::test]
async fn direct_messages_route_pending_plan_and_ask_inline(cx: &mut gpui::TestAppContext) {
    let guard = setup(cx);
    let model = ReplayModel::build("test/inline-interactions", Vec::new());
    let ws = build_workspace(cx, model);
    let h = Harness::new(ws.clone());

    cx.update(|cx| {
        let thread = ws.read(cx).thread.clone();
        thread.update(cx, |_, cx| {
            cx.emit(ThreadEvent::ToolCall {
                id: "plan_1".to_string(),
                name: "exit_plan_mode".to_string(),
                title: "Submit plan".to_string(),
                status: ToolCallStatus::PendingApproval,
                input: Some(serde_json::json!({})),
            });
            cx.emit(ThreadEvent::PlanProposed {
                id: "plan_1".to_string(),
                plan_text: "## Plan\n\nKeep this plan visible.".to_string(),
            });
        });
    });
    assert_eq!(
        cx.update(|cx| h.pending_plan_id(cx)),
        Some("plan_1".to_string())
    );

    let conversation = cx.update(|cx| h.read_conversation(cx));
    let plan_card = conversation["items"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item["kind"] == "tool_call" && item["name"] == "exit_plan_mode")
        })
        .expect("plan card is in the conversation");
    assert_eq!(plan_card["status"], "PendingApproval");
    assert_eq!(plan_card["output"], "## Plan\n\nKeep this plan visible.");

    cx.update(|cx| {
        h.send_message("Please revise the plan with the new scope.".into(), cx)
            .unwrap()
    });
    assert_eq!(cx.update(|cx| h.pending_plan_id(cx)), None);
    assert!(
        cx.update(|cx| h.has_queued_follow_up(cx)),
        "composer submit should queue the user turn until the plan-continue result lands"
    );

    let ask_input = json!({
        "questions": [{
            "header": "Scope",
            "question": "Which path should we take?",
            "options": [
                { "label": "A", "description": "First path" },
                { "label": "B", "description": "Second path" }
            ]
        }]
    });
    cx.update(|cx| {
        let thread = ws.read(cx).thread.clone();
        thread.update(cx, |_, cx| {
            cx.emit(ThreadEvent::ToolCallAuthorization {
                id: "ask_1".to_string(),
                tool_name: "AskUserQuestion".to_string(),
                summary: "Which path should we take?".to_string(),
                input: ask_input,
            });
        });
    });
    assert!(cx.update(|cx| h.has_pending_ask(cx)));
    assert_eq!(cx.update(|cx| h.pending_auth_count(cx)), 1);

    cx.update(|cx| h.send_message("Use the second path.".into(), cx).unwrap());
    assert!(
        !cx.update(|cx| h.has_pending_ask(cx)),
        "composer submit should resolve the inline ask card"
    );
    assert_eq!(cx.update(|cx| h.pending_auth_count(cx)), 0);
    let conversation = cx.update(|cx| h.read_conversation(cx));
    let user_bubbles = conversation["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["kind"] == "user")
        .count();
    assert_eq!(
        user_bubbles, 0,
        "ask response is not a duplicate user bubble"
    );

    drop(guard);
}

/// Offline: two workspaces, two concurrent `run_turn`s against a replay model.
/// Reproduces the gpui-side concurrency path (double-lease, entity racing)
/// without a live provider. `#[ignore]` because `setup()` reads the real
/// provider config to satisfy `Workspace::new`'s registry lookup.
#[gpui::test]
#[ignore = "reads ~/.config/cx provider config; run with --ignored"]
async fn two_concurrent_threads_repro(cx: &mut gpui::TestAppContext) {
    let _guard = setup(cx);
    let model = ReplayModel::build(
        "test/replay",
        vec![
            LanguageModelCompletionEvent::Text("hello from model".into()),
            LanguageModelCompletionEvent::Stop(StopReason::EndTurn),
        ],
    );
    let ws1 = build_workspace(cx, model.clone());
    let ws2 = build_workspace(cx, model);

    let h1 = Harness::new(ws1.clone());
    let h2 = Harness::new(ws2.clone());
    cx.update(|cx| {
        h1.send_message("msg1".into(), cx).unwrap();
        h2.send_message("msg2".into(), cx).unwrap();
    });

    let s1 = await_idle_sync(&ws1, cx, Duration::from_secs(30));
    let s2 = await_idle_sync(&ws2, cx, Duration::from_secs(30));
    assert_ne!(s1, IdleState::StillRunning, "ws1 never went idle");
    assert_ne!(s2, IdleState::StillRunning, "ws2 never went idle");
    drop(_guard);
}

/// Live: real Bailian glm-5.2, two concurrent turns — the exact SIGABRT
/// scenario the user reported. Gated by `MANOX_RUN_LIVE`.
#[gpui::test]
#[ignore = "live provider + MANOX_RUN_LIVE required; reproduces the SIGABRT scenario"]
async fn two_concurrent_threads_live(cx: &mut gpui::TestAppContext) {
    if std::env::var("MANOX_RUN_LIVE").is_err() {
        return;
    }
    let guard = setup(cx);
    let model = cx.update(|_cx| {
        agent::provider::registry::global()
            .models()
            .iter()
            .find(|m| m.name().contains("glm-5.2"))
            .cloned()
            .expect("glm-5.2 model in registry")
    });
    let ws1 = build_workspace(cx, model.clone());
    let ws2 = build_workspace(cx, model);

    let h1 = Harness::new(ws1.clone());
    let h2 = Harness::new(ws2.clone());
    cx.update(|cx| {
        h1.send_message("杭州最近天气".into(), cx).unwrap();
        h2.send_message("北京最近天气".into(), cx).unwrap();
    });

    let s1 = await_idle_sync(&ws1, cx, Duration::from_secs(90));
    let s2 = await_idle_sync(&ws2, cx, Duration::from_secs(90));
    assert_ne!(s1, IdleState::StillRunning, "ws1 live turn did not finish");
    assert_ne!(s2, IdleState::StillRunning, "ws2 live turn did not finish");
    drop(guard);
}

/// Live spawn-success path: drive a real `claude` launch through
/// `cx::AgentBuilder` (the Phase 5 spawn primitive the cascade wizard invokes)
/// with a raw model id, assert it succeeds, then kill + reap. Verifies the spawn
/// wiring end-to-end — binary discovery, BYOK env injection, PTY setup — with the
/// model-id form production must pass (`m.name()`, the bare id; the pre-fix
/// `m.id()` key form is rejected by cx with "provider X 下未找到支持 claude 的
/// model"). Routed as a direct `AgentBuilder::spawn()` (not the sidebar event)
/// because a successfully-spawned interactive CLI streams continuous TUI output,
/// which keeps the gpui test executor from parking — so any `cx.update` after a
/// successful spawn would hang. The kill+reap here is the reduction of both the
/// × path and the natural-exit path. Gated by `MANOX_RUN_LIVE`.
#[gpui::test]
#[ignore = "live external CLI + MANOX_RUN_LIVE required; spawns real claude"]
async fn external_session_spawn_ok_live(cx: &mut gpui::TestAppContext) {
    if std::env::var("MANOX_RUN_LIVE").is_err() {
        return;
    }
    let guard = setup(cx);
    let (provider, model) =
        first_anthropic_model().expect("an anthropic-wire model in the provider config");

    let handle = cx::AgentBuilder::new()
        .agent(SessionKind::ClaudeCode.agent())
        .pty(true)
        .provider(provider)
        .model(model)
        .spawn()
        .expect("claude spawn succeeds with a real anthropic-wire provider/model");

    // Both production exit paths reduce to kill + reap: × (close_external_session)
    // kills then drops; natural-exit (ChildExit) lets the waiter reap. Assert the
    // handle is killable and reapable — the contract ExternalSession relies on.
    let _ = handle.kill();
    assert!(handle.wait().is_ok(), "wait reaps the killed child");
    drop(guard);
}

/// Live regression test for the `visible_agents` gap: the cascade wizard
/// (`sidebar.rs::build_agent_model_cascade`) filters `registry::global().models()`
/// by each model's `visible_agents()`. Before the cx-providers fix (cx PR #71),
/// `visible_agents` was `endpoint.agents.clone()` — endpoint-only, dropping
/// model-level `agents` — so a config that marks models via model-level `agents`
/// (empty endpoint `agents`) yielded `visible_agents == []` for every model and
/// the wizard showed "no model configured" for every agent. After the fix,
/// `visible_agents` is `effective_agents_for_model(...)` (wire_api-compatible
/// baseline, filtered by endpoint + model `agents`, empty = no restriction).
/// This asserts at least one anthropic-wire model now lists `claude` in its
/// `visible_agents` — the data contract the cascade wizard relies on. Gated by
/// `MANOX_RUN_LIVE` (reads the real provider config).
#[gpui::test]
#[ignore = "live MANOX_RUN_LIVE required; reads real provider config"]
async fn external_session_cascade_resolves_models_live(cx: &mut gpui::TestAppContext) {
    if std::env::var("MANOX_RUN_LIVE").is_err() {
        return;
    }
    let guard = setup(cx);
    let claude_models = cx.update(|_cx| {
        registry::global()
            .models()
            .iter()
            .filter(|m| {
                m.visible_agents().iter().any(|a| a == "claude")
                    && m.wire_api() == agent::provider::WireApi::Anthropic
            })
            .count()
    });
    assert!(
        claude_models > 0,
        "no anthropic-wire model lists `claude` in visible_agents — \
         the cascade wizard would show \"no model\" for Claude Code"
    );
    drop(guard);
}

/// Live error path: emitting `SpawnExternalSession` against a provider that does
/// not exist must not add a session (the spawn `Err` arm pushes a notification
/// and returns before touching `external_sessions`). Asserts the sidebar stays
/// empty rather than panicking or orphaning a half-session. Routed through the
/// sidebar event (not a direct `spawn_external_session` call) so it also covers
/// the Phase 4–5 wiring: `SidebarEvent::SpawnExternalSession` →
/// `subscribe_in` handler → `spawn_external_session` → `push_notification`. The
/// error arm returns before any CLI streams, so the executor parks and the test
/// does not hang. Gated by `MANOX_RUN_LIVE`.
#[gpui::test]
#[ignore = "live MANOX_RUN_LIVE required; exercises spawn error path"]
async fn external_session_spawn_error_live(cx: &mut gpui::TestAppContext) {
    if std::env::var("MANOX_RUN_LIVE").is_err() {
        return;
    }
    let guard = setup(cx);
    let ws = build_workspace_with_window(cx);
    emit_spawn(
        cx,
        &ws,
        SessionKind::ClaudeCode,
        "nonexistent-provider".into(),
        "nonexistent-model".into(),
    );
    cx.update(|cx| {
        assert!(
            ws.read(cx).external_sessions.is_empty(),
            "failed spawn must not add a session"
        );
    });
    drop(guard);
}

/// Emit `SidebarEvent::SpawnExternalSession` on the workspace's sidebar and pump
/// the executor so the `subscribe_in` handler runs `spawn_external_session`
/// outside any `window.update` (its `push_notification` error arm re-enters
/// `Root::update` and cannot run nested). This mirrors the production cascade-
/// wizard path, not a direct method call.
fn emit_spawn(
    cx: &mut gpui::TestAppContext,
    ws: &gpui::Entity<crate::workspace::Workspace>,
    kind: SessionKind,
    provider: String,
    model: String,
) {
    let sidebar = cx.update(|cx| ws.read(cx).sidebar.clone());
    cx.update(|cx| {
        sidebar.update(cx, |_, cx| {
            cx.emit(crate::views::sidebar::SidebarEvent::SpawnExternalSession(
                kind, provider, model, None,
            ));
        });
    });
    cx.run_until_parked();
}

/// Pick the first anthropic-wire model in the registry, returning its
/// `(provider_name, raw_model_id)`. `raw_model_id` is the model's bare id
/// (`m.name()`), NOT the manox stable key (`m.id()` = `provider/model/wire`) —
/// `cx::AgentBuilder::spawn()` matches `ResolvedModel.id` (bare id), so the raw
/// form is what production must pass. Used to feed the live spawn test a real
/// provider/model `AgentBuilder` can spawn `claude` against (claude is an
/// anthropic-wire agent).
///
/// Deliberately bypasses the cascade wizard's `visible_agents` filter so the
/// spawn test is not coupled to the user's agent-marking config (it spawns
/// against any anthropic-wire model, regardless of whether the config marks it
/// for `claude`). The `visible_agents` data contract the cascade wizard relies
/// on is locked separately by `external_session_cascade_resolves_models_live`.
/// Historically that contract was broken — cx-providers `from_config` filled
/// `visible_agents` with `endpoint.agents.clone()` (endpoint-only, dropping
/// model-level `agents`), so the wizard showed "no model" for every agent.
/// Fixed in cx PR #71 (rev `e2dd576`), which this Cargo.toml pins.
fn first_anthropic_model() -> Option<(String, String)> {
    use agent::provider::WireApi;
    registry::global()
        .models()
        .iter()
        .find_map(|m| (m.wire_api() == WireApi::Anthropic).then(|| (m.provider_name(), m.name())))
}

/// Open a Workspace in a test window for the live external-session tests. The
/// window is kept alive by the test app (its `subscribe_in` dispatch needs a
/// real window); the tests drive spawning by emitting `SpawnExternalSession` on
/// the sidebar, not by holding the window handle. Pins a no-op replay model onto
/// the thread — the external-session tests never start a manox turn, so the
/// thread's model is unused, it just needs *some* model for `Workspace::new`.
fn build_workspace_with_window(
    cx: &mut gpui::TestAppContext,
) -> gpui::Entity<crate::workspace::Workspace> {
    use agent::language_model::{AnyLanguageModel, LanguageModelCompletionEvent};
    let dummy: AnyLanguageModel = ReplayModel::build(
        "test/external-unused",
        Vec::<LanguageModelCompletionEvent>::new(),
    );
    build_workspace(cx, dummy)
}

/// Typing `/` in the composer must open the slash-command typeahead popover.
/// Drives the real text-input path (`simulate_input`) so the `InputEvent::Change`
/// subscription fires `sync_completion`; the popover should list the built-in
/// commands. Regression guard for issue #226.
#[gpui::test]
#[ignore = "reads ~/.config/cx provider config; run with --ignored"]
async fn slash_typeahead_opens_on_slash(cx: &mut gpui::TestAppContext) {
    use agent::language_model::{AnyLanguageModel, LanguageModelCompletionEvent};
    use gpui::AppContext as _;
    let _guard = setup(cx);
    cx.update(crate::slash_command::init);
    let dummy: AnyLanguageModel = ReplayModel::build(
        "test/slash-typeahead",
        Vec::<LanguageModelCompletionEvent>::new(),
    );
    // Capture the workspace entity + window handle so we can dispatch input.
    let mut ws_handle: Option<gpui::Entity<crate::workspace::Workspace>> = None;
    let mut window_handle: Option<gpui::AnyWindowHandle> = None;
    let (_root, _vctx) = cx.add_window_view(|window, cx| {
        let ws = cx.new(|cx| crate::workspace::Workspace::new(window, cx));
        ws_handle = Some(ws.clone());
        window_handle = Some(window.window_handle());
        gpui_component::Root::new(ws, window, cx)
    });
    let ws = ws_handle.expect("workspace captured");
    let window = window_handle.expect("window handle captured");
    // Pin the dummy model so Workspace::new's provider assumptions hold.
    cx.update(|cx| {
        ws.update(cx, |ws, cx| {
            ws.thread.update(cx, |t, cx| t.set_model(dummy, cx));
        });
    });
    cx.run_until_parked();
    // Focus the composer input then type `/`.
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.input_state.update(cx, |s, cx| s.focus(window, cx));
        });
    })
    .unwrap();
    cx.run_until_parked();
    cx.simulate_input(window, "/");
    cx.run_until_parked();
    let open = cx.read(|cx| ws.read(cx).completion_is_open());
    assert!(open, "typing `/` should open the completion popover");
    drop(_guard);
}
