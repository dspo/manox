//! Regression: a non-question authorization (a `sandbox_permissions`
//! escalation from Edit/Write, or an ask payload the card cannot parse)
//! must surface as the generic approval card. Before this, such a pending
//! call blocked the thread invisibly — the user saw a stuck tool with no
//! way to answer it, and the only escape was cancelling the turn.
#![cfg(feature = "test-support")]

mod common;

use agent::PermissionDecision;
use common::{init_harness, open_workspace};
use gpui::{TestAppContext, VisualTestContext};

#[gpui::test]
async fn escalation_authorization_surfaces_as_generic_card(cx: &mut TestAppContext) {
    init_harness(cx);
    let (window, workspace) = open_workspace(cx);
    let mut visual = VisualTestContext::from_window(window.into(), cx);

    // The GateEscalationApprover payload: no `questions` key, so the ask
    // card cannot parse it.
    let before = workspace.read_with(&visual.cx, |ws, _| ws.diagnostic_pending_auth());
    assert!(before.is_none(), "no pending card before the event");

    workspace.update(&mut visual.cx, |ws, cx| {
        ws.diagnostic_seed_auth(
            "call_1",
            "Edit",
            "escalate sandbox to danger-full-access: deliver the plan in the worktree",
            cx,
        );
    });

    let pending = workspace
        .read_with(&visual.cx, |ws, _| ws.diagnostic_pending_auth())
        .expect("escalation surfaces as the generic card");
    assert_eq!(pending.0, "call_1");
    assert_eq!(pending.1, "Edit");
    assert!(pending.2.contains("danger-full-access"));
    assert!(
        workspace.read_with(&visual.cx, |ws, _| ws.diagnostic_blocking_overlay_active()),
        "the card blocks the workspace like the ask card does"
    );

    // The verdict clears the card — the visible decision replaces the
    // invisible 58-hour park this regression is named after.
    workspace.update(&mut visual.cx, |ws, cx| {
        ws.resolve_auth_for_test(PermissionDecision::AllowOnce, cx);
    });
    let after = workspace.read_with(&visual.cx, |ws, _| ws.diagnostic_pending_auth());
    assert!(after.is_none(), "card cleared by the verdict");
    assert!(
        !workspace.read_with(&visual.cx, |ws, _| ws.diagnostic_blocking_overlay_active()),
        "overlay gone with the card"
    );
    agent::thread_store::drop_global_for_test();
}
