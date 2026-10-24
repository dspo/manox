//! The sandbox POLICY vocabulary: the per-call access mode, the writable-root
//! derivation shared by the bash seatbelt and the fs write fence, and the
//! `require_escalated` escalation contract.
//!
//! This is the extension-layer home so the bash tool and the host fs-fence
//! wrapper consume one vocabulary without a host
//! import — the extension layer must not depend back on the host. The host
//! re-exports `AccessMode` from `crate::thread` for session/persistence.
//!
//! Model-facing strings (markers, escalation errors) are English and never
//! pass through untranslated — the model reads them verbatim.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File-effect policy for confined bash and the fs write fence: the access
/// mode a call runs under. `sandbox` confines writes to the workspace root,
/// the manox state home (`~/.manox`), and platform temp areas; `full-access`
/// bypasses confinement. Persisted in the session sidecar (wire field
/// `access_mode`, kebab values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccessMode {
    /// Confined: bash rides the seatbelt profile and writes only under the
    /// writable roots; fs mutations are fenced to the same roots.
    #[default]
    Sandbox,
    /// No confinement; bash runs unsandboxed, fs mutations are unfenced.
    FullAccess,
}

/// Sentinel for "no per-call grant" on a shared grant cell. The host stamps an
/// approved escalation's mode as `as_i64()` for one call; `NO_GRANT` means the
/// standing session mode applies.
pub const NO_GRANT: i64 = i64::MIN;

impl AccessMode {
    /// The kebab wire string (the schema enum value and the marker vocabulary).
    pub fn wire(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::FullAccess => "full-access",
        }
    }

    /// Parse a wire string; `None` for anything outside the closed vocabulary.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "sandbox" => Some(Self::Sandbox),
            "full-access" => Some(Self::FullAccess),
            _ => None,
        }
    }

    /// Map the persisted i64; any unknown value lands on the single bounded
    /// default. 1 = sandbox, 2 = full-access.
    pub fn from_i64(v: i64) -> Self {
        match v {
            2 => Self::FullAccess,
            _ => Self::default(),
        }
    }

    pub fn as_i64(self) -> i64 {
        match self {
            Self::Sandbox => 1,
            Self::FullAccess => 2,
        }
    }
}

/// Validate the escalation argument pairing a tool schema cannot express:
/// `require_escalated: true` must travel with a `justification` — an approval
/// prompt without a reason is a malformed ask — and the justification must be
/// a non-empty sentence. A justification without `require_escalated` is
/// inert prose, not an error.
pub fn validate_escalation_args(
    require_escalated: bool,
    justification: Option<&str>,
) -> Result<(), String> {
    if require_escalated {
        let Some(j) = justification else {
            return Err("invalid escalation: require_escalated requires a justification".into());
        };
        if j.trim().is_empty() {
            return Err("invalid justification: expected a non-empty sentence".into());
        }
    }
    Ok(())
}

/// The model-facing denial marker — the one vocabulary both enforcing families
/// (bash and fs) teach and report, so the model recognizes a policy denial
/// identically whether the seatbelt refused a bash file effect or the fs fence
/// refused a mutation. Only the sandbox mode denies; full access runs unfenced.
pub fn sandbox_denial_marker(mode: AccessMode) -> String {
    format!("[sandbox: file access denied under {} mode]", mode.wire())
}

/// Substrings a sandboxed file-write denial leaves in the process output
/// stream, per backend: the macOS seatbelt's EPERM surfaces as "operation
/// not permitted", the Linux bwrap read-only base as EROFS ("read-only
/// file system"). Shared by the sandboxed exec backends (stderr of a
/// finished child) and the registry-executed foreground path (combined
/// output after the exit is recorded).
pub const SANDBOX_DENIAL_SIGNATURES: &[&str] =
    &["operation not permitted", "read-only file system"];

/// Post-exit denial classification: when a nonzero-exit command's output
/// carries a sandbox denial signature, the marker + same-turn escalation
/// hint is returned so the model recognizes a policy refusal (not a command
/// bug) and can retry with `require_escalated`.
pub fn seatbelt_denial_note(output: &str, exit_code: i32, mode: AccessMode) -> Option<String> {
    if exit_code == 0 {
        return None;
    }
    let lower = output.to_ascii_lowercase();
    if SANDBOX_DENIAL_SIGNATURES
        .iter()
        .any(|sig| lower.contains(sig))
    {
        Some(format!(
            "\n{}\n{}",
            sandbox_denial_marker(mode),
            escalation_hint_marker("command")
        ))
    } else {
        None
    }
}

