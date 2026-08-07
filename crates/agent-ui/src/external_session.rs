//! External agent CLI sessions (claude / codex / copilot) launched from the
//! sidebar's `+` menu.
//!
//! An `ExternalSession` owns a `TerminalView` rendering the agent's TUI (driven
//! through a `CxSessionSource` PTY source that wraps the shared
//! `cx::SessionHandle`) plus the `Arc<SessionHandle>` itself, so the close path
//! can `kill` the agent explicitly. Sessions live only in memory — they are not
//! persisted and vanish from the sidebar on exit.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{Entity, Subscription};

use terminal_ui::TerminalView;

/// Which external agent CLI a session runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    ClaudeCode,
    Codex,
    GithubCopilot,
}

impl SessionKind {
    /// The sidebar row label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::GithubCopilot => "GitHub Copilot",
        }
    }

    /// The cx `Agent` to launch.
    pub fn agent(&self) -> cx::Agent {
        match self {
            Self::ClaudeCode => cx::Agent::Claude,
            Self::Codex => cx::Agent::Codex,
            Self::GithubCopilot => cx::Agent::Copilot,
        }
    }

    /// The cx agent id matching `ResolvedModel.visible_agents` entries, used to
    /// filter the model list to those that can drive this agent.
    pub fn agent_id(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::GithubCopilot => "copilot",
        }
    }

    /// Embedded SVG asset path (resolved by `ExtrasAssetSource`) for the
    /// agent's brand icon. The SVGs use `fill="currentColor"` so the caller
    /// tints via `.text_color(...)`.
    pub fn icon_asset(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "icons/claude.svg",
            Self::Codex => "icons/codex.svg",
            Self::GithubCopilot => "icons/githubcopilot.svg",
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
/// for `~/.config/cx/sessions/<id>.sock`, surfaced in the sidebar tag and the
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
    /// The cx-internal session id naming `~/.config/cx/sessions/<id>.sock`.
    /// Recovered from `handle.socket_path()`'s `<id>.sock` filename until cx
    /// exposes `SessionHandle::session_id()`. Empty when the IPC bind failed
    /// (no socket to derive from).
    pub cx_session_id: String,
    /// The absolute socket path (`~/.config/cx/sessions/<id>.sock`), `None`
    /// when the IPC bind failed. Copied to the clipboard as a fallback identity
    /// alongside `cx_session_id`.
    pub socket_path: Option<PathBuf>,
    pub terminal_view: Entity<TerminalView>,
    pub handle: Arc<cx::SessionHandle>,
    pub _exit_sub: Subscription,
}

impl ExternalSession {
    /// Display line for the sidebar row / titlebar: the agent's OSC title when
    /// it has set a non-empty one, else the kind label ("Claude Code" / "Codex"
    /// / "GitHub Copilot").
    pub fn display_title(&self) -> &str {
        self.title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| self.kind.label())
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
        }
    }
}

/// Render-only projection of an [`ExternalSession`] handed to the sidebar so it
/// can list external rows without holding PTY handles or terminal views. The
/// Workspace owns the canonical `Vec<ExternalSession>` and pushes a fresh
/// `Vec<ExternalSessionSummary>` snapshot to the sidebar whenever the set
/// changes (spawn/close) or a title updates.
#[derive(Debug, Clone)]
pub struct ExternalSessionSummary {
    pub id: String,
    pub kind: SessionKind,
    pub created_at: i64,
    pub project: Option<PathBuf>,
    /// Mirrored OSC title — `display_title()` falls back to the kind label.
    pub title: Option<String>,
    /// cx session id backing `~/.config/cx/sessions/<id>.sock`. Surfaced in the
    /// sidebar tag (short) + clipboard copy (full).
    pub cx_session_id: String,
    /// Absolute socket path, copied to the clipboard as a fallback identity.
    pub socket_path: Option<PathBuf>,
}

impl ExternalSessionSummary {
    /// Display line: the agent's OSC title when non-empty, else the kind label.
    pub fn display_title(&self) -> &str {
        self.title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| self.kind.label())
    }

    /// The value copied to the clipboard from the row's id tag — the cx session
    /// id traces back to `~/.config/cx/sessions/<id>.sock`; the socket path is a
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
            socket_path: Some(PathBuf::from("/h/u/.config/cx/sessions/deadbeef.sock")),
        }
    }

    #[test]
    fn title_falls_back_to_kind_label() {
        let mut s = sample_summary();
        assert_eq!(s.display_title(), "Claude Code");
        s.title = Some("   ".into());
        assert_eq!(s.display_title(), "Claude Code"); // whitespace-only ignored
        s.title = Some("Refactor auth".into());
        assert_eq!(s.display_title(), "Refactor auth");
    }

    #[test]
    fn title_falls_back_per_kind() {
        let mut codex = sample_summary();
        codex.kind = SessionKind::Codex;
        assert_eq!(codex.display_title(), "Codex");
        let mut copilot = sample_summary();
        copilot.kind = SessionKind::GithubCopilot;
        assert_eq!(copilot.display_title(), "GitHub Copilot");
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
        assert_eq!(s.copy_identity(), "/h/u/.config/cx/sessions/deadbeef.sock");
    }

    #[test]
    fn cx_session_id_extracted_from_socket_path() {
        let p = std::path::Path::new("/home/u/.config/cx/sessions/abcdef0123.sock");
        assert_eq!(cx_session_id_from_socket(p).as_deref(), Some("abcdef0123"));
        assert_eq!(
            cx_session_id_from_socket(std::path::Path::new("/tmp/notasock.json")),
            None
        );
        assert_eq!(cx_session_id_from_socket(std::path::Path::new("")), None);
    }
}
