//! Per-child-run escalation authority: the parent session's approval
//! channel, sliced one run at a time.
//!
//! The child snapshot is assembled once and shared by every concurrent
//! child, so the escalation state can never live on it: a shared grant cell
//! would let one Sailor's approved escalation widen a sibling's call, and a
//! shared approved-roots store would widen the Captain's. The
//! [`SubagentChildAuthority`] instead arms each run at the spawn boundary
//! with three pieces the run owns outright:
//!
//! - its own grant cell (the kernel bash's stamp → effective → clear
//!   pipeline reads it, and the per-run seatbelt backend's mode resolver
//!   reads the same cell, so the profile and the backend selection cannot
//!   disagree);
//! - its own approver, which parks the escalation on the PARENT session's
//!   approval gate as a regular approval card (the same
//!   `ToolCallAuthorization` round trip the Captain's escalations ride),
//!   named with the run's attribution;
//! - its own granted-roots overlay: the parent's roots read through live
//!   and read-only, and an approved escalation's root accumulates only in
//!   the overlay — approve once for this run, never re-ask, never leak.
//!
//! Both enforcing faces fail closed: an unanswered card (unattended run, no
//! UI client) denies after a bounded wait, a run spends a fixed escalation
//! budget, and a host that never installed the authority leaves the
//! snapshot's honest dead-end refusals in place.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use manox_harness::BackgroundRegistry;
use manox_harness::bash::BashTool;
use manox_harness::bash::persistent::PersistentShellOperations;
use manox_harness::sandbox::{
    AccessMode, EscalationApprover, EscalationOutcome, EscalationRequest, NO_GRANT,
    approve_escalation,
};
use manox_harness::subagent::ChildAuthority;
use manox_harness::tool::AgentTool as HarnessAgentTool;
use manox_harness::tool::ToolContext;
use manox_harness::tools::bash::BashOperations;
use tokio_util::sync::CancellationToken;

use super::tools_assembly::{SubagentBashTool, SubagentWriteFence};
use crate::approval::{ApprovalGate, fs_call_target_root};
use crate::permission::{PermissionDecision, ToolAuthorizationResponse};
use crate::thread::ThreadEvent;

/// How many escalation cards one child run may raise. A run that keeps
/// asking must not keep the parent's user busy card after card; past the
/// budget every further request fails with the count, no card raised.
pub(super) const MAX_ESCALATIONS_PER_RUN: usize = 4;

/// How long a parked escalation card waits for the parent's user before it
/// denies. The bound is what makes unattended runs safe: nobody home means
/// the child gets a denial and settles its work, never an indefinite park.
pub(super) const ESCALATION_ANSWER_TIMEOUT: Duration = Duration::from_secs(120);

/// One child run's escalation state, shared by the run's enforcing wrappers
/// (armed bash + both fs fences). Lives exactly as long as the run's tools.
pub(super) struct ChildEscalation {
    run_id: String,
    kind: String,
    label: String,
    gate: Arc<ApprovalGate>,
    /// Cards still raisable. Consumed by the approver per actual card; the
    /// wrappers pre-check so an exhausted budget produces an informative
    /// refusal instead of a surprise denial at the channel.
    budget_left: AtomicUsize,
    answer_timeout: Duration,
}

impl ChildEscalation {
    pub(super) fn new(
        gate: Arc<ApprovalGate>,
        run_id: String,
        kind: String,
        label: String,
    ) -> Arc<Self> {
        Self::build(gate, run_id, kind, label, ESCALATION_ANSWER_TIMEOUT)
    }