/// The same-turn escalation hint that rides a denial: the nudge lives at the
/// decision point so the sanctioned retry does not depend on the model
/// recalling the tool description. `subject` is the family's noun for the
/// denied action (`"command"` for bash, `"operation"` for a fs mutation).
pub fn escalation_hint_marker(subject: &str) -> String {
    format!(
        "[sandbox: escalation available — retry this exact {subject} once with `require_escalated: \
         true` + `justification` (full access); the approval prompt asks the user]"
    )
}

/// Resolve a granted root to the path the enforcement layer actually compares:
/// canonical (symlinks resolved), because `/tmp` IS `/private/tmp` on darwin
/// and an as-spelled grant would match nothing. Falls back to the spelling
/// when resolution fails (a missing root matches nothing until it exists — the
/// conservative outcome; inventing a fallback would grant a path the caller
/// never named).
pub fn canonicalize_best_effort(path: &Path) -> PathBuf {
    // Walk up to the deepest existing ancestor, collecting the missing tail.
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if existing.exists() {
            break;
        }
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                if !existing.pop() {
                    return normalize_lexical(path);
                }
            }
            None => return normalize_lexical(path),
        }
    }
    let mut base = existing.canonicalize().unwrap_or_else(|_| existing.clone());
    // Re-append the tail in original order, folding `.`/`..` against the
    // accumulated base so traversal cannot survive into the classified path.
    for component in tail.into_iter().rev() {
        if component == "." || component.is_empty() {
            continue;
        }
        if component == ".." {
            let _ = base.pop(); // pop stops at the root
            continue;
        }
        base.push(component);
    }
    base
}

/// Purely lexical `.`/`..` folding for paths with no existing ancestor
/// (relative inputs, or everything missing). `..` past the root is dropped.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                let _ = out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// `$HOME/.manox` — the manox state home (plans, settings, sessions, agent
/// definitions), part of the `sandbox` writable scope so session state such
/// as plan files needs no escalation. `None` when the home dir
/// does not resolve — the host then places its state under the process cwd,
/// which the workspace root already admits. The host resolves its home from
/// `$HOME` (cwd fallback), this uses `dirs::home_dir()` (passwd fallback) —
/// the only divergence is with `$HOME` unset, where this admits the
/// passwd-home `.manox`, a lexical root matching nothing unless it exists.
pub fn manox_home() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".manox"))
}

/// The roots one confined execution may WRITE under — the sandbox mode's
/// meaning as a canonical, deduplicated allow-list: the policy's workspace
/// root, the manox state home ([`manox_home`] — plan files and other session
/// state write without escalation), the host `/tmp`, and the per-user
/// platform temp dir (`std::env::temp_dir()` — the real temp area; omitting
/// it would deny what the mode promises). `full-access` is never fenced, so
/// it has no root list (enforcement skips this function entirely for it).
/// Shared by the bash seatbelt and the fs fence so the "the write tool
/// cannot write `/tmp` but bash can" asymmetry cannot arise.
pub fn writable_roots(mode: AccessMode, workspace_root: &Path) -> Vec<PathBuf> {
    if mode != AccessMode::Sandbox {
        return Vec::new();
    }
    let mut candidates = vec![
        workspace_root.to_path_buf(),
        PathBuf::from("/tmp"),
        std::env::temp_dir(),
    ];
    candidates.extend(manox_home());
    let mut roots: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        let canon = canonicalize_best_effort(&candidate);
        if !roots.iter().any(|r| r == &canon) {
            roots.push(canon);
        }
    }
    roots
}

/// The conventional scratch locations distinct from `$TMPDIR` (the per-user
/// temp dir is already in [`writable_roots`]; these are the shared `/tmp`
/// spools).
pub fn temp_scratch_roots() -> [PathBuf; 2] {
    [PathBuf::from("/tmp"), PathBuf::from("/private/tmp")]
}

/// Whether `path` falls under a temp-scratch root. Canonicalizes first so
/// `/tmp` classifies correctly on macOS (where it is a symlink to
/// `/private/tmp`); `PathBuf::starts_with` is component-based, so a sibling
/// like `/tmp-evil` never matches.
pub fn is_temp_scratch(path: &Path) -> bool {
    let canon = canonicalize_best_effort(path);
    temp_scratch_roots()
        .iter()
        .any(|root| canon.starts_with(root))
}

/// The closed outcome vocabulary of one escalation ask — structurally identical
/// to the host approval seam's `PermissionDecision` so the host impl maps
/// without translation.
#[derive(Clone, Copy)]
pub enum EscalationOutcome {
    /// The user approved the wider mode for this one call.
    AllowedOnce,
    /// The user explicitly rejected the escalation.
    Rejected,
    /// The approval was cancelled (e.g. the turn was aborted).
    Cancelled,
    /// No approval channel is available (the approver exists but its channel
    /// is down — a rare composition/runtime failure). The host
    /// `GateEscalationApprover` always has a channel, so this is not produced
    /// in manox today; the `unavailable` outcome is only returned when an
    /// existing approver reports its channel unavailable — a missing approver
    /// is a fatal `Err`.
    Unavailable,
}

