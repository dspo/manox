//! Per-session UI metadata stored beside the pi jsonl transcript.
//!
//! The pi core owns the conversation (jsonl session files); sidebar-only
//! flags — pin, archive, unread, error, display title — have no home in the
//! transcript schema, so they live in a small sidecar keyed by session id.
//! Loading tolerates a missing file (a fresh session has no sidecar yet) but
//! not a corrupt one: a truncated file is a real fault, not an absence.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// UI-only flags the sidebar renders for one session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The explicitly bound project directory. Absent for unbound
    /// sessions — the session cwd is a working directory, not a project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The approval gate policy the session runs under (`"autopilot"` or
    /// `"danger"`), as chosen in the access chip. Absent = the harness
    /// default. Stored as the mode's wire string; the harness parses it
    /// leniently and falls back to its default on unknown values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<String>,
    /// Plan mode active for this session. Absent = off. Restored on thread
    /// load so a resumed session keeps its read-only planning semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode: Option<bool>,
    /// Last plan file this session proposed (`<slug>-plan.md` under the
    /// global plans dir), kept for restore + execution handoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_file: Option<String>,
    /// A plan review card was pending (proposed, no verdict yet) when the
    /// session last settled; a restarted session re-surfaces the card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_review_pending: Option<bool>,
    /// Active git-worktree binding (`EnterWorktree`/`ExitWorktree`): the
    /// session is a fork whose cwd is the worktree; the original session
    /// file + cwd are kept so `ExitWorktree` can return. Absent = not in a
    /// worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeMeta>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub unread: bool,
    #[serde(default)]
    pub errored: bool,
    /// Compact display forms for registry slash turns (`/name args`), keyed
    /// by the user message's ordinal (0-based among user-role prompt messages)
    /// in the pi transcript. The transcript stores only the expanded
    /// macro/skill body, so the sidecar restores the send-time bubble on
    /// reload.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub registry_displays: HashMap<usize, String>,
}

/// Active git-worktree binding persisted in the session sidecar (see
/// `SessionMeta.worktree`). `original_session_path`/`original_cwd` are the
/// pre-enter state `ExitWorktree` returns to; `worktree_path`/`branch` name
/// the bound git worktree.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeMeta {
    pub worktree_path: String,
    pub branch: String,
    pub original_session_path: String,
    pub original_cwd: String,
}

/// The sidecar path for a session file: `<dir>/<id>.meta.json`.
pub fn meta_path(session_dir: &Path, session_path: &Path) -> PathBuf {
    let id = session_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    session_dir.join(format!("{id}.meta.json"))
}

/// Read the sidecar; a missing file yields the default (fresh session).
pub async fn load(session_dir: &Path, session_path: &Path) -> Result<SessionMeta, anyhow::Error> {
    let path = meta_path(session_dir, session_path);
    match tokio::fs::read(&path).await {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SessionMeta::default()),
        Err(e) => Err(e.into()),
    }
}

/// Write the sidecar atomically (write temp + rename) so a crash cannot
/// leave a truncated file behind.
pub async fn save(
    session_dir: &Path,
    session_path: &Path,
    meta: &SessionMeta,
) -> Result<(), anyhow::Error> {
    let path = meta_path(session_dir, session_path);
    let bytes = serde_json::to_vec_pretty(meta)?;
    let tmp = path.with_extension("meta.json.tmp");
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_sidecar_loads_as_fresh_session() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("abc.jsonl");
        let meta = load(dir.path(), &session).await.unwrap();
        assert!(meta.title.is_none() && !meta.pinned && !meta.archived);
    }

    #[tokio::test]
    async fn save_and_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("abc.jsonl");
        let meta = SessionMeta {
            title: Some("fix the widget".into()),
            project: Some("/p/a".into()),
            pinned: true,
            unread: true,
            ..Default::default()
        };
        save(dir.path(), &session, &meta).await.unwrap();
        let loaded = load(dir.path(), &session).await.unwrap();
        assert_eq!(loaded.title.as_deref(), Some("fix the widget"));
        assert_eq!(loaded.project.as_deref(), Some("/p/a"));
        assert!(loaded.pinned && loaded.unread && !loaded.archived && !loaded.errored);
    }

    #[tokio::test]
    async fn corrupt_sidecar_surfaces_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("abc.jsonl");
        tokio::fs::write(meta_path(dir.path(), &session), "{not json")
            .await
            .unwrap();
        assert!(load(dir.path(), &session).await.is_err());
    }

    #[tokio::test]
    async fn registry_displays_round_trip_and_default_empty() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("abc.jsonl");

        // Fresh sidecar: no registry displays.
        let fresh = load(dir.path(), &session).await.unwrap();
        assert!(fresh.registry_displays.is_empty());

        let meta = SessionMeta {
            registry_displays: [(1usize, "/gitwork:deliver fast".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        save(dir.path(), &session, &meta).await.unwrap();
        let loaded = load(dir.path(), &session).await.unwrap();
        assert_eq!(
            loaded.registry_displays.get(&1).map(String::as_str),
            Some("/gitwork:deliver fast")
        );
        assert!(!loaded.registry_displays.contains_key(&0));
    }
}
