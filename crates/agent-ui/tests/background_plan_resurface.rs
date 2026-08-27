//! Regression: a plan proposed while the thread is parked in the
//! background. The background `PlanReady` subscription must stash the review
//! (and keep the sidebar badge through the settling turn); switching back
//! re-surfaces an active verdict card.
#![cfg(feature = "test-support")]

mod common;

use agent::ThreadEvent;
use agent_ui::thread_proxy::ThreadProxy;
use common::{emit, fake_thread, init_harness, open_workspace, write_plan_file};
use gpui::{TestAppContext, VisualTestContext};

#[gpui::test]
async fn background_plan_ready_resurfaces_on_switch_back(cx: &mut TestAppContext) {
    init_harness(cx);
    let (window, workspace) = open_workspace(cx);
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    let (_dir, plan_file) = write_plan_file();

    let a: gpui::Entity<ThreadProxy> = fake_thread(cx, Vec::new());
    let a_id = cx.read(|cx| a.read(cx).id.0.clone());
    let b = fake_thread(cx, Vec::new());
    // A must look running so attaching B parks it in the background.
    a.update(&mut visual.cx, |t, _| t.set_running_for_test(true));

    visual.update(|window, cx| {
        workspace.update(cx, |ws, cx| {
            ws.diagnostic_attach_thread(a.clone(), window, cx)
        });
    });
    visual.update(|window, cx| {
        workspace.update(cx, |ws, cx| {
            ws.diagnostic_attach_thread(b.clone(), window, cx)
        });
    });

    // The parked thread proposes a plan; the turn then settles normally.
    emit(
        &a,
        &mut visual.cx,
        ThreadEvent::PlanReady {
            plan_file: plan_file.clone(),
            title: "Audit".into(),
        },
    );
    assert!(
        workspace.read_with(&visual.cx, |ws, _| ws.diagnostic_has_stashed_plan(&a_id)),
        "background PlanReady must stash the review"
    );
    assert!(
        agent::thread_store_global().read(|s| s.pending_plan_contains(&a_id)),
        "sidebar keeps the pending-plan badge"
    );
    emit(
        &a,
        &mut visual.cx,
        ThreadEvent::TurnFinished {
            cancelled: false,
            failed: false,
            stranded_steer_ids: Vec::new(),
        },
    );
    assert!(
        workspace.read_with(&visual.cx, |ws, _| ws.diagnostic_has_stashed_plan(&a_id)),
        "a normal settle keeps the stashed plan"
    );
    assert!(
        agent::thread_store_global().read(|s| s.pending_plan_contains(&a_id)),
        "the badge survives the settle while the verdict is due"
    );

    // Switching back re-surfaces the card as an active review.
    visual.update(|window, cx| {
        workspace.update(cx, |ws, cx| {
            ws.diagnostic_attach_thread(a.clone(), window, cx)
        });
    });
    assert!(
        workspace.read_with(&visual.cx, |ws, _| ws.diagnostic_pending_plan_review()),
        "switch-back restores the pending plan review"
    );
    assert!(
        workspace.read_with(&visual.cx, |ws, cx| ws.diagnostic_tail_plan_active(cx)),
        "the restored card must be active (verdict buttons rendered)"
    );
    agent::thread_store::drop_global_for_test();
}
