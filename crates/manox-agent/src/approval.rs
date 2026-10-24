//! Permission gating for the manox harness (host-layer policy).
//!
//! The kernel exposes the `requires_approval` seam on `AgentTool` but
//! deliberately ships no gate — permission policy is a harness concern. This
//! module is the manox harness's gate: pure mode-based allow/deny, fully
//! synchronous, no reviewer and no interactive approval round trip.
//!
//! - `FullAccess` runs every gated call ungated (bash unsandboxed).
//! - `Sandbox` runs a gated call when its target is provably inside the
//!   writable roots (`sandbox_verdict` for `Write`/`Edit`; sandbox-confined
//!   `Bash` bypass via the auto-allow resolver); anything the
//!   policy cannot classify is denied (fail-closed).
//!
//! Denials return a `ToolError` to the model — the gate never parks a call
//! on the user. The only round trip left on this channel is
//! `AskUserQuestion`, which is an interaction by design, not a permission
//! prompt.

use manox_harness::types::Model as HarnessModel;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use manox_harness::tool::{
    AgentTool as HarnessAgentTool, AgentToolResult, ExecutionMode, ToolContext, ToolError,
    ToolProgress,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::permission::{PendingAuthMeta, PermissionDecision, ToolAuthorizationResponse};
use crate::thread::{AccessMode, ThreadEvent};
use crate::thread_engine::{BackendNotice, send_notice};

/// A refused fs mutation: the sandbox-mode marker (the one vocabulary both
/// enforcing families teach) plus the same-turn escalation hint. Only the
/// sandbox mode denies — full access runs unfenced, so there is exactly one
/// denial word.
fn fs_denial() -> ToolError {
    ToolError::Other(format!(
        "{}\n{}",
        manox_harness::sandbox::sandbox_denial_marker(AccessMode::Sandbox),
        manox_harness::sandbox::escalation_hint_marker("operation")
    ))
}

/// A pending interaction parked on the user's answer (`AskUserQuestion`).
struct PendingAuth {
    tx: oneshot::Sender<ToolAuthorizationResponse>,
    meta: PendingAuthMeta,
}

/// Shared permission state for one thread: the mode, the pending
/// interaction round trips, and the event channel back to the UI. Lives in
/// the engine state so the tool wrappers (tokio side) and the facade's
/// respond path (gpui side) see one source of truth.
pub struct ApprovalGate {
    mode: Mutex<AccessMode>,
    /// The session's sandbox network switch (`networkAccess`). Memory-only
    /// cell — no object row, no sidecar field — so a restored session
    /// starts closed again; the AHP `session/configChanged` write is the
    /// only thing that opens it. The per-call resolvers (the bash
    /// seatbelt's route picker, the denial classifier) read this live.
    network: Mutex<manox_harness::sandbox::NetworkAccess>,
    /// The conversation's configuration authority. When installed it answers
    /// [`Self::mode`], so every per-call reader — this gate's own tool
    /// wrapper, the bash seatbelt's standing resolver, the Write/Edit
    /// escalation baseline — sees a mid-turn switch on the very next tool
    /// call. The `mode` mirror above remains the answer only for a gate with
    /// no source (bare constructions, tests).
    config_source: Mutex<Option<Arc<dyn manox_harness::ConversationConfigSource>>>,
    pending: Mutex<HashMap<String, PendingAuth>>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    model: Arc<Mutex<Option<HarnessModel>>>,
    /// K3 (L3): the engine actor's command sender, wired at spawn. The
    /// user's verdict on a parked card is an observable state change — it
    /// journals as an `approval` decision entry through the actor's
    /// serializer queue (the request entry rides the notice tap; the
    /// decision has no notice vocabulary of its own). `None` on a
    /// standalone gate (tests): verdicts simply do not journal.
    journal_sink: Mutex<Option<mpsc::UnboundedSender<crate::engine::SessionCmd>>>,
    /// D5 marker: `true` on a gate handed to a *delegated* (subagent) tool.
    /// The runtime root's gate is `false`. `AskUserQuestionTool` refuses a
    /// call on a delegated gate (`DELEGATED_CALLER`) so only the root can ask
    /// a human. Default `false` (main-line gate).
    delegated: bool,
}

impl ApprovalGate {
    pub fn new(
        notice_tx: mpsc::UnboundedSender<BackendNotice>,
        model_slot: Arc<Mutex<Option<HarnessModel>>>,
    ) -> Self {
        Self {
            mode: Mutex::new(AccessMode::default()),
            network: Mutex::new(manox_harness::sandbox::NetworkAccess::default()),
            config_source: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            notice_tx,
            model: model_slot,
            journal_sink: Mutex::new(None),
            delegated: false,
        }
    }

    /// Install the conversation's configuration authority. From here on
    /// [`Self::mode`] answers from it, so the mode a tool call is gated on is
    /// whatever the conversation's row says at that instant — not a value
    /// cached when the request began.
    pub fn set_config_source(&self, source: Arc<dyn manox_harness::ConversationConfigSource>) {
        *self.config_source.lock().unwrap() = Some(source);
    }

    /// Mark this gate as belonging to a delegated (subagent) tool — the
    /// `DELEGATED_CALLER` signal `AskUserQuestionTool` refuses on.
    pub fn with_delegated(mut self, delegated: bool) -> Self {
        self.delegated = delegated;
        self
    }

    /// Whether this gate backs a delegated (subagent) caller rather than the
    /// runtime root.
    pub fn is_delegated(&self) -> bool {
        self.delegated
    }

    /// Wire the actor command sender that carries verdict journaling (K3).
    /// Set once by `spawn_engine`; the gate is constructed before the
    /// sender exists, hence the setter.
    pub(crate) fn set_journal_sink(&self, tx: mpsc::UnboundedSender<crate::engine::SessionCmd>) {
        *self.journal_sink.lock().unwrap() = Some(tx);
    }

    /// The mode governing the *next* tool call. With a configuration source
    /// installed this is a fresh read of the conversation's row, so a switch
    /// that lands mid-turn governs the very next call rather than the next
    /// turn.
    pub fn mode(&self) -> AccessMode {
        let source = self.config_source.lock().unwrap().clone();
        if let Some(mode) = source.as_ref().and_then(|source| source.access_mode()) {
            return mode;
        }
        *self.mode.lock().unwrap()
    }

    pub fn set_mode(&self, mode: AccessMode) {
        *self.mode.lock().unwrap() = mode;
    }

    /// The session's sandbox network state, read per call.
    pub fn network_access(&self) -> manox_harness::sandbox::NetworkAccess {
        *self.network.lock().unwrap()
    }

    pub fn set_network_access(&self, access: manox_harness::sandbox::NetworkAccess) {
        *self.network.lock().unwrap() = access;
    }

    /// The network face one confined call runs under: the switch mapped to
    /// its route (the managed-proxy face joins when a proxy is running).
    pub fn network_route(&self) -> crate::sandbox::NetworkRoute {
        crate::sandbox::NetworkRoute::from_access(self.network_access())
    }

    pub fn model(&self) -> Option<HarnessModel> {
        self.model.lock().unwrap().clone()
    }

    /// Live handle to the owner's model slot so dispatch-time readers (e.g.
    /// subagent spawn) inherit the current model, not an assembly snapshot.
    pub fn model_slot(&self) -> Arc<Mutex<Option<HarnessModel>>> {
        Arc::clone(&self.model)
    }

    pub(crate) fn emit(&self, event: ThreadEvent) {
        send_notice(
            &self.notice_tx,
            BackendNotice::Event(Box::new(event)),
            "approval-flow notice",
        );
    }

    /// Park a new interaction: stores the responder, returns the receiver
    /// the tool awaits.
    pub(crate) fn register(
        &self,
        id: &str,
        meta: PendingAuthMeta,
    ) -> oneshot::Receiver<ToolAuthorizationResponse> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id.to_string(), PendingAuth { tx, meta });
        rx
    }

    /// Re-park a replayed card only when the id is not already parked. The
    /// restore path re-parks the journal's unsettled rows, and a re-open
    /// racing a LIVE card must keep the live responder — displacing it
    /// would drop the user's verdict on the floor. There is no awaiting
    /// tool future behind a replayed park, so the settle receiver goes
    /// straight to the drop.
    pub(crate) fn register_if_absent(&self, id: &str, meta: PendingAuthMeta) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if pending.contains_key(id) {
            return false;
        }
        let (tx, _rx) = oneshot::channel();
        pending.insert(id.to_string(), PendingAuth { tx, meta });
        true
    }

    /// Drop a pending interaction without answering (turn cancelled).
    pub(crate) fn discard(&self, id: &str) {
        // K3 (L3): a cancelled card journals its decision too — the
        // `pending_auth` fold would otherwise keep a card alive that no
        // client can ever answer (§C.2: the decision closes the fold).
        if let Some(pending) = self.pending.lock().unwrap().remove(id) {
            self.journal_decision(id, &pending.meta.tool_name, "cancelled");
        }
    }

    /// Settle a pending interaction: deliver the user's answer, or an
    /// explicit non-answer (`AskUserQuestionExpired`). Unknown ids are
    /// ignored (already settled) — the first settle wins.
    pub fn respond(&self, id: &str, response: ToolAuthorizationResponse) {
        if let Some(pending) = self.pending.lock().unwrap().remove(id) {
            // K3 (L3): the verdict is an observable state change — it
            // journals as the `approval` decision entry (§C.2 dual state)
            // through the actor's serializer queue.
            let verdict = match &response {
                ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce) => "allow_once",
                ToolAuthorizationResponse::Decision(PermissionDecision::Deny) => "deny",
                ToolAuthorizationResponse::AskUserQuestion { .. } => "answered",
                ToolAuthorizationResponse::AskUserQuestionExpired => "expired",
                ToolAuthorizationResponse::AskUserQuestionDismissed => "dismissed",
            };
            self.journal_decision(id, &pending.meta.tool_name, verdict);
            let _ = pending.tx.send(response);
        }
    }

    /// Queue the `approval` decision entry onto the engine actor (K3).
    /// Without a sink (a standalone gate) or with the actor already gone
    /// the decision does not journal: a gate with no engine has no session
    /// to append to and no facade left to hear about it.
    fn journal_decision(&self, id: &str, tool_name: &str, verdict: &str) {
        let Some(tx) = self.journal_sink.lock().unwrap().clone() else {
            return;
        };
        let _ = tx.send(crate::engine::SessionCmd::AppendJournal {
            kind: "approval".into(),
            payload: serde_json::json!({
                "kind": "decision",
                "authId": id,
                "payload": { "toolName": tool_name, "verdict": verdict },
            }),
        });
    }

    /// Snapshot of pending interactions for card re-surfacing.
    pub fn pending_entries(&self) -> Vec<(String, PendingAuthMeta)> {
        self.pending
            .lock()
            .unwrap()
            .iter()
            .map(|(id, p)| (id.clone(), p.meta.clone()))
            .collect()
    }
}

