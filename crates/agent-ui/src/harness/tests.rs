//! Repro tests for the two-concurrent-threads crash. Both are `#[ignore]`-gated:
//! they need the real provider config at `~/.config/cx/` and (for the live
//! variant) `MANOX_RUN_LIVE=1`. Run with `cargo test -p agent-ui -- --ignored`.

#![cfg(test)]

use std::time::Duration;

use agent::language_model::{LanguageModelCompletionEvent, StopReason};

use super::test_support::{ReplayModel, build_workspace, setup};
use super::{Harness, IdleState, await_idle_sync};

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
