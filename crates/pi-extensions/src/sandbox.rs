//! The sandbox POLICY vocabulary: the per-call file-effect mode, the
//! writable-root derivation shared by the bash seatbelt and the fs write
//! fence, and the `sandbox_permissions` escalation contract. Mirrors
//! `~/projects/github/deepseek-harness` `dsh-sandbox` + `escalation.ts`.
//!
//! This is the extension-layer home so the bash tool (`pi_extensions::bash`)
//! and the host fs-fence wrapper consume one vocabulary without a host
//! import — the extension layer must not depend back on the host. The host
//! re-exports `PermissionMode` from `crate::thread` for session/persistence.
//!
//! Model-facing strings (markers, escalation errors) are English and never
//! pass through i18n — the model reads them verbatim.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File-effect policy for confined bash and the fs write fence: the mode a
/// call runs under. `read-only` permits only required sinks (`/dev/null`);
/// `workspace-write` also permits the workspace root, the manox state home
/// (`~/.manox`), and platform temp areas; `danger-full-access` bypasses
/// confinement. Persisted in the session sidecar (wire field `approval_mode`,
/// kebab values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Confined bash runs but may not write files; fs mutations are refused.
    ReadOnly,
    /// Writes under the workspace root, the manox state home (`~/.manox`),
    /// and platform temp areas are allowed.
    #[default]
    WorkspaceWrite,
    /// No confinement; bash runs unsandboxed, fs mutations are unfenced.
    DangerFullAccess,
}

/// Sentinel for "no per-call grant" on a shared grant cell. The host stamps an
/// approved escalation's mode as `as_i64()` for one call; `NO_GRANT` means the
/// standing session mode applies.
pub const NO_GRANT: i64 = i64::MIN;

impl PermissionMode {
    /// The kebab wire string (the schema enum value and the marker vocabulary).
    pub fn wire(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    /// Parse a wire string; `None` for anything outside the closed vocabulary.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "danger-full-access" => Some(Self::DangerFullAccess),
            _ => None,
        }
    }

    /// Map the persisted i64; any unknown value lands on the single bounded
    /// default.
    pub fn from_i64(v: i64) -> Self {
        match v {
            0 => Self::ReadOnly,
            1 => Self::WorkspaceWrite,
            2 => Self::DangerFullAccess,
            _ => Self::default(),
        }
    }

    pub fn as_i64(self) -> i64 {
        match self {
            Self::ReadOnly => 0,
            Self::WorkspaceWrite => 1,
            Self::DangerFullAccess => 2,
        }
    }
}

/// The strictly-wider ladder: what a call whose effective mode is the key may
/// escalate TO. Checked at execution, never baked into a tool schema — the
/// schema advertises [`ESCALATION_TARGETS`], while the effective mode is
/// per-call truth.
pub const WIDER_MODES: &[(PermissionMode, &[PermissionMode])] = &[
    (
        PermissionMode::ReadOnly,
        &[
            PermissionMode::WorkspaceWrite,
            PermissionMode::DangerFullAccess,
        ],
    ),
    (
        PermissionMode::WorkspaceWrite,
        &[PermissionMode::DangerFullAccess],
    ),
];

/// The closed escalation-target vocabulary — every mode a call could ever
/// escalate TO (`read-only` is the floor; nothing escalates to it).
pub const ESCALATION_TARGETS: &[PermissionMode] = &[
    PermissionMode::WorkspaceWrite,
    PermissionMode::DangerFullAccess,
];

/// Validate the escalation argument pairing a tool schema cannot express:
/// `sandbox_permissions` and `justification` travel together — an approval
/// prompt without a reason, or a reason driving nothing, is a malformed ask —
/// and the justification must be a non-empty sentence.
pub fn validate_escalation_args(
    sandbox_permissions: Option<&str>,
    justification: Option<&str>,
) -> Result<(), String> {
    if sandbox_permissions.is_some() && justification.is_none() {
        return Err("invalid escalation: sandbox_permissions requires a justification".into());
    }
    if justification.is_some() && sandbox_permissions.is_none() {
        return Err(
            "invalid escalation: justification is only valid together with sandbox_permissions"
                .into(),
        );
    }
    if let Some(j) = justification
        && j.trim().is_empty()
    {
        return Err("invalid justification: expected a non-empty sentence".into());
    }
    Ok(())
}

