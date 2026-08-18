//! External agent CLI sessions (claude / codex / copilot) launched from the
//! sidebar's `+` menu.
//!
//! An `ExternalSession` owns a `TerminalView` rendering the agent's TUI (driven
//! through a `CxSessionSource` PTY source that wraps the shared
//! `cx::SessionHandle`) plus the `Arc<SessionHandle>` itself, so the close path
//! can `kill` the agent explicitly.
//!
//! Agent-kind sessions survive an app exit as a [`ResumeSidecar`]: the sidecar
//! is written at spawn, deleted only when the session is explicitly closed, and
//! re-scanned at startup. A sidecar left on disk is therefore exactly an
//! *unclosed* session — the user re-opens it from the sidebar and manox
//! re-spawns the CLI with its resume flag (`claude --continue` / `codex resume
//! --last` / `copilot --continue`), so the CLI's own on-disk conversation
//! storage picks the conversation back up. Plain PTY shells are never
//! persisted (nothing to resume).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent::i18n;
use anyhow::Result;
use gpui::{Entity, SharedString, Subscription};
use serde::{Deserialize, Serialize};

use terminal_ui::TerminalView;

/// Which session a sidebar `+` spawn runs. The first three are external agent
/// CLIs driven through cx (provider/model injection + IPC handle); `Terminal`
/// is a plain PTY session with no cx involvement — the user's shell in the
/// session's cwd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    ClaudeCode,
    Codex,
    GithubCopilot,
    /// A plain interactive shell in the session's cwd.
    Terminal,
}

impl SessionKind {
    /// The sidebar row label. Brand names stay untranslated; `Terminal` is
    /// UI chrome and localized.
    pub fn label(&self) -> SharedString {
        match self {
            Self::ClaudeCode => "Claude Code".into(),
            Self::Codex => "Codex".into(),
            Self::GithubCopilot => "GitHub Copilot".into(),
            Self::Terminal => i18n::t("session-kind-terminal"),
        }
    }

    /// The cx `Agent` to launch; `None` for plain PTY sessions.
    pub fn agent(&self) -> Option<cx::Agent> {
        match self {
            Self::ClaudeCode => Some(cx::Agent::Claude),
            Self::Codex => Some(cx::Agent::Codex),
            Self::GithubCopilot => Some(cx::Agent::Copilot),
            Self::Terminal => None,
        }
    }

    /// The cx agent id matching `ResolvedModel.visible_agents` entries, used to
    /// filter the model list to those that can drive this agent. Plain PTY
    /// sessions have no model cascade; the id only namespaces their session
    /// id (`external:<id>:<uuid>`).
    pub fn agent_id(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::GithubCopilot => "copilot",
            Self::Terminal => "terminal",
        }
    }

    /// Embedded SVG asset path (resolved by `ExtrasAssetSource`) for the
    /// session's icon. The SVGs use `currentColor` so the caller tints via
    /// `.text_color(...)`.
    pub fn icon_asset(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "icons/claude.svg",
            Self::Codex => "icons/codex.svg",
            Self::GithubCopilot => "icons/githubcopilot.svg",
            Self::Terminal => "icons/terminal.svg",
        }
    }

    /// The agent kind named by a `ResumeSidecar`'s `agent_id`. `None` for the
    /// plain PTY kind — terminal shells are never persisted, so a sidecar
    /// never carries `terminal`.
    pub fn from_agent_id(agent_id: &str) -> Option<Self> {
        match agent_id {
            "claude" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            "copilot" => Some(Self::GithubCopilot),
            _ => None,
        }
    }
}