    fn build(
        gate: Arc<ApprovalGate>,
        run_id: String,
        kind: String,
        label: String,
        answer_timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            run_id,
            kind,
            label,
            gate,
            budget_left: AtomicUsize::new(MAX_ESCALATIONS_PER_RUN),
            answer_timeout,
        })
    }

    /// Test-only: shrink the card wait so the fail-closed timeout is
    /// observable without waiting real time.
    #[cfg(test)]
    pub(super) fn new_with_answer_timeout(
        gate: Arc<ApprovalGate>,
        run_id: String,
        kind: String,
        label: String,
        timeout: Duration,
    ) -> Arc<Self> {
        Self::build(gate, run_id, kind, label, timeout)
    }

    /// The model-facing budget guard: spent means refuse before asking.
    pub(super) fn check_budget(&self) -> Result<(), String> {
        if self.budget_left.load(Ordering::SeqCst) == 0 {
            return Err(format!(
                "this run's escalation budget is spent ({} requests); no further approval cards \
                 will be raised for it — stay inside the writable roots, or surface the blocker \
                 in your final summary",
                MAX_ESCALATIONS_PER_RUN
            ));
        }
        Ok(())
    }

    /// Consume one card slot; `false` when the budget is already spent (the
    /// wrappers' pre-check raced with a concurrent ask).
    fn consume_budget(&self) -> bool {
        self.budget_left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                if n == 0 { None } else { Some(n - 1) }
            })
            .is_ok()
    }

    /// The parent-channel approver this run's tools resolve through.
    pub(super) fn approver(self: &Arc<Self>) -> Arc<dyn EscalationApprover + Send + Sync> {
        Arc::new(ChildEscalationApprover(Arc::clone(self)))
    }

    /// Resolve one fs-fence escalation through the parent channel: budget
    /// guard, attributed card, verdict. On approval the covered root is
    /// returned for the caller to record into the run's overlay (approve
    /// once for this run, never re-ask). `Err` carries the model-facing
    /// text for every non-approved path.
    pub(super) async fn resolve_fs_escalation(
        self: &Arc<Self>,
        ask: FsEscalationAsk<'_>,
        call_id: &str,
        signal: &CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<PathBuf, String> {
        let FsEscalationAsk {
            tool_name,
            params,
            justification,
            effective_mode,
        } = ask;
        self.check_budget()?;
        let Some(target_root) = fs_call_target_root(tool_name, params, ctx) else {
            return Err("the escalation could not resolve the call's write target".to_string());
        };
        // The subject names the covered root, so the card — and every denial
        // text approve_escalation composes — says what would be widened.
        let noun = if tool_name == "Edit" { "edit" } else { "write" };
        let subject = format!("{noun} under {}", target_root.display());
        approve_escalation(
            EscalationRequest {
                requested_mode: AccessMode::FullAccess,
                justification: justification.to_string(),
                effective_mode,
                subject,
                tool_name: tool_name.to_string(),
                call_id: call_id.to_string(),
                signal: Some(signal.clone()),
            },
            Some(self.approver().as_ref()),
        )
        .await?;
        Ok(target_root)
    }
}

/// The call facts one fs-fence escalation judges: what the model asked to
/// do, where, in one sentence, and at what mode the call sits today.
pub(super) struct FsEscalationAsk<'a> {
    pub(super) tool_name: &'a str,
    pub(super) params: &'a serde_json::Value,
    pub(super) justification: &'a str,
    pub(super) effective_mode: AccessMode,
}

/// What the card wait resolved to, before outcome mapping.
enum Waited {
    Answered(ToolAuthorizationResponse),
    /// The gate's responder is gone (the parent session ended): deny.
    Dropped,
    /// The calling tool's signal fired (the child's call was cancelled).
    Cancelled,
}

/// The approver over one run's parent gate: every ask becomes a card on the
/// PARENT session (attributed to this run), resolved by the parent's user.
struct ChildEscalationApprover(Arc<ChildEscalation>);

