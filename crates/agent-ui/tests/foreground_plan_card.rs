//! Regression: the foreground plan card must survive a normal turn end.
//! The proposal turn settles right after `ProposePlan`; the `TurnFinished`
//! handler used to demote the card unconditionally, flashing its verdict
//! buttons away and collapsing it into a plain record. The sidebar
//! pending-plan badge follows the same lifecycle: it survives a normal
//! settle while the verdict is due and clears on a cancelled turn.
#![cfg(feature = "test-support")]

mod common;

use agent::ThreadEvent;
use common::{emit, fake_thread, init_harness, open_workspace, write_plan_file};
use gpui::{TestAppContext, VisualTestContext};

#[gpui::test]
async fn foreground_plan_card_survives_normal_turn_end(cx: &mut TestAppContext) {
    init_harness(cx);
    let (window, workspace) = open_workspace(cx);
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    let (_dir, plan_file) = write_plan_file();

    let a = fake_thread(cx, Vec::new());
    let a_id = a.read(|t| t.id.0.clone());
    visual.update(|window, cx| {
        workspace.update(cx, |ws, cx| {
            ws.diagnostic_attach_thread(a.clone(), window, cx)
        });
    });
    cx.run_until_parked();

    emit(
        &workspace,
        &mut visual.cx,
        &a_id,
        ThreadEvent::PlanReady {
            plan_file,
            title: "Audit".into(),
        },
    );
    assert!(
        workspace.read_with(&visual.cx, |ws, _| ws.diagnostic_pending_plan_review()),
        "PlanReady sets the pending review"
    );
    assert!(
        workspace.read_with(&visual.cx, |ws, cx| ws.diagnostic_tail_plan_active(cx)),
        "the fresh card is active"
    );
    assert!(
        agent::thread_store_global().read(|s| s.pending_plan_contains(&a_id)),
        "PlanReady raises the pending-plan badge"
    );

    // The proposal turn settles normally; the card must survive.
    emit(
        &workspace,
        &mut visual.cx,
        &a_id,
        ThreadEvent::TurnFinished {
            cancelled: false,
            failed: false,
            stranded_steer_ids: Vec::new(),
        },
    );
    assert!(
        workspace.read_with(&visual.cx, |ws, _| ws.diagnostic_pending_plan_review()),
        "a normal settle must not clear the pending review"
    );
    assert!(
        workspace.read_with(&visual.cx, |ws, cx| ws.diagnostic_tail_plan_active(cx)),
        "the card stays interactive after the proposal turn"
    );
    assert!(
        agent::thread_store_global().read(|s| s.pending_plan_contains(&a_id)),
        "the pending-plan badge survives a normal settle while the verdict is due"
    );

    // An abnormal end demotes it — the verdict is moot.
    emit(
        &workspace,
        &mut visual.cx,
        &a_id,
        ThreadEvent::TurnFinished {
            cancelled: true,
            failed: false,
            stranded_steer_ids: Vec::new(),
        },
    );
    assert!(
        !workspace.read_with(&visual.cx, |ws, _| ws.diagnostic_pending_plan_review()),
        "a cancelled turn clears the pending review"
    );
    assert!(
        !workspace.read_with(&visual.cx, |ws, cx| ws.diagnostic_tail_plan_active(cx)),
        "the cancelled turn demotes the card to a plain record"
    );
    assert!(
        !agent::thread_store_global().read(|s| s.pending_plan_contains(&a_id)),
        "a cancelled turn clears the pending-plan badge"
    );
    agent::thread_store::drop_global_for_test();
}