/// A live external agent CLI session. The `TerminalView` renders the agent's
/// TUI; the shared `handle` lets the close path `kill` the agent (the terminal
/// view holds the `CxSessionSource` that drives IO, this clone holds the
/// kill-capable reference so closing the tab doesn't orphan the child).
///
/// The `id` is namespaced `external:<kind>:<uuid>` so it never collides with a
/// manox thread UUID in the sidebar's selection namespace. The cx-internal
/// `cx_session_id` (and the socket path backing it) are the traceable identity
/// for `~/.manox/sessions/<id>.sock`, surfaced in the sidebar tag and the
/// copy-to-clipboard action; they are derived from `handle.socket_path()` until
/// cx exposes `session_id()` publicly.
///
/// `_exit_sub` observes the terminal's events — `ChildExit` tears the session
/// down on a natural CLI exit (e.g. `/exit` in claude), and `Title` syncs the
/// agent's OSC title into the sidebar row + titlebar. The subscription lives on
/// the session so an explicit close detaches it first; once detached, the killed
/// child's eventual reap emits `ChildExit` into a listener set that no longer
/// holds this observer, so the close path is the sole remover (no
/// double-removal).
pub struct ExternalSession {
    pub id: String,
    pub kind: SessionKind,
    /// Epoch seconds at spawn time. The sidebar sort key so an external
    /// session mixes into the Conversations list by recency alongside manox
    /// threads (which sort by `interacted_at`). manox cannot observe model
    /// switches inside the TUI, let alone inter-message timing, so the spawn
    /// time is the only stable ordering signal it has.
    pub created_at: i64,
    /// The project path the session was bound to at spawn (`Some` when launched
    /// from a project folder's `+` button, `None` from the Conversations
    /// header). The sidebar uses it to group the row under its project folder
    /// instead of in the loose Conversations list, matching how manox threads
    /// bound to a project are grouped.
    pub project: Option<PathBuf>,
    /// The agent's OSC title, mirrored from `TerminalEvent::Title`. Empty until
    /// the TUI emits one; `display_title()` falls back to the kind label so a
    /// freshly spawned session reads "Claude Code" / "Codex" / "GitHub
    /// Copilot" before the TUI sets its own title.
    pub title: Option<String>,
    /// The cx-internal session id naming `~/.manox/sessions/<id>.sock`.
    /// Recovered from `handle.socket_path()`'s `<id>.sock` filename until cx
    /// exposes `SessionHandle::session_id()`. Empty when the IPC bind failed
    /// (no socket to derive from).
    pub cx_session_id: String,
    /// The absolute socket path (`~/.manox/sessions/<id>.sock`), `None`
    /// when the IPC bind failed. Copied to the clipboard as a fallback identity
    /// alongside `cx_session_id`.
    pub socket_path: Option<PathBuf>,
    pub terminal_view: Entity<TerminalView>,
    /// The cx IPC handle for agent kinds — the close path kills through it.
    /// `None` for plain PTY sessions (Terminal): dropping the
    /// `TerminalView` drops the `PtyHandle`, whose teardown kills the child
    /// tree, so no explicit kill reference is needed.
    pub handle: Option<Arc<cx::SessionHandle>>,
    pub _exit_sub: Subscription,
    /// The durable record backing this session — written at spawn, kept in
    /// sync (title) while live, deleted on close. `None` for plain PTY
    /// sessions, which are never persisted.
    pub sidecar: Option<ResumeSidecar>,
}

impl ExternalSession {
    /// Display line for the sidebar row / titlebar: the session's OSC title
    /// when it has set a non-empty one, else the kind label.
    pub fn display_title(&self) -> SharedString {
        match self.title.as_deref().filter(|t| !t.trim().is_empty()) {
            Some(t) => SharedString::from(t),
            None => self.kind.label(),
        }
    }

    /// The lightweight descriptor the sidebar renders from. The sidebar is a
    /// separate Entity from the Workspace that owns the live `ExternalSession`
    /// (with its `TerminalView` + `Arc<SessionHandle>`); it only needs identity
    /// and display fields to render a row. The spawn-time provider/model are
    /// intentionally not projected: the user can switch models inside the TUI
    /// and manox cannot observe that, so showing them would mislead.
    pub fn summary(&self) -> ExternalSessionSummary {
        ExternalSessionSummary {
            id: self.id.clone(),
            kind: self.kind,
            created_at: self.created_at,
            project: self.project.clone(),
            title: self.title.clone(),
            cx_session_id: self.cx_session_id.clone(),
            socket_path: self.socket_path.clone(),
            resumable: false,
            resuming: false,
        }
    }
}