/// Host `EscalationApprover` over the `ApprovalGate`: parks a
/// `require_escalated` escalation on the user via the same
/// `ToolCallAuthorization` round-trip `AskUserQuestion` uses, mapping the
/// decision to the closed escalation outcome vocabulary.
pub struct GateEscalationApprover {
    gate: Arc<ApprovalGate>,
}

impl GateEscalationApprover {
    pub fn new(gate: Arc<ApprovalGate>) -> Self {
        Self { gate }
    }
}

#[async_trait::async_trait]
impl manox_harness::sandbox::EscalationApprover for GateEscalationApprover {
    async fn request(
        &self,
        req: manox_harness::sandbox::EscalationRequest,
    ) -> manox_harness::sandbox::EscalationOutcome {
        let mode = req.requested_mode;
        let tool_name = req.tool_name.clone();
        let reason = format!("escalate sandbox to {}: {}", mode.wire(), req.justification);
        let input = serde_json::json!({
            "require_escalated": true,
            "justification": req.justification,
        });
        let rx = self.gate.register(
            &req.call_id,
            PendingAuthMeta {
                tool_name: tool_name.clone(),
                summary: reason.clone(),
                input: input.clone(),
            },
        );
        self.gate.emit(ThreadEvent::ToolCallAuthorization {
            id: req.call_id.clone(),
            tool_name,
            summary: reason,
            input,
        });
        let response = match req.signal {
            Some(signal) => tokio::select! {
                r = rx => r.unwrap_or(ToolAuthorizationResponse::Decision(PermissionDecision::Deny)),
                _ = signal.cancelled() => {
                    self.gate.discard(&req.call_id);
                    return manox_harness::sandbox::EscalationOutcome::Cancelled;
                }
            },
            None => rx.await.unwrap_or(ToolAuthorizationResponse::Decision(
                PermissionDecision::Deny,
            )),
        };
        self.gate.discard(&req.call_id);
        match response {
            ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce) => {
                manox_harness::sandbox::EscalationOutcome::AllowedOnce
            }
            _ => manox_harness::sandbox::EscalationOutcome::Rejected,
        }
    }
}

// ── The gating wrapper ──────────────────────────────────────────────────────

/// Wraps a kernel tool with the host's permission policy. Tools that neither
/// declare `requires_approval` nor mutate anything pass straight through.
pub struct ApprovalGatedTool {
    inner: Arc<dyn HarnessAgentTool>,
    gate: Arc<ApprovalGate>,
    auto_allow: Option<AutoAllowResolver>,
    /// Optional sandbox-escalation config (Write/Edit): resolves a
    /// `require_escalated` grant through the host approver before the
    /// mode check, so an approved full-access grant widens the verdict for
    /// one call. The grant is a per-call local value (no shared cell —
    /// Write/Edit run `Parallel`, a shared cell would race). Bash resolves
    /// its own escalation inside the tool.
    escalation_approver: Option<Arc<dyn manox_harness::sandbox::EscalationApprover + Send + Sync>>,
    mode_resolver: Option<Arc<dyn Fn() -> AccessMode + Send + Sync>>,
    /// Session-shared extra writable roots, derived per call from the call's
    /// effective cwd: a linked worktree of the workspace admits itself
    /// automatically, any other directory is admitted by an approved
    /// escalation and accumulates for the session. Shared with the bash
    /// seatbelt — one derivation, no asymmetry between the two enforcing
    /// families. Absent in bare constructions (tests) — no extra roots.
    granted_roots: Option<crate::granted_roots::GrantedRoots>,
}