#[async_trait::async_trait]
impl EscalationApprover for ChildEscalationApprover {
    async fn request(&self, req: EscalationRequest) -> EscalationOutcome {
        let esc = &self.0;
        if !esc.consume_budget() {
            // The wrappers' pre-check gives the informative refusal; this is
            // the raced-to-zero backstop — deny, never prompt.
            return EscalationOutcome::Unavailable;
        }
        let summary = format!(
            "Subagent {} \"{}\" (run {}) asks to escalate {} to {}: {}",
            esc.kind,
            esc.label,
            esc.run_id,
            req.subject,
            req.requested_mode.wire(),
            req.justification
        );
        let id = uuid::Uuid::new_v4().to_string();
        let input = serde_json::json!({
            "require_escalated": true,
            "justification": req.justification,
            "subject": req.subject,
            "subagent": {
                "runId": esc.run_id,
                "kind": esc.kind,
                "label": esc.label,
            },
        });
        let rx = esc.gate.register(
            &id,
            crate::permission::PendingAuthMeta {
                tool_name: req.tool_name.clone(),
                summary: summary.clone(),
                input: input.clone(),
            },
        );
        esc.gate.emit(ThreadEvent::ToolCallAuthorization {
            id: id.clone(),
            tool_name: req.tool_name,
            summary,
            input,
        });
        // Bounded wait on both exits: a cancelled call discards its card,
        // an unanswered card (unattended run, no UI client) times out to a
        // denial — the channel never parks a child forever.
        let wait = async {
            match req.signal {
                Some(signal) => tokio::select! {
                    r = rx => match r {
                        Ok(response) => Waited::Answered(response),
                        Err(_) => Waited::Dropped,
                    },
                    _ = signal.cancelled() => Waited::Cancelled,
                },
                None => match rx.await {
                    Ok(response) => Waited::Answered(response),
                    Err(_) => Waited::Dropped,
                },
            }
        };
        let waited = match tokio::time::timeout(esc.answer_timeout, wait).await {
            Ok(waited) => waited,
            Err(_) => {
                tracing::warn!(
                    run = %esc.run_id,
                    timeout = ?esc.answer_timeout,
                    "subagent escalation card unanswered; denying (fail-closed)"
                );
                esc.gate.discard(&id);
                return EscalationOutcome::Unavailable;
            }
        };
        // `discard` closes the card in the parent's journal either way; a
        // verdict that raced the discard already settled it (first wins).
        esc.gate.discard(&id);
        match waited {
            Waited::Answered(ToolAuthorizationResponse::Decision(
                PermissionDecision::AllowOnce,
            )) => EscalationOutcome::AllowedOnce,
            Waited::Answered(_) => EscalationOutcome::Rejected,
            Waited::Dropped => EscalationOutcome::Rejected,
            Waited::Cancelled => EscalationOutcome::Cancelled,
        }
    }
}

/// Arms each child run's enforcing tools with that run's own escalation
/// state, replacing the shared snapshot's Bash/Write/Edit for the run.
pub(super) struct SubagentChildAuthority {
    gate: Arc<ApprovalGate>,
    workspace: PathBuf,
    parent_roots: crate::granted_roots::GrantedRoots,
    sandbox_available: bool,
    unsandboxed_ops: Option<Arc<dyn BashOperations>>,
    edit_enforce_seen_lines: bool,
}

impl SubagentChildAuthority {
    pub(super) fn new(
        gate: Arc<ApprovalGate>,
        workspace: PathBuf,
        parent_roots: crate::granted_roots::GrantedRoots,
        sandbox_available: bool,
        unsandboxed_ops: Option<Arc<dyn BashOperations>>,
    ) -> Self {
        Self {
            gate,
            workspace,
            parent_roots,
            sandbox_available,
            unsandboxed_ops,
            edit_enforce_seen_lines: crate::settings::edit().enforce_seen_lines,
        }
    }
}