/// Render-only projection of an [`ExternalSession`] handed to the sidebar so it
/// can list external rows without holding PTY handles or terminal views. The
/// Workspace owns the canonical `Vec<ExternalSession>` and pushes a fresh
/// `Vec<ExternalSessionSummary>` snapshot to the sidebar whenever the set
/// changes (spawn/close) or a title updates.
///
/// `resumable` marks a row restored from a [`ResumeSidecar`]: no live process
/// backs it, clicking re-spawns the CLI with the resume flag.
#[derive(Debug, Clone)]
pub struct ExternalSessionSummary {
    pub id: String,
    pub kind: SessionKind,
    pub created_at: i64,
    pub project: Option<PathBuf>,
    /// Mirrored OSC title — `display_title()` falls back to the kind label.
    pub title: Option<String>,
    /// cx session id backing `~/.manox/sessions/<id>.sock`. Surfaced in the
    /// sidebar tag (short) + clipboard copy (full). Empty for a resumable row
    /// (its cx id died with the previous process).
    pub cx_session_id: String,
    /// Absolute socket path, copied to the clipboard as a fallback identity.
    pub socket_path: Option<PathBuf>,
    /// The row has no live process; clicking it resumes the CLI session.
    pub resumable: bool,
    /// A resume is in flight for this row (the CLI is being re-spawned); the
    /// sidebar renders a loading indicator instead of the idle row.
    pub resuming: bool,
}

impl ExternalSessionSummary {
    /// Display line: the session's OSC title when non-empty, else the kind
    /// label.
    pub fn display_title(&self) -> SharedString {
        match self.title.as_deref().filter(|t| !t.trim().is_empty()) {
            Some(t) => SharedString::from(t),
            None => self.kind.label(),
        }
    }

    /// The value copied to the clipboard from the row's id tag — the cx session
    /// id traces back to `~/.manox/sessions/<id>.sock`; the socket path is a
    /// fallback when the id could not be recovered (IPC bind failed).
    pub fn copy_identity(&self) -> String {
        if !self.cx_session_id.is_empty() {
            self.cx_session_id.clone()
        } else if let Some(p) = &self.socket_path {
            p.to_string_lossy().into_owned()
        } else {
            self.id.clone()
        }
    }
}

/// Recover the cx session id from a `<id>.sock` socket path (stripping the
/// `.sock` extension + parent dir). Returns `None` for paths that do not end in
/// `.sock`. cx does not yet expose `SessionHandle::session_id()`, so this is
/// the derivation until that lands upstream.
pub(crate) fn cx_session_id_from_socket(path: &std::path::Path) -> Option<String> {
    let file = path.file_name()?.to_str()?;
    let trimmed = file.strip_suffix(".sock")?;
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The durable record of an unclosed external agent session. Written at spawn,
/// deleted only when the session is explicitly closed (sidebar `×` or a
/// natural CLI exit); a graceful quit or a crash leaves it on disk, so startup
/// can offer exactly the sessions the user never closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeSidecar {
    /// The `external:<agent>:<uuid>` session id — the sidebar row identity and
    /// the sidecar filename base.
    pub id: String,
    /// `claude` / `codex` / `copilot` — the `SessionKind` is recovered via
    /// [`SessionKind::from_agent_id`]; plain PTY sessions never write a sidecar.
    pub agent_id: String,
    /// The directory the CLI was launched in; `--continue`-style resume runs
    /// there so the CLI picks the same project's conversation.
    pub cwd: String,
    /// The project folder the session was bound to at spawn (`+` button), if
    /// any — the sidebar groups the resumable row under the same folder.
    pub project: Option<PathBuf>,
    /// Epoch seconds at spawn; the sidebar recency sort key.
    pub created_at: i64,
    /// The provider the session was launched under, replayed on resume (no
    /// picker).
    pub provider: String,
    /// The model id the session was launched with, replayed on resume.
    pub model: String,
    /// The agent's last mirrored OSC title, if it ever set one.
    pub title: Option<String>,
}