/// One escalation request, as [`approve_escalation`] judges it.
pub struct EscalationRequest {
    /// The requested target mode — with two tiers this is always
    /// [`AccessMode::FullAccess`] (the only widening edge).
    pub requested_mode: AccessMode,
    /// The model's one-sentence reason, shown verbatim to the user.
    pub justification: String,
    /// The call's effective mode (session override or standing default) the
    /// request must strictly widen.
    pub effective_mode: AccessMode,
    /// The family's noun for the escalated action in user-facing texts
    /// (`"command"` for bash, `"operation"` for fs).
    pub subject: String,
    /// The originating tool name (e.g. `"Bash"`, `"Write"`, `"Edit"`) recorded
    /// on the approval audit trail + card.
    pub tool_name: String,
    /// The tool-call id the approval prompt attaches to (the host's approval
    /// gate keys its pending round-trip off this).
    pub call_id: String,
    /// The tool-execution abort signal; the host parks the approval await on
    /// it so a cancelled turn frees the pending entry.
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

/// Minimal approval-request shape — a structural function the host closes over
/// its `ApprovalGate`. The extension layer resolves escalations through this
/// without importing the host's approval types.
#[async_trait::async_trait]
pub trait EscalationApprover: Send + Sync {
    /// Ask the human to approve one action, resolving to a closed outcome.
    async fn request(&self, req: EscalationRequest) -> EscalationOutcome;
}

/// Resolve a sandbox-escalation request BEFORE anything executes: check strict
/// widening against the call's effective mode, then resolve the approval
/// channel, then map every outcome — the ordered fail-closed sequence both
/// enforcing families share. Returns the granted mode to stamp onto exactly
/// this call; returns the distinct verbatim text for every other path (a
/// narrowing request, a missing approval service, a rejection, a
/// cancellation, an unanswerable ask). A same-level request is an idempotent
/// no-op grant — the call already runs at that mode — and never prompts a
/// human; a narrowing one fails, because escalation only widens. With two
/// tiers the only widening edge is sandbox → full access.
pub async fn approve_escalation(
    request: EscalationRequest,
    approver: Option<&dyn EscalationApprover>,
) -> Result<AccessMode, String> {
    let mode = request.requested_mode;
    let subject = request.subject.clone();
    if mode == request.effective_mode {
        return Ok(mode);
    }
    // Strict widening is an execution check against the call's effective
    // mode: sandbox → full access is the single edge; anything else
    // narrows (full access has nowhere to go).
    if !(request.effective_mode == AccessMode::Sandbox && mode == AccessMode::FullAccess) {
        return Err(format!(
            "sandbox escalation to \"{}\" is not strictly wider than this call's current \"{}\" mode. \
             Escalation only widens: omit `require_escalated` to run at the current mode",
            mode.wire(),
            request.effective_mode.wire()
        ));
    }
    let approver = approver.ok_or_else(|| {
        format!(
            "sandbox escalation to \"{}\" requires approval, but no approval service is composed",
            mode.wire()
        )
    })?;
    match approver.request(request).await {
        EscalationOutcome::AllowedOnce => Ok(mode),
        EscalationOutcome::Rejected => Err(format!(
            "the user rejected escalating this {subject} to \"{}\"",
            mode.wire()
        )),
        EscalationOutcome::Cancelled => Err(format!(
            "approval for escalating to \"{}\" was cancelled",
            mode.wire()
        )),
        EscalationOutcome::Unavailable => Err(format!(
            "sandbox escalation to \"{}\" requires approval, but no approval channel is available",
            mode.wire()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_roots_canonical_dedup() {
        let root = Path::new("/tmp/ext-sandbox-roots-test");
        let roots = writable_roots(AccessMode::Sandbox, root);
        // The workspace root is always present (canonicalized).
        assert!(roots.iter().any(|r| r.ends_with("ext-sandbox-roots-test")));
        // Canonical + deduplicated: a second resolution adds no new entries.
        let before = roots.len();
        let mut seen: Vec<PathBuf> = Vec::new();
        for r in writable_roots(AccessMode::Sandbox, root) {
            if !seen.iter().any(|s| s == &r) {
                seen.push(r);
            }
        }
        assert_eq!(seen.len(), before);
    }

    #[test]
    fn writable_roots_include_manox_home() {
        let Some(home) = manox_home() else {
            return; // no home dir to admit
        };
        let roots = writable_roots(AccessMode::Sandbox, Path::new("/tmp/ws"));
        assert!(roots.contains(&canonicalize_best_effort(&home)));
        // Full access is never fenced, so it has no root list.
        assert!(writable_roots(AccessMode::FullAccess, Path::new("/tmp/ws")).is_empty());
    }

    #[test]
    fn i64_and_wire_vocabulary() {
        // The persisted i64: 1 = sandbox, 2 = full access; every other value
        // (including the retired read-only 0) lands on the bounded default.
        assert_eq!(AccessMode::from_i64(1), AccessMode::Sandbox);
        assert_eq!(AccessMode::from_i64(2), AccessMode::FullAccess);
        assert_eq!(AccessMode::from_i64(0), AccessMode::default());
        assert_eq!(AccessMode::from_i64(-1), AccessMode::default());
        assert_eq!(AccessMode::from_i64(3), AccessMode::default());
        assert_eq!(AccessMode::Sandbox.as_i64(), 1);
        assert_eq!(AccessMode::FullAccess.as_i64(), 2);
        // The closed wire vocabulary: old three-tier names are unknown.
        assert_eq!(AccessMode::from_wire("sandbox"), Some(AccessMode::Sandbox));
        assert_eq!(
            AccessMode::from_wire("full-access"),
            Some(AccessMode::FullAccess)
        );
        for legacy in ["read-only", "workspace-write", "danger-full-access", ""] {
            assert_eq!(AccessMode::from_wire(legacy), None, "{legacy}");
        }
        assert_eq!(
            serde_json::to_value(AccessMode::FullAccess).unwrap(),
            serde_json::json!("full-access")
        );
        assert_eq!(
            serde_json::from_value::<AccessMode>(serde_json::json!("sandbox")).unwrap(),
            AccessMode::Sandbox
        );
    }

    #[test]
    fn validate_escalation_args_pairing() {
        assert!(validate_escalation_args(false, None).is_ok());
        // A justification without require_escalated is inert prose, not an error.
        assert!(validate_escalation_args(false, Some("reason")).is_ok());
        assert!(validate_escalation_args(true, Some("need it")).is_ok());
        assert!(validate_escalation_args(true, None).is_err());
        assert!(validate_escalation_args(true, Some("   ")).is_err());
    }

    #[test]
    fn marker_and_hint_vocabulary() {
        assert_eq!(
            sandbox_denial_marker(AccessMode::Sandbox),
            "[sandbox: file access denied under sandbox mode]"
        );
        assert!(escalation_hint_marker("command").contains("retry this exact command once"));
        assert!(escalation_hint_marker("command").contains("`require_escalated: true`"));
    }

    struct CannedApprover(EscalationOutcome);

    #[async_trait::async_trait]
    impl EscalationApprover for CannedApprover {
        async fn request(&self, _req: EscalationRequest) -> EscalationOutcome {
            self.0
        }
    }

    fn req(mode: AccessMode, effective: AccessMode) -> EscalationRequest {
        EscalationRequest {
            requested_mode: mode,
            justification: "need it".into(),
            effective_mode: effective,
            subject: "command".into(),
            tool_name: "Bash".into(),
            call_id: "test".into(),
            signal: None,
        }
    }

    #[tokio::test]
    async fn approve_escalation_same_level_is_idempotent_no_op() {
        // full access -> full access: the call already runs at that
        // mode, so the request is a no-op grant — it neither prompts a human
        // (no approver is composed) nor errors.
        let mode = approve_escalation(req(AccessMode::FullAccess, AccessMode::FullAccess), None)
            .await
            .unwrap();
        assert_eq!(mode, AccessMode::FullAccess);
    }

    #[tokio::test]
    async fn approve_escalation_narrowing_fails_with_omit_hint() {
        // full-access -> sandbox narrows: escalation only widens, and the
        // error must teach the omission escape hatch.
        let err = approve_escalation(req(AccessMode::Sandbox, AccessMode::FullAccess), None)
            .await
            .unwrap_err();
        assert!(
            err.contains("not strictly wider") && err.contains("omit `require_escalated`"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn approve_escalation_no_approver_fails_closed() {
        let err = approve_escalation(req(AccessMode::FullAccess, AccessMode::Sandbox), None)
            .await
            .unwrap_err();
        assert!(err.contains("no approval service is composed"), "{err}");
    }

    #[tokio::test]
    async fn approve_escalation_allowed_grants_one_call() {
        let mode = approve_escalation(
            req(AccessMode::FullAccess, AccessMode::Sandbox),
            Some(&CannedApprover(EscalationOutcome::AllowedOnce)),
        )
        .await
        .unwrap();
        assert_eq!(mode, AccessMode::FullAccess);
    }

    #[tokio::test]
    async fn approve_escalation_rejected_is_verbatim() {
        let err = approve_escalation(
            req(AccessMode::FullAccess, AccessMode::Sandbox),
            Some(&CannedApprover(EscalationOutcome::Rejected)),
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("the user rejected escalating this command to \"full-access\""),
            "{err}"
        );
    }
}