impl ChildAuthority for SubagentChildAuthority {
    fn arm(
        &self,
        run_id: &str,
        kind: &str,
        label: &str,
        tools: Vec<Arc<dyn HarnessAgentTool>>,
    ) -> Vec<Arc<dyn HarnessAgentTool>> {
        let overlay = self.parent_roots.child_overlay();
        let cell = Arc::new(AtomicI64::new(NO_GRANT));
        let escalation = ChildEscalation::new(
            Arc::clone(&self.gate),
            run_id.to_string(),
            kind.to_string(),
            label.to_string(),
        );

        // Standing-mode and network resolvers: the session authority read
        // live, identical to the Captain's. The grant cell is NOT the
        // baseline — a grant is what an escalation requests, so it cannot
        // be its own standing mode.
        let standing_gate = Arc::clone(&self.gate);
        let standing_resolver: Arc<dyn Fn() -> AccessMode + Send + Sync> =
            Arc::new(move || standing_gate.mode());
        let access_gate = Arc::clone(&self.gate);
        let access_resolver: Arc<dyn Fn() -> manox_harness::sandbox::NetworkAccess + Send + Sync> =
            Arc::new(move || access_gate.network_route().access());

        // Per-run seatbelt backend: reads THIS run's grant cell (never the
        // Captain's — one run's stamp cannot confine or free another run's
        // call) and THIS run's overlay roots, so an approved root widens
        // the seatbelt exactly as it widens the fs fence.
        let bash_ops: Arc<dyn BashOperations> = if self.sandbox_available {
            let cell_for_resolver = Arc::clone(&cell);
            let gate_for_resolver = Arc::clone(&self.gate);
            let mode_resolver: Arc<dyn Fn() -> AccessMode + Send + Sync> = Arc::new(move || {
                let g = cell_for_resolver.load(Ordering::SeqCst);
                if g != NO_GRANT {
                    AccessMode::from_i64(g)
                } else {
                    gate_for_resolver.mode()
                }
            });
            let route_gate = Arc::clone(&self.gate);
            let route_resolver: Arc<dyn Fn() -> crate::sandbox::NetworkRoute + Send + Sync> =
                Arc::new(move || route_gate.network_route());
            Arc::new(crate::sandbox::SandboxedBashOperations::new(
                self.workspace.clone(),
                overlay.clone(),
                mode_resolver,
                route_resolver,
            ))
        } else {
            Arc::new(PersistentShellOperations::new(self.workspace.clone()))
        };
        let mut bash = BashTool::new(bash_ops, Arc::new(BackgroundRegistry::new()))
            .with_sandbox_available(self.sandbox_available)
            .with_mode_resolver(Arc::clone(&standing_resolver))
            .with_network_resolver(access_resolver)
            .with_grant_cell(cell)
            .with_escalation_approver(escalation.approver());
        if let Some(ops) = &self.unsandboxed_ops {
            bash = bash.with_unsandboxed_operations(Arc::clone(ops));
        }

        let armed_bash: Arc<dyn HarnessAgentTool> = Arc::new(SubagentBashTool {
            inner: Arc::new(bash),
            escalation: Some(Arc::clone(&escalation)),
        });
        let armed_write: Arc<dyn HarnessAgentTool> = Arc::new(SubagentWriteFence {
            inner: Arc::new(crate::file_lock::FileLockedTool::new(
                Arc::new(manox_harness::tools::write::WriteTool),
                "sailor",
            )),
            mode_resolver: Arc::clone(&standing_resolver),
            granted_roots: overlay.clone(),
            escalation: Some(Arc::clone(&escalation)),
        });
        let armed_edit: Arc<dyn HarnessAgentTool> = Arc::new(SubagentWriteFence {
            inner: Arc::new(crate::file_lock::FileLockedTool::new(
                Arc::new(
                    manox_harness::tools::edit::EditTool::default()
                        .with_enforce_seen_lines(self.edit_enforce_seen_lines),
                ),
                "sailor",
            )),
            mode_resolver: standing_resolver,
            granted_roots: overlay,
            escalation: Some(escalation),
        });
        tools
            .into_iter()
            .map(|tool| match tool.name() {
                "Bash" => Arc::clone(&armed_bash),
                "Write" => Arc::clone(&armed_write),
                "Edit" => Arc::clone(&armed_edit),
                _ => tool,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn gate_with_events() -> (
        Arc<ApprovalGate>,
        tokio::sync::mpsc::UnboundedReceiver<crate::thread_engine::BackendNotice>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(ApprovalGate::new(tx, Arc::new(std::sync::Mutex::new(None)))),
            rx,
        )
    }

    /// The card reaches the parent's notice channel attributed to the run;
    /// an AllowOnce verdict maps to AllowedOnce.
    #[tokio::test]
    async fn an_ask_parks_an_attributed_card_and_allow_once_maps() {
        let (gate, mut rx) = gate_with_events();
        let esc = ChildEscalation::new(
            Arc::clone(&gate),
            "run-9".into(),
            "Sailor".into(),
            "fix tests".into(),
        );
        let approver = esc.approver();
        let ask = tokio::spawn(async move {
            approver
                .request(EscalationRequest {
                    requested_mode: AccessMode::FullAccess,
                    justification: "need to write the report".into(),
                    effective_mode: AccessMode::Sandbox,
                    subject: "command".into(),
                    tool_name: "Bash".into(),
                    call_id: "call-1".into(),
                    signal: None,
                })
                .await
        });
        // The card is observable before the verdict: it rides the parent's
        // notice channel as a regular ToolCallAuthorization.
        let notice = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the card parks on the channel")
            .expect("the channel stays open");
        let crate::thread_engine::BackendNotice::Event(event) = notice else {
            panic!("expected an event notice");
        };
        let card = match *event {
            crate::thread::ThreadEvent::ToolCallAuthorization {
                id,
                tool_name,
                summary,
                input,
            } => (id, tool_name, summary, input),
            other => panic!("expected a tool-call authorization, got {other:?}"),
        };
        assert_eq!(card.1, "Bash", "the card names the real tool");
        assert!(
            card.2.contains("Sailor") && card.2.contains("fix tests") && card.2.contains("run-9"),
            "the card names the run: {}",
            card.2
        );
        assert_eq!(
            card.3["subagent"]["runId"],
            serde_json::json!("run-9"),
            "the input carries the structured attribution"
        );
        // The verdict arrives through the parent's normal respond path.
        gate.respond(
            &card.0,
            ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce),
        );
        let outcome = tokio::time::timeout(Duration::from_secs(5), ask)
            .await
            .expect("resolves once answered")
            .expect("join");
        assert!(matches!(outcome, EscalationOutcome::AllowedOnce));
    }

    /// Wait for one card to park on the gate (the ask runs on its own task).
    async fn next_pending_id(gate: &ApprovalGate) -> String {
        for _ in 0..500 {
            if let Some((id, _)) = gate.pending_entries().into_iter().next() {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no card parked");
    }

    /// Every non-approval verdict — and a dropped responder — denies.
    #[tokio::test]
    async fn non_approval_verdicts_deny() {
        for denied in [
            ToolAuthorizationResponse::Decision(PermissionDecision::Deny),
            ToolAuthorizationResponse::AskUserQuestionExpired,
        ] {
            let (gate, _rx) = gate_with_events();
            let esc = ChildEscalation::new(gate.clone(), "r".into(), "Sailor".into(), "l".into());
            let approver = esc.approver();
            let handle = tokio::spawn(async move {
                approver
                    .request(EscalationRequest {
                        requested_mode: AccessMode::FullAccess,
                        justification: "j".into(),
                        effective_mode: AccessMode::Sandbox,
                        subject: "command".into(),
                        tool_name: "Bash".into(),
                        call_id: "c".into(),
                        signal: None,
                    })
                    .await
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
            let id = next_pending_id(&gate).await;
            gate.respond(&id, denied);
            let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("resolves")
                .expect("join");
            assert!(matches!(outcome, EscalationOutcome::Rejected));
        }
    }

    /// The budget bounds the cards a run can raise: the fourth ask still
    /// parks; a fifth is refused at the channel without a card.
    #[tokio::test]
    async fn the_budget_bounds_the_cards_per_run() {
        let (gate, _rx) = gate_with_events();
        let esc = ChildEscalation::new(gate.clone(), "r".into(), "Sailor".into(), "l".into());
        let req = || EscalationRequest {
            requested_mode: AccessMode::FullAccess,
            justification: "j".into(),
            effective_mode: AccessMode::Sandbox,
            subject: "command".into(),
            tool_name: "Bash".into(),
            call_id: "c".into(),
            signal: None,
        };
        for _ in 0..MAX_ESCALATIONS_PER_RUN {
            let approver = esc.approver();
            let handle = tokio::spawn(async move { approver.request(req()).await });
            tokio::time::sleep(Duration::from_millis(50)).await;
            let id = next_pending_id(&gate).await;
            gate.respond(
                &id,
                ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce),
            );
            let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("resolves")
                .expect("join");
            assert!(matches!(outcome, EscalationOutcome::AllowedOnce));
        }
        assert!(esc.check_budget().is_err(), "the budget is spent");
        // The next ask is refused without parking anything.
        let before = gate.pending_entries().len();
        assert!(matches!(
            esc.approver().request(req()).await,
            EscalationOutcome::Unavailable
        ));
        assert_eq!(gate.pending_entries().len(), before, "no card parked");
    }

    /// An unanswered card denies after the bounded wait — the unattended /
    /// no-UI-client shape — and leaves no parked card behind.
    #[tokio::test]
    async fn an_unanswered_card_times_out_to_a_denial() {
        let (gate, _rx) = gate_with_events();
        let esc = ChildEscalation::new_with_answer_timeout(
            gate.clone(),
            "r".into(),
            "Sailor".into(),
            "l".into(),
            Duration::from_millis(80),
        );
        let outcome = esc
            .approver()
            .request(EscalationRequest {
                requested_mode: AccessMode::FullAccess,
                justification: "j".into(),
                effective_mode: AccessMode::Sandbox,
                subject: "command".into(),
                tool_name: "Bash".into(),
                call_id: "c".into(),
                signal: None,
            })
            .await;
        assert!(matches!(outcome, EscalationOutcome::Unavailable));
        assert!(
            gate.pending_entries().is_empty(),
            "the timed-out card is discarded"
        );
    }

    /// A cancelled call discards its card and maps to Cancelled.
    #[tokio::test]
    async fn a_cancelled_call_discards_its_card() {
        let (gate, _rx) = gate_with_events();
        let esc = ChildEscalation::new(gate.clone(), "r".into(), "Sailor".into(), "l".into());
        let signal = CancellationToken::new();
        let approver = esc.approver();
        let signal_for_ask = signal.clone();
        let handle = tokio::spawn(async move {
            approver
                .request(EscalationRequest {
                    requested_mode: AccessMode::FullAccess,
                    justification: "j".into(),
                    effective_mode: AccessMode::Sandbox,
                    subject: "command".into(),
                    tool_name: "Bash".into(),
                    call_id: "c".into(),
                    signal: Some(signal_for_ask),
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        signal.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("resolves")
            .expect("join");
        assert!(matches!(outcome, EscalationOutcome::Cancelled));
        assert!(gate.pending_entries().is_empty(), "no dead card parked");
    }

    /// The authority replaces exactly the three enforcing names; reads pass
    /// through untouched.
    #[test]
    fn arm_replaces_the_enforcing_names_only() {
        struct NamedTool(&'static str);
        #[async_trait::async_trait]
        impl HarnessAgentTool for NamedTool {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "mock"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn is_read_only(&self) -> bool {
                true
            }
            async fn execute(
                &self,
                _: &str,
                _: serde_json::Value,
                _: CancellationToken,
                _: &dyn ToolContext,
            ) -> Result<manox_harness::tool::AgentToolResult, manox_harness::tool::ToolError>
            {
                Ok(manox_harness::tool::AgentToolResult::text("ran"))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (gate, _rx) = gate_with_events();
        let authority = SubagentChildAuthority::new(
            gate,
            dir.path().to_path_buf(),
            crate::granted_roots::GrantedRoots::new(dir.path()),
            true,
            None,
        );
        let tools: Vec<Arc<dyn HarnessAgentTool>> = vec![
            Arc::new(NamedTool("Read")),
            Arc::new(NamedTool("Bash")),
            Arc::new(NamedTool("Write")),
            Arc::new(NamedTool("Edit")),
            Arc::new(NamedTool("Grep")),
        ];
        let armed = authority.arm("run-1", "Sailor", "l", tools);
        let names: Vec<&str> = armed.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["Read", "Bash", "Write", "Edit", "Grep"]);
        // The replacements carry the escalation channel; the reads do not.
        assert!(
            armed[1]
                .parameters_schema()
                .get("properties")
                .and_then(|p| p.get("require_escalated"))
                .is_some(),
            "armed bash advertises the escalation fields"
        );
        assert!(
            armed[0]
                .parameters_schema()
                .get("properties")
                .is_none_or(|p| p.get("require_escalated").is_none()),
            "reads stay untouched"
        );
    }
}