impl ResumeSidecar {
    pub fn summary(&self) -> ExternalSessionSummary {
        ExternalSessionSummary {
            id: self.id.clone(),
            kind: SessionKind::from_agent_id(&self.agent_id)
                .expect("sidecar agent_id is always an agent kind"),
            created_at: self.created_at,
            project: self.project.clone(),
            title: self.title.clone(),
            cx_session_id: String::new(),
            socket_path: None,
            resumable: true,
            resuming: false,
        }
    }
}

/// `~/.manox/external-sessions` — one `<id>.json` per unclosed
/// external agent session.
pub(crate) fn resume_dir() -> PathBuf {
    agent::paths::manox_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("external-sessions")
}

/// Persist a session sidecar (atomic write: temp file + rename, so a crash
/// mid-write never leaves a half-written record the scanner would parse).
pub(crate) fn write_sidecar(sidecar: &ResumeSidecar) -> Result<()> {
    write_sidecar_in(&resume_dir(), sidecar)
}

/// Drop a session's sidecar — the explicit-close path. The session is then no
/// longer offered for resume.
pub(crate) fn remove_sidecar(id: &str) {
    remove_sidecar_in(&resume_dir(), id);
}

/// Every unclosed session's sidecar, newest first. Unparsable files are
/// skipped (a corrupt sidecar must not block the whole list).
pub(crate) fn list_sidecars() -> Vec<ResumeSidecar> {
    list_sidecars_in(&resume_dir())
}

/// [`write_sidecar`] against an explicit directory — the test seam so sidecar
/// roundtrips run against a tempdir instead of the real config root.
pub(crate) fn write_sidecar_in(dir: &Path, sidecar: &ResumeSidecar) -> Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", sidecar.id));
    let tmp = dir.join(format!("{}.json.tmp", sidecar.id));
    let content = serde_json::to_vec_pretty(sidecar)?;
    fs::write(&tmp, content)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// [`remove_sidecar`] against an explicit directory.
pub(crate) fn remove_sidecar_in(dir: &Path, id: &str) {
    let _ = fs::remove_file(dir.join(format!("{id}.json")));
}

/// [`list_sidecars`] against an explicit directory.
pub(crate) fn list_sidecars_in(dir: &Path) -> Vec<ResumeSidecar> {
    let entries = match fs::read_dir(dir) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<ResumeSidecar> = entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .filter_map(|s| serde_json::from_str(&s).ok())
        // Unknown agent_ids (a manually-edited or future-build sidecar) are
        // dropped here so `ResumeSidecar::summary`'s kind invariant holds and
        // startup never panics on a foreign record.
        .filter(|s: &ResumeSidecar| SessionKind::from_agent_id(&s.agent_id).is_some())
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.created_at));
    out
}

/// CLI resume flags per agent, appended after the agent's configured args via
/// `cx::AgentBuilder::passthrough`. All three resume the most recent
/// conversation the CLI recorded for the launch cwd (claude / copilot) or the
/// most recent recorded session (codex), which after an app exit is exactly the
/// unclosed one.
pub(crate) fn resume_args(agent_id: &str) -> Vec<String> {
    match agent_id {
        "claude" | "copilot" => vec!["--continue".into()],
        "codex" => vec!["resume".into(), "--last".into()],
        _ => Vec::new(),
    }
}