pub type AutoAllowResolver = Arc<dyn Fn(&str, &serde_json::Value) -> bool + Send + Sync>;

impl ApprovalGatedTool {
    pub fn new(inner: Arc<dyn HarnessAgentTool>, gate: Arc<ApprovalGate>) -> Self {
        Self {
            inner,
            gate,
            auto_allow: None,
            escalation_approver: None,
            mode_resolver: None,
            granted_roots: None,
        }
    }

    pub fn with_auto_allow(mut self, resolver: AutoAllowResolver) -> Self {
        self.auto_allow = Some(resolver);
        self
    }

    /// Attach the sandbox-escalation config (Write/Edit): the host approver +
    /// the standing-mode resolver. When the model passes
    /// `require_escalated`+`justification`, the gate resolves full access
    /// through the approver and the returned per-call grant widens the
    /// verdict for that one call (no shared cell — Write/Edit are parallel).
    pub fn with_escalation(
        mut self,
        approver: Arc<dyn manox_harness::sandbox::EscalationApprover + Send + Sync>,
        mode_resolver: Arc<dyn Fn() -> AccessMode + Send + Sync>,
    ) -> Self {
        self.escalation_approver = Some(approver);
        self.mode_resolver = Some(mode_resolver);
        self
    }

    /// Attach the session-shared granted-roots store (Write/Edit): the
    /// verdict admits targets under the same-repo worktree roots of the
    /// call's cwd plus escalation-approved roots — mirroring the seatbelt's
    /// additive roots exactly.
    pub fn with_granted_roots(mut self, granted: crate::granted_roots::GrantedRoots) -> Self {
        self.granted_roots = Some(granted);
        self
    }

    /// Host gate set: the kernel's declarative hint OR any mutating tool
    /// (write/edit/bash gated, reads free). Read-only tools and tools the
    /// kernel marks approval-free run ungated.
    fn needs_gate(&self, params: &serde_json::Value) -> bool {
        if self
            .auto_allow
            .as_ref()
            .is_some_and(|allow| allow(self.inner.name(), params))
        {
            return false;
        }
        self.inner.requires_approval(params) || !self.inner.is_read_only()
    }

    async fn delegate(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
        progress: Option<&dyn ToolProgress>,
    ) -> Result<AgentToolResult, ToolError> {
        match progress {
            Some(progress) => {
                self.inner
                    .execute_with_progress(tool_call_id, params, signal, ctx, progress)
                    .await
            }
            None => self.inner.execute(tool_call_id, params, signal, ctx).await,
        }
    }

    async fn run_gated(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
        progress: Option<&dyn ToolProgress>,
    ) -> Result<AgentToolResult, ToolError> {
        if !self.needs_gate(&params) {
            return self
                .delegate(tool_call_id, params, signal, ctx, progress)
                .await;
        }
        // Resolve a one-shot sandbox-escalation grant (Write/Edit) before the
        // mode check; an approved full-access grant widens the verdict for
        // this call only — a per-call local value, not a shared cell
        // (Write/Edit run `Parallel`; a shared cell would race between
        // concurrent calls). Bash resolves its own escalation inside the tool
        // and never reaches here (auto-allowed).
        let standing = self.standing_mode();
        let grant = self
            .resolve_escalation(tool_call_id, &signal, &params, standing, ctx)
            .await?;
        match grant.unwrap_or(standing) {
            // Full access runs every gated call ungated.
            AccessMode::FullAccess => {
                self.delegate(tool_call_id, params, signal, ctx, progress)
                    .await
            }
            // Sandbox enters the verdict: the call runs when its target is
            // provably inside the writable roots, denied otherwise.
            AccessMode::Sandbox => {
                self.sandbox_verdict(&params, ctx)?;
                self.delegate(tool_call_id, params, signal, ctx, progress)
                    .await
            }
        }
    }

    /// The standing session mode (absent any per-call grant).
    fn standing_mode(&self) -> AccessMode {
        self.mode_resolver
            .as_ref()
            .map(|r| r())
            .unwrap_or_else(|| self.gate.mode())
    }