/// The model-facing denial marker — the one vocabulary both enforcing families
/// (bash and fs) teach and report, so the model recognizes a policy denial
/// identically whether the seatbelt refused a bash file effect or the fs fence
/// refused a mutation.
pub fn sandbox_denial_marker(mode: PermissionMode) -> String {
    format!("[sandbox: file access denied under {} mode]", mode.wire())
}

/// The same-turn escalation hint that rides a denial: the nudge lives at the
/// decision point so the sanctioned retry does not depend on the model
/// recalling the tool description. `subject` is the family's noun for the
/// denied action (`"command"` for bash, `"operation"` for a fs mutation).
pub fn escalation_hint_marker(subject: &str) -> String {
    format!(
        "[sandbox: escalation available — retry this exact {subject} once with sandbox_permissions \
         (the narrowest wider mode that suffices) + justification; the approval prompt asks the user]"
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
/// definitions), part of the `workspace-write` writable scope so session
/// state such as plan files needs no escalation. `None` when the home dir
/// does not resolve — the host then places its state under the process cwd,
/// which the workspace root already admits.
pub fn manox_home() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".manox"))
}

/// The roots one confined execution may WRITE under — the mode's meaning as a
/// canonical, deduplicated allow-list. `read-only` returns an empty list
/// (no caller guard needed — mirrors deepseek's `writableRoots(policy)` which
/// checks the mode itself); `workspace-write` allows the policy's workspace
/// root, the manox state home ([`manox_home`] — plan files and other session
/// state write without escalation), the host `/tmp`, and the per-user
/// platform temp dir (`std::env::temp_dir()` — the real temp area; omitting
/// it would deny what the mode promises). Shared by the bash seatbelt and
/// the fs fence so the "the write tool cannot write `/tmp` but bash can"
/// asymmetry cannot arise.
pub fn writable_roots(mode: PermissionMode, workspace_root: &Path) -> Vec<PathBuf> {
    if mode != PermissionMode::WorkspaceWrite {
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
/// spools plan mode admits alongside the plans dir).
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
    /// in manox today; it maps to the deepseek `unavailable` outcome (which,
    /// like here, is only returned when an existing approver reports its
    /// channel unavailable — a missing approver is a fatal `Err` in both).
    Unavailable,
}

/// One escalation request, as [`approve_escalation`] judges it.
pub struct EscalationRequest {
    /// The requested target mode (validated against [`WIDER_MODES`] at
    /// execution).
    pub requested_mode: PermissionMode,
    /// The model's one-sentence reason, shown verbatim to the user.
    pub justification: String,
    /// The call's effective mode (session override or standing default) the
    /// request must strictly widen.
    pub effective_mode: PermissionMode,
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
/// non-widening request, a missing approval service, a rejection, a
/// cancellation, an unanswerable ask). A non-widening request never prompts a
/// human.
pub async fn approve_escalation(
    request: EscalationRequest,
    approver: Option<&dyn EscalationApprover>,
) -> Result<PermissionMode, String> {
    let mode = request.requested_mode;
    let subject = request.subject.clone();
    // Strict widening is an execution check against the call's effective mode.
    let wider = WIDER_MODES
        .iter()
        .find(|(base, _)| *base == request.effective_mode)
        .map(|(_, wider)| *wider)
        .unwrap_or(&[] as &[PermissionMode]);
    if !wider.contains(&mode) {
        return Err(format!(
            "sandbox escalation to \"{}\" is not strictly wider than this call's current \"{}\" mode",
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
        let root = Path::new("/tmp/pi-ext-sandbox-roots-test");
        let roots = writable_roots(PermissionMode::WorkspaceWrite, root);
        // The workspace root is always present (canonicalized).
        assert!(
            roots
                .iter()
                .any(|r| r.ends_with("pi-ext-sandbox-roots-test"))
        );
        // Canonical + deduplicated: a second resolution adds no new entries.
        let before = roots.len();
        let mut seen: Vec<PathBuf> = Vec::new();
        for r in writable_roots(PermissionMode::WorkspaceWrite, root) {
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
        let roots = writable_roots(PermissionMode::WorkspaceWrite, Path::new("/tmp/ws"));
        assert!(roots.contains(&canonicalize_best_effort(&home)));
        // read-only admits nothing, manox home included.
        assert!(writable_roots(PermissionMode::ReadOnly, Path::new("/tmp/ws")).is_empty());
    }

    #[test]
    fn validate_escalation_args_pairing() {
        assert!(validate_escalation_args(None, None).is_ok());
        assert!(validate_escalation_args(Some("workspace-write"), Some("need it")).is_ok());
        assert!(validate_escalation_args(Some("workspace-write"), None).is_err());
        assert!(validate_escalation_args(None, Some("reason")).is_err());
        assert!(validate_escalation_args(Some("workspace-write"), Some("   ")).is_err());
    }

    #[test]
    fn marker_and_hint_vocabulary() {
        assert_eq!(
            sandbox_denial_marker(PermissionMode::ReadOnly),
            "[sandbox: file access denied under read-only mode]"
        );
        assert!(escalation_hint_marker("command").contains("retry this exact command once"));
    }

    #[test]
    fn wider_modes_ladder() {
        let ro = WIDER_MODES
            .iter()
            .find(|(b, _)| *b == PermissionMode::ReadOnly)
            .unwrap();
        assert_eq!(
            ro.1,
            &[
                PermissionMode::WorkspaceWrite,
                PermissionMode::DangerFullAccess
            ]
        );
        let ww = WIDER_MODES
            .iter()
            .find(|(b, _)| *b == PermissionMode::WorkspaceWrite)
            .unwrap();
        assert_eq!(ww.1, &[PermissionMode::DangerFullAccess]);
        // read-only is the floor: nothing escalates to it.
        assert!(!ESCALATION_TARGETS.contains(&PermissionMode::ReadOnly));
    }

    struct CannedApprover(EscalationOutcome);

    #[async_trait::async_trait]
    impl EscalationApprover for CannedApprover {
        async fn request(&self, _req: EscalationRequest) -> EscalationOutcome {
            self.0
        }
    }

    fn req(mode: PermissionMode, effective: PermissionMode) -> EscalationRequest {
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
    async fn approve_escalation_non_widening_never_prompts() {
        // workspace-write -> workspace-write is not strictly wider.
        let err = approve_escalation(
            req(
                PermissionMode::WorkspaceWrite,
                PermissionMode::WorkspaceWrite,
            ),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("not strictly wider"), "{err}");
    }

    #[tokio::test]
    async fn approve_escalation_no_approver_fails_closed() {
        let err = approve_escalation(
            req(PermissionMode::WorkspaceWrite, PermissionMode::ReadOnly),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("no approval service is composed"), "{err}");
    }

    #[tokio::test]
    async fn approve_escalation_allowed_grants_one_call() {
        let mode = approve_escalation(
            req(PermissionMode::WorkspaceWrite, PermissionMode::ReadOnly),
            Some(&CannedApprover(EscalationOutcome::AllowedOnce)),
        )
        .await
        .unwrap();
        assert_eq!(mode, PermissionMode::WorkspaceWrite);
    }

    #[tokio::test]
    async fn approve_escalation_rejected_is_verbatim() {
        let err = approve_escalation(
            req(PermissionMode::WorkspaceWrite, PermissionMode::ReadOnly),
            Some(&CannedApprover(EscalationOutcome::Rejected)),
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("the user rejected escalating this command to \"workspace-write\""),
            "{err}"
        );
    }
}