/// Merge the live session summaries and the resumable sidecars into the single
/// list the sidebar renders. A sidecar whose id now runs live (resumed this
/// launch) is dropped so the row never appears twice. Live rows come first,
/// then resumable ones, each group newest-first as produced by their sources.
pub(crate) fn merge_external_summaries(
    live: Vec<ExternalSessionSummary>,
    resumable: Vec<ResumeSidecar>,
) -> Vec<ExternalSessionSummary> {
    let live_ids: std::collections::HashSet<String> = live.iter().map(|s| s.id.clone()).collect();
    let mut out = live;
    out.extend(
        resumable
            .into_iter()
            .filter(|r| !live_ids.contains(&r.id))
            .map(|r| r.summary()),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_summary() -> ExternalSessionSummary {
        ExternalSessionSummary {
            id: "external:claude:x".into(),
            kind: SessionKind::ClaudeCode,
            created_at: 0,
            project: None,
            title: None,
            cx_session_id: "deadbeef".into(),
            socket_path: Some(PathBuf::from("/h/u/.manox/sessions/deadbeef.sock")),
            resumable: false,
            resuming: false,
        }
    }

    #[test]
    fn title_falls_back_to_kind_label() {
        let mut s = sample_summary();
        assert_eq!(s.display_title().as_str(), "Claude Code");
        s.title = Some("   ".into());
        assert_eq!(s.display_title().as_str(), "Claude Code"); // whitespace-only ignored
        s.title = Some("Refactor auth".into());
        assert_eq!(s.display_title().as_str(), "Refactor auth");
    }

    #[test]
    fn title_falls_back_per_kind() {
        let mut codex = sample_summary();
        codex.kind = SessionKind::Codex;
        assert_eq!(codex.display_title().as_str(), "Codex");
        let mut copilot = sample_summary();
        copilot.kind = SessionKind::GithubCopilot;
        assert_eq!(copilot.display_title().as_str(), "GitHub Copilot");
        let mut terminal = sample_summary();
        terminal.kind = SessionKind::Terminal;
        terminal.title = None;
        assert!(!terminal.display_title().is_empty());
    }

    #[test]
    fn copy_identity_prefers_cx_session_id() {
        let s = sample_summary();
        assert_eq!(s.copy_identity(), "deadbeef");
    }

    #[test]
    fn copy_identity_falls_back_to_socket_path() {
        let mut s = sample_summary();
        s.cx_session_id = String::new();
        assert_eq!(s.copy_identity(), "/h/u/.manox/sessions/deadbeef.sock");
    }

    #[test]
    fn cx_session_id_extracted_from_socket_path() {
        let p = std::path::Path::new("/home/u/.manox/sessions/abcdef0123.sock");
        assert_eq!(cx_session_id_from_socket(p).as_deref(), Some("abcdef0123"));
        assert_eq!(
            cx_session_id_from_socket(std::path::Path::new("/tmp/notasock.json")),
            None
        );
        assert_eq!(cx_session_id_from_socket(std::path::Path::new("")), None);
    }

    fn sample_sidecar() -> ResumeSidecar {
        ResumeSidecar {
            id: "external:claude:abc".into(),
            agent_id: "claude".into(),
            cwd: "/repo".into(),
            project: Some(PathBuf::from("/repo")),
            created_at: 42,
            provider: "DeepSeek".into(),
            model: "deepseek-v4-flash[1m]".into(),
            title: None,
        }
    }

    #[test]
    fn sidecar_roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = sample_sidecar();
        write_sidecar_in(dir.path(), &sidecar).expect("write");
        let listed = list_sidecars_in(dir.path());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, sidecar.id);
        assert_eq!(listed[0].agent_id, "claude");
        assert_eq!(listed[0].cwd, "/repo");
        assert_eq!(listed[0].provider, "DeepSeek");
        assert_eq!(listed[0].model, "deepseek-v4-flash[1m]");
        remove_sidecar_in(dir.path(), &sidecar.id);
        assert!(
            list_sidecars_in(dir.path()).is_empty(),
            "removed sidecar is gone"
        );
    }

    #[test]
    fn list_sidecars_skips_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        write_sidecar_in(dir.path(), &sample_sidecar()).expect("write");
        std::fs::write(dir.path().join("external:codex:garbage.json"), "not json").unwrap();
        std::fs::write(dir.path().join("not-a-sidecar.txt"), "ignored").unwrap();
        let listed = list_sidecars_in(dir.path());
        assert_eq!(listed.len(), 1, "corrupt + non-json files are skipped");
        assert_eq!(listed[0].id, "external:claude:abc");
    }

    /// A valid-JSON sidecar with an unknown agent_id (a manually-edited or
    /// future-build record) must be dropped at list time — `ResumeSidecar::summary`
    /// would otherwise panic on it at startup.
    #[test]
    fn list_sidecars_skips_unknown_agent_ids() {
        let dir = tempfile::tempdir().unwrap();
        write_sidecar_in(dir.path(), &sample_sidecar()).expect("write");
        let mut alien = sample_sidecar();
        alien.id = "external:gemini:xyz".into();
        alien.agent_id = "gemini".into();
        write_sidecar_in(dir.path(), &alien).expect("write");
        let listed = list_sidecars_in(dir.path());
        assert_eq!(
            listed.len(),
            1,
            "unknown agent_id is dropped without panicking"
        );
        assert_eq!(listed[0].agent_id, "claude");
    }

    #[test]
    fn list_sidecars_orders_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut old = sample_sidecar();
        old.created_at = 1;
        let mut new = sample_sidecar();
        new.id = "external:codex:xyz".into();
        new.agent_id = "codex".into();
        new.created_at = 2;
        write_sidecar_in(dir.path(), &old).expect("write");
        write_sidecar_in(dir.path(), &new).expect("write");
        let ids: Vec<_> = list_sidecars_in(dir.path())
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, vec!["external:codex:xyz", "external:claude:abc"]);
    }

    #[test]
    fn sidecar_summary_is_resumable_and_preserves_identity() {
        let s = sample_sidecar().summary();
        assert!(s.resumable);
        assert_eq!(s.id, "external:claude:abc");
        assert_eq!(s.kind, SessionKind::ClaudeCode);
        assert_eq!(s.created_at, 42);
        assert_eq!(s.project.as_deref(), Some(std::path::Path::new("/repo")));
        assert!(
            s.cx_session_id.is_empty(),
            "no live cx id on a resumable row"
        );
    }

    #[test]
    fn from_agent_id_rejects_plain_terminal() {
        assert_eq!(
            SessionKind::from_agent_id("claude"),
            Some(SessionKind::ClaudeCode)
        );
        assert_eq!(
            SessionKind::from_agent_id("codex"),
            Some(SessionKind::Codex)
        );
        assert_eq!(
            SessionKind::from_agent_id("copilot"),
            Some(SessionKind::GithubCopilot)
        );
        assert_eq!(SessionKind::from_agent_id("terminal"), None);
        assert_eq!(SessionKind::from_agent_id("other"), None);
    }

    #[test]
    fn resume_args_map_per_agent() {
        assert_eq!(resume_args("claude"), vec!["--continue"]);
        assert_eq!(resume_args("copilot"), vec!["--continue"]);
        assert_eq!(resume_args("codex"), vec!["resume", "--last"]);
        assert!(resume_args("unknown").is_empty());
    }

    #[test]
    fn merge_drops_sidecars_now_live() {
        let mut live = sample_summary();
        live.id = "external:claude:abc".into();
        let mut other_live = sample_summary();
        other_live.id = "external:codex:live".into();
        other_live.kind = SessionKind::Codex;
        let resumable = vec![
            sample_sidecar(), // now live → dropped
            {
                let mut s = sample_sidecar();
                s.id = "external:copilot:dead".into();
                s.agent_id = "copilot".into();
                s
            },
        ];
        let merged = merge_external_summaries(vec![live, other_live], resumable);
        assert_eq!(merged.len(), 3);
        let resumable_rows: Vec<_> = merged.iter().filter(|s| s.resumable).collect();
        assert_eq!(resumable_rows.len(), 1);
        assert_eq!(resumable_rows[0].id, "external:copilot:dead");
        assert_eq!(resumable_rows[0].kind, SessionKind::GithubCopilot);
    }
}