    /// Validate the `require_escalated`+`justification` pairing and, when
    /// set, resolve full access through the host approver, returning it as a
    /// per-call grant (no shared state). `None` means no escalation was
    /// requested. A non-widening or unapproved request returns the verbatim
    /// error (fail-closed). The requested mode is always full access — with
    /// two tiers it is the only widening edge.
    async fn resolve_escalation(
        &self,
        tool_call_id: &str,
        signal: &CancellationToken,
        params: &serde_json::Value,
        standing: AccessMode,
        ctx: &dyn ToolContext,
    ) -> Result<Option<AccessMode>, ToolError> {
        let escalate = params
            .get("require_escalated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let just = params.get("justification").and_then(|v| v.as_str());
        manox_harness::sandbox::validate_escalation_args(escalate, just)
            .map_err(ToolError::InvalidArguments)?;
        if !escalate {
            return Ok(None);
        }
        let approver = self.escalation_approver.as_ref().ok_or_else(|| {
            ToolError::Other(
                "sandbox escalation requires approval, but no approval service is composed".into(),
            )
        })?;
        let grant = manox_harness::sandbox::approve_escalation(
            manox_harness::sandbox::EscalationRequest {
                requested_mode: AccessMode::FullAccess,
                justification: just.unwrap().to_string(),
                effective_mode: standing,
                subject: "operation".into(),
                tool_name: self.inner.name().to_string(),
                call_id: tool_call_id.to_string(),
                signal: Some(signal.clone()),
            },
            Some(approver.as_ref()),
        )
        .await
        .map_err(ToolError::Other)?;
        // An approved escalation covers a concrete out-of-workspace target;
        // its nearest existing ancestor directory joins the session's
        // granted roots, so the model never re-escalates for the same
        // directory (approve once, accumulate).
        if let Some(root) = self.call_target_root(params, ctx)
            && let Some(granted) = &self.granted_roots
        {
            granted.approve(root);
        }
        Ok(Some(grant))
    }

    /// The nearest existing ancestor directory of the call's write target —
    /// the root an approved escalation covers. Resolved with the same chain
    /// the tool itself will use (the `cwd` `[anchor, ...route]` array, peeked
    /// read-only so the fence neither advances the sticky cwd nor re-anchors
    /// it).
    fn call_target_root(
        &self,
        params: &serde_json::Value,
        ctx: &dyn ToolContext,
    ) -> Option<std::path::PathBuf> {
        let cwd =
            manox_harness::tools::path_utils::peek_effective_cwd(ctx, params.get("cwd")).ok()?;
        let target = match self.inner.name() {
            "Write" => {
                let path = params.get("path")?.as_str()?;
                if Path::new(path).is_absolute() {
                    std::path::PathBuf::from(path)
                } else {
                    cwd.join(path)
                }
            }
            "Edit" => {
                let patch = params.get("patch")?.as_str()?;
                let first = manox_harness::hashline::parse_patch(patch)
                    .ok()?
                    .files
                    .into_iter()
                    .next()?;
                if first.path.is_absolute() {
                    first.path
                } else {
                    cwd.join(first.path)
                }
            }
            _ => return None,
        };
        Some(nearest_existing_ancestor(&target))
    }

    /// Sandbox verdict for one gated call: `Ok` when the operation
    /// target is provably inside the writable roots, `Err` (model-facing
    /// reason) otherwise. Fail-closed: any gated call the policy cannot
    /// classify is denied. Sandbox-confined bash never reaches this check
    /// (its auto-allow resolver bypasses the gate entirely).
    fn sandbox_verdict(
        &self,
        params: &serde_json::Value,
        ctx: &dyn ToolContext,
    ) -> Result<(), ToolError> {
        let deny = || Err(fs_denial());
        // The call's effective cwd — the same `cwd` `[anchor, ...route]`
        // resolution the tool itself will perform, peeked read-only so the
        // fence does not advance the sticky cwd. A malformed or unresolvable
        // `cwd` is an argument error, not a permission problem: surface the
        // same instructional text the tool itself would return next instead
        // of dressing it up as a sandbox denial that lures the model into an
        // escalation it cannot benefit from.
        let cwd = match manox_harness::tools::path_utils::peek_effective_cwd(ctx, params.get("cwd"))
        {
            Ok(cwd) => cwd,
            Err(e) => return Err(ToolError::InvalidArguments(e)),
        };
        // Containment against the shared writable-root set (workspace + manox
        // home + /tmp + tmpdir) plus the granted roots derived from the
        // call's cwd: a linked worktree of the workspace admits itself, an
        // escalation-approved directory stays for the session — no `.git` or
        // plans-dir special-casing: the plans dir is admitted transitively
        // under the manox home.
        let mut roots = manox_harness::sandbox::writable_roots(AccessMode::Sandbox, &cwd);
        if let Some(granted) = &self.granted_roots {
            roots.extend(granted.roots_for(&cwd));
        }
        let contained = |target: &Path| {
            let canon = manox_harness::sandbox::canonicalize_best_effort(target);
            roots.iter().any(|r| canon.starts_with(r))
        };
        match self.inner.name() {
            "Write" => {
                let Some(path) = params.get("path").and_then(|v| v.as_str()) else {
                    return deny();
                };
                let target = if Path::new(path).is_absolute() {
                    std::path::PathBuf::from(path)
                } else {
                    cwd.join(path)
                };
                if !contained(&target) {
                    return deny();
                }
                Ok(())
            }
            "Edit" => {
                let Some(patch) = params.get("patch").and_then(|v| v.as_str()) else {
                    return deny();
                };
                let file_patches = match manox_harness::hashline::parse_patch(patch) {
                    Ok(p) => p.files,
                    // Unverifiable targets fail closed (the Edit tool
                    // rejects malformed hashline anyway).
                    Err(_) => return deny(),
                };
                for fp in &file_patches {
                    let target = if fp.path.is_absolute() {
                        fp.path.clone()
                    } else {
                        cwd.join(&fp.path)
                    };
                    if !contained(&target) {
                        return deny();
                    }
                }
                Ok(())
            }
            // Py: a script's write targets are decided at runtime, so
            // containment cannot be proven the way Write/Edit prove a path —
            // and unlike sandbox-confined Bash there is no OS fence to stand
            // in (the interpreter runs in-process, with the app's full user
            // privileges). The call fails closed with the escalation hint;
            // the assembly line arms this wrapper with the escalation
            // config, so an approved `require_escalated`+`justification`
            // grant widens the mode for exactly that call.
            crate::tools::PY => deny(),
            // Session/repo-scoped coordination (task tools) was removed in
            // the tools-optimization cycle — they were retired with the
            // Steer-based team architecture.
            // Every other gated call (escalated bash, unknown mutating
            // tools) has no in-workspace proof.
            _ => deny(),
        }
    }
}

/// The nearest ancestor of `target` that exists on disk. An escalation
/// covers a directory the model can keep working in — a not-yet-created
/// target contributes its closest existing parent instead.
fn nearest_existing_ancestor(target: &Path) -> PathBuf {
    let mut current = target.to_path_buf();
    while !current.is_dir() {
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return target.to_path_buf(),
        }
    }
    current
}

#[async_trait::async_trait]
impl HarnessAgentTool for ApprovalGatedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let mut schema = self.inner.parameters_schema();
        // Advertise the escalation fields on the wrapping tool (Write/Edit)
        // when a sandbox backend is mounted, so the model knows the denied
        // call can be retried with `require_escalated`+`justification`.
        if self.escalation_approver.is_some()
            && let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut())
        {
            props.insert(
                "require_escalated".into(),
                serde_json::json!({
                    "type": "boolean",
                    "description": "Set true to retry this exact operation with full access (no sandbox). Only valid as a one-shot retry of an operation the sandbox just denied; requires justification and user approval."
                }),
            );
            props.insert(
                "justification".into(),
                serde_json::json!({
                    "type": "string",
                    "description": "Required with require_escalated: one sentence for the user explaining why this exact file operation needs full access."
                }),
            );
        }
        schema
    }

    fn requires_approval(&self, params: &serde_json::Value) -> bool {
        self.needs_gate(params)
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.inner.execution_mode()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        self.run_gated(tool_call_id, params, signal, ctx, None)
            .await
    }

    async fn execute_with_progress(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
        progress: &dyn ToolProgress,
    ) -> Result<AgentToolResult, ToolError> {
        self.run_gated(tool_call_id, params, signal, ctx, Some(progress))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::AskAnswer;
    use manox_harness::env::TokioExecutionEnv;
    use manox_harness::tool::{LocalToolContext, ToolState};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The one denial word under two tiers: only sandbox mode denies, so the
    /// model-facing marker is a constant fact.
    fn sandbox_denial() -> String {
        manox_harness::sandbox::sandbox_denial_marker(AccessMode::Sandbox)
    }

    fn gate() -> Arc<ApprovalGate> {
        let (tx, _rx) = mpsc::unbounded_channel();
        Arc::new(ApprovalGate::new(tx, Arc::new(Mutex::new(None))))
    }

    fn gate_with_events() -> (Arc<ApprovalGate>, mpsc::UnboundedReceiver<BackendNotice>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(ApprovalGate::new(tx, Arc::new(Mutex::new(None)))),
            rx,
        )
    }

    fn tool_ctx() -> LocalToolContext {
        LocalToolContext::new(
            Arc::new(TokioExecutionEnv::new(std::env::temp_dir())),
            std::env::temp_dir(),
            Arc::new(ToolState::new()),
        )
    }

    struct MockTool {
        name: &'static str,
        approval: bool,
        read_only: bool,
        ran: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl HarnessAgentTool for MockTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "mock"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn requires_approval(&self, _params: &serde_json::Value) -> bool {
            self.approval
        }
        fn is_read_only(&self) -> bool {
            self.read_only
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: serde_json::Value,
            _signal: CancellationToken,
            _ctx: &dyn ToolContext,
        ) -> Result<AgentToolResult, ToolError> {
            self.ran.fetch_add(1, Ordering::SeqCst);
            Ok(AgentToolResult::text("ran"))
        }
    }

    fn gated_named(
        name: &'static str,
        approval: bool,
        read_only: bool,
        gate: Arc<ApprovalGate>,
    ) -> (ApprovalGatedTool, Arc<AtomicUsize>) {
        let ran = Arc::new(AtomicUsize::new(0));
        let tool = ApprovalGatedTool::new(
            Arc::new(MockTool {
                name,
                approval,
                read_only,
                ran: Arc::clone(&ran),
            }),
            gate,
        );
        (tool, ran)
    }

    fn gated(
        approval: bool,
        read_only: bool,
        gate: Arc<ApprovalGate>,
    ) -> (ApprovalGatedTool, Arc<AtomicUsize>) {
        gated_named("Mock", approval, read_only, gate)
    }

    #[test]
    fn gate_register_respond_round_trip() {
        let gate = gate();
        let rx = gate.register(
            "call-1",
            PendingAuthMeta {
                tool_name: "AskUserQuestion".into(),
                summary: "q".into(),
                input: serde_json::json!({}),
            },
        );
        assert_eq!(gate.pending_entries().len(), 1);
        gate.respond(
            "call-1",
            ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce),
        );
        assert!(gate.pending_entries().is_empty());
        let mut rx = rx;
        let response = rx.try_recv().unwrap();
        assert!(matches!(
            response,
            ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce)
        ));
        // Unknown ids are silently ignored.
        gate.respond(
            "nope",
            ToolAuthorizationResponse::Decision(PermissionDecision::Deny),
        );
    }

    #[test]
    fn gate_mode_switches() {
        let gate = gate();
        assert_eq!(gate.mode(), AccessMode::Sandbox);
        gate.set_mode(AccessMode::FullAccess);
        assert_eq!(gate.mode(), AccessMode::FullAccess);
    }

    #[tokio::test]
    async fn read_only_tools_pass_through_ungated() {
        let (gate, mut rx) = gate_with_events();
        gate.set_mode(AccessMode::Sandbox);
        let (tool, ran) = gated(false, true, Arc::clone(&gate));
        let ctx = tool_ctx();
        let result = tool
            .execute("c1", serde_json::json!({}), CancellationToken::new(), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert!(rx.try_recv().is_err(), "no authorization event expected");
    }

    #[tokio::test]
    async fn full_access_delegates_gated_tools_without_prompting() {
        let (gate, mut rx) = gate_with_events();
        gate.set_mode(AccessMode::FullAccess);
        let (tool, ran) = gated(true, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        let result = tool
            .execute("c1", serde_json::json!({}), CancellationToken::new(), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn sandbox_allows_in_workspace() {
        let (gate, _rx) = gate_with_events();
        gate.set_mode(AccessMode::Sandbox);
        let (tool, ran) = gated_named("Write", false, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        // The tool ctx's cwd is the workspace root for the gate's policy.
        let target = ctx.cwd().join("src/ok.txt");
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"path": target, "cwd": [""], "content": "x"}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    /// Py has no statically knowable target set — a script decides at
    /// runtime what it touches — and no OS sandbox stands in (the
    /// interpreter runs in-process), so the sandbox mode fails it closed:
    /// it denies with the escalation hint (the assembly line arms the
    /// wrapper with the escalation config, so an approved
    /// `require_escalated` grant is the one wider road), and only full
    /// access runs the script.
    #[tokio::test]
    async fn py_fails_closed_in_sandbox_runs_in_full_access() {
        let (gate, _rx) = gate_with_events();
        gate.set_mode(AccessMode::Sandbox);
        let (tool, ran) = gated_named("Py", true, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        let params = serde_json::json!({"script": "print(1)", "cwd": [""]});
        let err = tool
            .execute("c1", params.clone(), CancellationToken::new(), &ctx)
            .await
            .unwrap_err();
        match err {
            ToolError::Other(message) => {
                assert!(message.contains("sandbox mode"), "{message}");
            }
            other => panic!("expected the sandbox refusal, got {other:?}"),
        }
        assert_eq!(ran.load(Ordering::SeqCst), 0, "the script never ran");

        gate.set_mode(AccessMode::FullAccess);
        let result = tool
            .execute("c3", params, CancellationToken::new(), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    /// An escalation-approved root widens the fs fence for the rest of the
    /// session: while the sticky cwd is an unrelated directory, a Write
    /// under the approved root passes the verdict. The target sits outside
    /// every default writable root so only the accumulated root can admit
    /// it.
    #[tokio::test]
    async fn sandbox_allows_approved_granted_roots() {
        let dir = tempfile::tempdir().unwrap();
        // Outside every default writable root (temp areas and the manox home
        // would admit it regardless); the mock tool never writes, so the
        // directory needs no on-disk existence.
        let approved = PathBuf::from("/usr/local/manox-cwd-per-call-fence-probe");
        let target = approved.join("probe.txt");
        let (gate, _rx) = gate_with_events();
        gate.set_mode(AccessMode::Sandbox);

        // With the granted-roots store holding the approved root: the write
        // passes.
        let ran = Arc::new(AtomicUsize::new(0));
        let granted = crate::granted_roots::GrantedRoots::new(dir.path());
        granted.approve(&approved);
        let tool = ApprovalGatedTool::new(
            Arc::new(MockTool {
                name: "Write",
                approval: false,
                read_only: false,
                ran: Arc::clone(&ran),
            }),
            Arc::clone(&gate),
        )
        .with_granted_roots(granted);
        let ctx = LocalToolContext::new(
            Arc::new(TokioExecutionEnv::new(dir.path())),
            dir.path().to_path_buf(),
            Arc::new(ToolState::new()),
        );
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"path": target, "cwd": [""], "content": "x"}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(ran.load(Ordering::SeqCst), 1);

        // Control: the same call without the store is denied (the session
        // cwd's default roots never cover the approved path).
        let (tool2, ran2) = gated_named("Write", false, false, Arc::clone(&gate));
        let err = tool2
            .execute(
                "c2",
                serde_json::json!({"path": target, "cwd": [""], "content": "x"}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains(&sandbox_denial()));
        assert_eq!(ran2.load(Ordering::SeqCst), 0);
    }

    /// A linked worktree of the session workspace admits itself through the
    /// per-call cwd: an explicit `cwd` pointing into a same-repo worktree
    /// passes the verdict for targets inside it, with no approval anywhere.
    #[tokio::test]
    async fn sandbox_admits_a_same_repo_worktree_via_call_cwd() {
        let base = tempfile::tempdir().unwrap();
        let repo = base.path().join("repo");
        let wt = base.path().join("wt");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("f"), "x").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(
            &repo,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "init",
            ],
        );
        run_git(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "-b", "probe"],
        );

        let granted = crate::granted_roots::GrantedRoots::new(&repo);
        let (gate, _rx) = gate_with_events();
        gate.set_mode(AccessMode::Sandbox);
        let ran = Arc::new(AtomicUsize::new(0));
        let tool = ApprovalGatedTool::new(
            Arc::new(MockTool {
                name: "Write",
                approval: false,
                read_only: false,
                ran: Arc::clone(&ran),
            }),
            Arc::clone(&gate),
        )
        .with_granted_roots(granted);
        let ctx = LocalToolContext::new(
            Arc::new(TokioExecutionEnv::new(repo.clone())),
            repo.clone(),
            Arc::new(ToolState::new()),
        );
        let target = wt.join("src/ok.txt");
        let result = tool
            .execute(
                "c1",
                serde_json::json!({
                    "path": target,
                    "cwd": [wt.to_string_lossy()],
                    "content": "x",
                }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            !result.is_error,
            "same-repo worktree admitted through the call cwd"
        );
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The manox state home is part of the sandbox writable scope:
    /// plan files (and other session state) write without escalation. The
    /// target resolves through the HOST home resolver while the writable
    /// roots resolve through the extension layer's — this test pins that the
    /// two agree.
    #[tokio::test]
    async fn sandbox_allows_manox_home_write() {
        let Ok(home) = crate::paths::manox_home() else {
            return; // no home dir to admit
        };
        let target = home.join("plans/gate-probe-plan.md");
        let (gate, _rx) = gate_with_events();
        gate.set_mode(AccessMode::Sandbox);
        let ctx = tool_ctx();

        let (tool, ran) = gated_named("Write", false, false, Arc::clone(&gate));
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"path": target, "cwd": [""], "content": "x"}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(ran.load(Ordering::SeqCst), 1);

        let (tool, ran) = gated_named("Edit", false, false, Arc::clone(&gate));
        let patch = format!(
            "*** Begin Patch\n[{}#1A2B3C]\nDEL 1\n*** End Patch",
            target.display()
        );
        let result = tool
            .execute(
                "c2",
                serde_json::json!({"patch": patch, "cwd": [""]}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn sandbox_denies_out_of_roots_write() {
        let (gate, mut rx) = gate_with_events();
        gate.set_mode(AccessMode::Sandbox);
        let (tool, ran) = gated_named("Write", false, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        let err = tool
            .execute(
                "c1",
                serde_json::json!({"path": "/etc/manox-gate-test/x.txt", "cwd": [""], "content": "x"}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains(&sandbox_denial()));
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert!(rx.try_recv().is_err(), "no authorization event on deny");
    }

    #[tokio::test]
    async fn sandbox_edit_patch_confines_targets() {
        let (gate, _rx) = gate_with_events();
        gate.set_mode(AccessMode::Sandbox);
        let (tool, ran) = gated_named("Edit", false, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        let ok_patch = "*** Begin Patch\n[src/ok.rs#1A2B3C]\nDEL 1\n*** End Patch";
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"patch": ok_patch, "cwd": [""]}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(ran.load(Ordering::SeqCst), 1);

        let escape_patch =
            "*** Begin Patch\n[/etc/manox-gate-test/bad.rs#1A2B3C]\nDEL 1\n*** End Patch";
        let err = tool
            .execute(
                "c2",
                serde_json::json!({"patch": escape_patch, "cwd": [""]}),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains(&sandbox_denial()));
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    /// Fail-closed: a gated mutating tool the path policy cannot classify
    /// (no path target) is denied under Sandbox. The `cwd` carries a
    /// well-formed anchor so the case isolates target classification — a
    /// missing/malformed `cwd` is an argument error, not a denial.
    #[tokio::test]
    async fn sandbox_denies_unclassifiable_targets() {
        let (gate, _rx) = gate_with_events();
        gate.set_mode(AccessMode::Sandbox);
        let (tool, ran) = gated(true, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        let err = tool
            .execute(
                "c1",
                serde_json::json!({ "cwd": [""] }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains(&sandbox_denial()));
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    /// A gated Write whose `cwd` is missing reaches the fence (the schema
    /// deliberately does not require the property) and must surface the
    /// instructional argument error — not a sandbox denial luring the model
    /// into an escalation — without running the tool.
    #[tokio::test]
    async fn sandbox_missing_cwd_surfaces_the_instructional_error() {
        let (gate, _rx) = gate_with_events();
        gate.set_mode(AccessMode::Sandbox);
        let (tool, ran) = gated_named("Write", true, false, Arc::clone(&gate));
        let ctx = tool_ctx();
        let err = tool
            .execute(
                "c1",
                serde_json::json!({ "path": "x.txt", "content": "y" }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains(manox_harness::tools::path_utils::CWD_REQUIRED_DOC),
            "{err}"
        );
        assert!(!err.to_string().contains(&sandbox_denial()));
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    fn mutating_gated() -> ApprovalGatedTool {
        ApprovalGatedTool::new(
            Arc::new(MockTool {
                name: "Mutating",
                approval: false,
                read_only: false,
                ran: Arc::new(AtomicUsize::new(0)),
            }),
            gate(),
        )
    }

    #[test]
    fn mutating_tools_reach_the_gate() {
        assert!(mutating_gated().needs_gate(&serde_json::json!({})));
    }

    #[test]
    fn explicit_auto_allow_preserves_sandboxed_bash_bypass_contract() {
        let tool = mutating_gated().with_auto_allow(Arc::new(|_, params| {
            !params["unsandboxed"].as_bool().unwrap_or(false)
        }));
        assert!(!tool.needs_gate(&serde_json::json!({"unsandboxed": false})));
        assert!(tool.needs_gate(&serde_json::json!({"unsandboxed": true})));
    }

    struct CannedEscalationApprover(manox_harness::sandbox::EscalationOutcome);

    #[async_trait::async_trait]
    impl manox_harness::sandbox::EscalationApprover for CannedEscalationApprover {
        async fn request(
            &self,
            _req: manox_harness::sandbox::EscalationRequest,
        ) -> manox_harness::sandbox::EscalationOutcome {
            self.0
        }
    }

    /// Regression: the per-call grant dies with its call. Call 1 escalates
    /// to full access and RUNS (full access is ungated); call 2 carries no
    /// escalation and must fall back to the standing sandbox mode and be
    /// DENIED — a leaked grant would have run it too.
    #[tokio::test]
    async fn escalation_grant_does_not_leak_to_the_next_call() {
        let (gate, _rx) = gate_with_events();
        gate.set_mode(AccessMode::Sandbox);
        let (tool, ran) = gated_named("Write", false, false, Arc::clone(&gate));
        let standing = Arc::clone(&gate);
        let mode_resolver: Arc<dyn Fn() -> AccessMode + Send + Sync> =
            Arc::new(move || standing.mode());
        let tool = tool.with_escalation(
            Arc::new(CannedEscalationApprover(
                manox_harness::sandbox::EscalationOutcome::AllowedOnce,
            )),
            mode_resolver,
        );
        let ctx = tool_ctx();
        // Call 1: escalate to full access for an out-of-roots target — the
        // approved grant runs the call.
        let args = serde_json::json!({
            "path": "/etc/manox-grant-leak-test/x.txt",
            "cwd": [""],
            "content": "x",
            "require_escalated": true,
            "justification": "need it"
        });
        let result = tool
            .execute("c1", args, CancellationToken::new(), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error, "the approved grant runs the call");
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        // Call 2: no escalation, same out-of-roots target — the standing
        // sandbox verdict denies it; the stale full-access grant would have
        // run it a second time.
        let args2 = serde_json::json!({
            "path": "/etc/manox-grant-leak-test/x.txt",
            "cwd": [""],
            "content": "x"
        });
        let err2 = tool
            .execute("c2", args2, CancellationToken::new(), &ctx)
            .await
            .unwrap_err();
        assert!(
            err2.to_string().contains(&sandbox_denial()),
            "call 2 denied by the standing sandbox verdict: {err2}"
        );
        assert_eq!(ran.load(Ordering::SeqCst), 1, "the stale grant never ran");
    }

    /// Parallel regression: an escalated Write/Edit call must not leak its
    /// grant to a concurrent non-escalated call (Write/Edit run `Parallel`).
    /// The grant is a per-call local value, so the escalated call runs while
    /// the plain call is denied by the standing sandbox verdict.
    #[tokio::test]
    async fn parallel_non_escalated_call_is_unaffected_by_in_flight_grant() {
        let (gate, _rx) = gate_with_events();
        gate.set_mode(AccessMode::Sandbox);
        let (tool, ran) = gated_named("Write", false, false, Arc::clone(&gate));
        let standing = Arc::clone(&gate);
        let mode_resolver: Arc<dyn Fn() -> AccessMode + Send + Sync> =
            Arc::new(move || standing.mode());
        let tool = Arc::new(tool.with_escalation(
            Arc::new(CannedEscalationApprover(
                manox_harness::sandbox::EscalationOutcome::AllowedOnce,
            )),
            mode_resolver,
        ));
        let ctx = tool_ctx();
        let escalated = serde_json::json!({
            "path": "/etc/manox-parallel-race/x.txt",
            "cwd": [""],
            "content": "x",
            "require_escalated": true,
            "justification": "need it"
        });
        let plain = serde_json::json!({
            "path": "/etc/manox-parallel-race/y.txt",
            "cwd": [""],
            "content": "y"
        });
        let (a, b) = tokio::join!(
            tool.execute("c1", escalated, CancellationToken::new(), &ctx),
            tool.execute("c2", plain, CancellationToken::new(), &ctx),
        );
        let a = a.unwrap();
        let b = b.unwrap_err();
        assert!(!a.is_error, "the escalated call runs at full access");
        // The plain call must be denied by the standing sandbox verdict, NOT
        // the in-flight full-access grant (no shared cell → no leak).
        assert!(
            b.to_string().contains(&sandbox_denial()),
            "plain fell back to the sandbox verdict: {b}"
        );
        assert_eq!(ran.load(Ordering::SeqCst), 1, "only the escalated call ran");
    }
    /// A mock tool whose schema carries a `properties` object (so the gate's
    /// escalation-field merge has somewhere to land).
    struct PropMock;
    #[async_trait::async_trait]
    impl HarnessAgentTool for PropMock {
        fn name(&self) -> &str {
            "Write"
        }
        fn description(&self) -> &str {
            "mock"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]})
        }
        fn requires_approval(&self, _params: &serde_json::Value) -> bool {
            false
        }
        fn is_read_only(&self) -> bool {
            false
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: serde_json::Value,
            _signal: CancellationToken,
            _ctx: &dyn ToolContext,
        ) -> Result<AgentToolResult, ToolError> {
            Ok(AgentToolResult::text("ran"))
        }
    }

    #[test]
    fn escalation_fields_advertised_on_wrapped_write_edit_schema() {
        let bare_gate = gate();
        let gate = gate();
        let standing = Arc::clone(&gate);
        let mode_resolver: Arc<dyn Fn() -> AccessMode + Send + Sync> =
            Arc::new(move || standing.mode());
        let tool = ApprovalGatedTool::new(Arc::new(PropMock), gate).with_escalation(
            Arc::new(CannedEscalationApprover(
                manox_harness::sandbox::EscalationOutcome::AllowedOnce,
            )),
            mode_resolver,
        );
        let schema = tool.parameters_schema();
        let props = schema["properties"]
            .as_object()
            .expect("schema has properties");
        assert!(props.contains_key("path"), "inner field preserved");
        assert!(
            props.contains_key("require_escalated"),
            "escalation field advertised"
        );
        assert!(
            props.contains_key("justification"),
            "escalation field advertised"
        );
        assert_eq!(
            props["require_escalated"]["type"],
            serde_json::json!("boolean"),
            "the escalation slot is a boolean flag, not an enum"
        );
        // Without escalation configured, the inner schema passes through bare.
        let bare = ApprovalGatedTool::new(Arc::new(PropMock), bare_gate);
        let bare_schema = bare.parameters_schema();
        let bare_props = bare_schema["properties"]
            .as_object()
            .expect("bare schema has properties");
        assert!(!bare_props.contains_key("require_escalated"));
    }

    /// K3 (L3): the user's verdict on a parked card journals as an
    /// `approval` decision entry through the actor's command queue —
    /// `respond` carries the verdict vocabulary (`allow_once` / `deny` /
    /// `answered` / `expired`), and `discard` (a cancelled turn) closes the card with
    /// a `cancelled` decision so the `pending_auth` fold never keeps a
    /// dead card alive. A settled card never journals twice.
    #[test]
    fn respond_and_discard_journal_approval_decisions() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<crate::engine::SessionCmd>();
        let (notice_tx, _notice_rx) = mpsc::unbounded_channel::<BackendNotice>();
        let gate = ApprovalGate::new(notice_tx, Arc::new(Mutex::new(None)));
        gate.set_journal_sink(cmd_tx);
        let meta = || PendingAuthMeta {
            tool_name: "Bash".into(),
            summary: "s".into(),
            input: serde_json::json!({}),
        };
        let next_decision = |cmd_rx: &mut mpsc::UnboundedReceiver<crate::engine::SessionCmd>,
                             auth_id: &str,
                             verdict: &str| {
            match cmd_rx.try_recv().expect("the verdict must journal") {
                crate::engine::SessionCmd::AppendJournal { kind, payload } => {
                    assert_eq!(kind, "approval");
                    assert_eq!(payload["kind"], serde_json::json!("decision"));
                    assert_eq!(payload["authId"], serde_json::json!(auth_id));
                    assert_eq!(payload["payload"]["toolName"], serde_json::json!("Bash"));
                    assert_eq!(payload["payload"]["verdict"], serde_json::json!(verdict));
                }
                _ => panic!("expected an AppendJournal row for {auth_id}"),
            }
        };

        let mut rx = gate.register("call-1", meta());
        gate.respond(
            "call-1",
            ToolAuthorizationResponse::Decision(PermissionDecision::AllowOnce),
        );
        next_decision(&mut cmd_rx, "call-1", "allow_once");
        // The responder still receives the answer (journaling never eats it).
        assert!(rx.try_recv().is_ok());

        let _rx = gate.register("call-2", meta());
        gate.respond(
            "call-2",
            ToolAuthorizationResponse::Decision(PermissionDecision::Deny),
        );
        next_decision(&mut cmd_rx, "call-2", "deny");

        let _rx = gate.register("call-3", meta());
        gate.respond(
            "call-3",
            ToolAuthorizationResponse::AskUserQuestion {
                answers: vec![AskAnswer::new("q-id".into(), vec!["yes".into()], None)],
            },
        );
        next_decision(&mut cmd_rx, "call-3", "answered");

        // R1: an adjudication that settled with no user input journals
        // `expired`, never a fake `answered`.
        let _rx = gate.register("call-3b", meta());
        gate.respond("call-3b", ToolAuthorizationResponse::AskUserQuestionExpired);
        next_decision(&mut cmd_rx, "call-3b", "expired");

        let _rx = gate.register("call-4", meta());
        gate.discard("call-4");
        next_decision(&mut cmd_rx, "call-4", "cancelled");

        // A settled card is gone from `pending`: neither a second respond
        // nor a second discard journals again.
        gate.discard("call-4");
        gate.respond(
            "call-4",
            ToolAuthorizationResponse::Decision(PermissionDecision::Deny),
        );
        assert!(
            cmd_rx.try_recv().is_err(),
            "a settled card must not journal a second decision"
        );
    }

    /// The restore path's re-park never displaces a LIVE park: a re-open
    /// racing a live card keeps the live responder, so the user's verdict
    /// reaches the awaiting tool future instead of dying with a dropped
    /// receiver.
    #[test]
    fn register_if_absent_keeps_the_live_park() {
        let meta = || PendingAuthMeta {
            tool_name: "Bash".into(),
            summary: "s".into(),
            input: serde_json::json!({}),
        };
        let gate = gate();
        let mut rx = gate.register("call-1", meta());
        assert!(
            !gate.register_if_absent("call-1", meta()),
            "a live park keeps its responder"
        );
        assert!(gate.register_if_absent("call-2", meta()));
        gate.respond(
            "call-1",
            ToolAuthorizationResponse::Decision(PermissionDecision::Deny),
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(ToolAuthorizationResponse::Decision(
                PermissionDecision::Deny
            ))
        ));
    }

    /// A stand-in for the conversation's configuration surface. The test
    /// flips `mode` between calls to stand in for the user's write landing
    /// mid-run; the real object-store implementation is covered by
    /// `conversation_config`'s own tests.
    struct FakeConfigSource {
        mode: Mutex<Option<AccessMode>>,
    }

    impl manox_harness::ConversationConfigSource for FakeConfigSource {
        fn model(&self) -> Option<manox_harness::types::Model> {
            None
        }

        fn model_ref(&self) -> Option<String> {
            None
        }

        fn access_mode(&self) -> Option<AccessMode> {
            *self.mode.lock().unwrap()
        }

        fn reasoning_effort(&self) -> Option<String> {
            None
        }

        fn config_version(&self) -> i64 {
            0
        }
    }

    fn gated_mutating_tool(
        gate: Arc<ApprovalGate>,
    ) -> (Arc<dyn HarnessAgentTool>, Arc<AtomicUsize>) {
        let ran = Arc::new(AtomicUsize::new(0));
        let inner: Arc<dyn HarnessAgentTool> = Arc::new(MockTool {
            name: "Write",
            approval: false,
            read_only: false,
            ran: Arc::clone(&ran),
        });
        (Arc::new(ApprovalGatedTool::new(inner, gate)), ran)
    }

    /// The permission gate reads the conversation's configuration before
    /// EVERY tool call: a mode the user flips mid-run governs the very next
    /// call, not the next turn. Without the live read the gate would keep the
    /// mode it was handed when the run began, and a user who notices the
    /// model heading somewhere dangerous would have to stop the run to be
    /// obeyed.
    #[tokio::test]
    async fn a_mode_flip_governs_the_very_next_tool_call() {
        let source = Arc::new(FakeConfigSource {
            mode: Mutex::new(Some(AccessMode::Sandbox)),
        });
        let gate = gate();
        gate.set_config_source(source.clone());
        // A stale mirror must not win over the source: this is the value the
        // gate was handed when the run began.
        gate.set_mode(AccessMode::FullAccess);

        let (tool, ran) = gated_mutating_tool(Arc::clone(&gate));
        let ctx = tool_ctx();
        // An out-of-roots target: denied under sandbox, admitted at full
        // access — the pair makes the governing mode observable.
        let args = serde_json::json!({
            "path": "/etc/manox-mode-flip-test/x.txt",
            "cwd": [""],
            "content": "x"
        });
        let err = tool
            .execute("c1", args.clone(), CancellationToken::new(), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains(&sandbox_denial()),
            "the source's sandbox governs the first call: {err}"
        );
        assert_eq!(ran.load(Ordering::SeqCst), 0);

        // The user's write lands while the run is in flight.
        *source.mode.lock().unwrap() = Some(AccessMode::FullAccess);

        let result = tool
            .execute("c2", args, CancellationToken::new(), &ctx)
            .await
            .expect("the next call must be admitted");
        assert!(!result.is_error);
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "the flipped mode governs the very next call"
        );
    }

    /// A gate with no source keeps the mirror as its answer — the bare
    /// constructions (subagent seats, tests) still work.
    #[tokio::test]
    async fn a_gate_without_a_source_keeps_its_mirror() {
        let gate = gate();
        gate.set_mode(AccessMode::Sandbox);
        let (tool, ran) = gated_mutating_tool(Arc::clone(&gate));
        let ctx = tool_ctx();
        let err = tool
            .execute(
                "c1",
                serde_json::json!({
                    "path": "/etc/manox-mirror-test/x.txt",
                    "cwd": [""],
                    "content": "x"
                }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains(&sandbox_denial()));
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }
}
