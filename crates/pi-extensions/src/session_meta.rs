//! Per-session UI metadata stored beside the pi jsonl transcript.
//!
//! The pi core owns the conversation (jsonl session files); sidebar-only
//! flags — pin, archive, unread, error, display title — have no home in the
//! transcript schema, so they live in a small sidecar keyed by session id.
//! Loading tolerates a missing file (a fresh session has no sidecar yet) but
//! not a corrupt one: a truncated file is a real fault, not an absence.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

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
    /// The permission gate policy the session runs under (`"read-only"`,
    /// `"workspace-write"`, or `"danger-full-access"`), as chosen in the access
    /// chip. Absent = the harness default. Stored as the mode's wire string
    /// (field name kept as `approval_mode`); the harness parses it leniently
    /// and falls back to its bounded default on unknown values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<String>,
    /// Plan mode active for this session. Absent = off. Restored on thread
    /// load so a resumed session keeps its read-only planning semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode: Option<bool>,
    /// The reasoning effort the session runs at (`"high"` or `"max"`), as
    /// chosen in the model dropdown. Absent = the harness default (High).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Last plan file this session proposed (`<slug>-plan.md` under the
    /// global plans dir), kept for restore + execution handoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_file: Option<String>,
    /// A plan review card was pending (proposed, no verdict yet) when the
    /// session last settled; a restarted session re-surfaces the card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_review_pending: Option<bool>,
    /// Last execution plan the model published via `UpdatePlan`, persisted so
    /// it survives compaction (the transcript's tool calls are summarized
    /// away) and restarts. Serialized `agent::plan::PlanSnapshot`; `None`
    /// after the model clears its plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_snapshot: Option<serde_json::Value>,
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
    /// User-assigned tag shown as a chip on the sidebar row. Absent = no tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Compact display forms for registry slash turns (`/name args`), keyed
    /// by the user message's ordinal (0-based among user-role prompt messages)
    /// in the pi transcript. The transcript stores only the expanded
    /// macro/skill body, so the sidecar restores the send-time bubble on
    /// reload.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub registry_displays: HashMap<usize, String>,
    /// Agent attribution for user-role messages the human did not type
    /// (plan seeds, peer deliveries, member opening tasks), keyed by the
    /// same user-prompt ordinal as `registry_displays`. The host resolves
    /// `author` (a routing identity) to a display name at render time.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub user_attributions: HashMap<usize, UserAttributionMeta>,
}

/// One persisted attribution record (see `SessionMeta::user_attributions`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserAttributionMeta {
    /// Routing identity of the originating agent: `"lead"` for the main
    /// agent, the manifest / member name otherwise.
    pub author: String,
    /// The message entered via team peer delivery; the reload path
    /// rebuilds it as a team bubble.
    #[serde(default)]
    pub peer: bool,
    /// The send-time display form of the message (e.g. the unwrapped body of
    /// a peer delivery whose model-facing text is a wrapped `[from …]` form).
    /// The reload path re-attaches it so restored bubbles match the live view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_text: Option<String>,
}

/// Active git-worktree binding persisted in the session sidecar (see
/// `SessionMeta.worktree`). `original_session_path`/`original_cwd` are the
/// pre-enter state `ExitWorktree` returns to; `worktree_path`/`branch` name
/// the bound git worktree; `git_common_dir` is the owning repository's git
/// common dir (where linked-worktree commits write and `ExitWorktree`
/// removal runs — the worktree may belong to another repository than the
/// session cwd).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeMeta {
    pub worktree_path: String,
    pub branch: String,
    pub original_session_path: String,
    pub original_cwd: String,
    pub git_common_dir: String,
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
    // `<id>.meta.json.tmp`: `with_extension` would only replace the last
    // extension (`json`), yielding a surprising `<id>.meta.meta.json.tmp`.
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default()
    ));
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

/// Per-sidecar write lock keyed by sidecar path: every load→modify→save
/// cycle takes it before touching the file, so concurrent writers (archive /
/// pin / unread, engine title / approval-mode persists) can never interleave
/// and clobber each other's fields. The map is unbounded by design — one
/// entry per session ever written.
static WRITE_LOCKS: OnceLock<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

fn write_lock_for(session_dir: &Path, session_path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let key = meta_path(session_dir, session_path)
        .to_string_lossy()
        .into_owned();
    let map = WRITE_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(key)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Read-modify-write a sidecar under the per-session lock. A corrupt file
/// (load error) is treated as fresh and overwritten: the sidecar is
/// best-effort UI state and the transcript is authoritative, so the write
/// repairs the file while persisting the mutation. A missing file loads as
/// the fresh-session default and materializes on first write.
pub async fn update<F>(
    session_dir: &Path,
    session_path: &Path,
    mutate: F,
) -> Result<(), anyhow::Error>
where
    F: FnOnce(&mut SessionMeta) + Send,
{
    let lock = write_lock_for(session_dir, session_path);
    let _guard = lock.lock().await;
    // Self-heal: a corrupt sidecar loads as fresh and is overwritten by the
    // save below (the sidecar is best-effort UI state, the transcript is
    // authoritative). Logged so a self-heal is observable in production.
    let mut meta = load(session_dir, session_path)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(session = %session_path.display(), error = %error, "session sidecar unreadable; self-healing from defaults");
            SessionMeta::default()
        });
    mutate(&mut meta);
    save(session_dir, session_path, &meta).await
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
    async fn plan_snapshot_round_trips_and_defaults_absent() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("abc.jsonl");

        // Fresh sidecar: no persisted plan.
        let fresh = load(dir.path(), &session).await.unwrap();
        assert!(fresh.plan_snapshot.is_none());

        // Persist a snapshot (serialized `agent::plan::PlanSnapshot` shape).
        let snapshot = serde_json::json!({
            "explanation": null,
            "steps": [
                { "step": "investigate", "status": "completed" },
                { "step": "implement", "status": "in_progress" }
            ]
        });
        let mut meta = load(dir.path(), &session).await.unwrap();
        meta.plan_snapshot = Some(snapshot.clone());
        save(dir.path(), &session, &meta).await.unwrap();
        let loaded = load(dir.path(), &session).await.unwrap();
        assert_eq!(loaded.plan_snapshot, Some(snapshot));

        // Clearing (the model dropped its plan) removes the field entirely.
        let mut meta = loaded;
        meta.plan_snapshot = None;
        save(dir.path(), &session, &meta).await.unwrap();
        let cleared = load(dir.path(), &session).await.unwrap();
        assert!(cleared.plan_snapshot.is_none());
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

    #[tokio::test]
    async fn reasoning_effort_round_trips_and_defaults_absent() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("abc.jsonl");

        // Fresh sidecar: no persisted effort.
        let fresh = load(dir.path(), &session).await.unwrap();
        assert!(fresh.reasoning_effort.is_none());

        let meta = SessionMeta {
            reasoning_effort: Some("max".into()),
            ..Default::default()
        };
        save(dir.path(), &session, &meta).await.unwrap();
        let loaded = load(dir.path(), &session).await.unwrap();
        assert_eq!(loaded.reasoning_effort.as_deref(), Some("max"));
    }

    #[tokio::test]
    async fn update_round_trips_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("abc.jsonl");
        update(dir.path(), &session, |meta| meta.archived = true)
            .await
            .unwrap();
        let meta = load(dir.path(), &session).await.unwrap();
        assert!(meta.archived);
    }

    /// Two read-modify-write cycles racing the same sidecar must both
    /// survive: without the per-session lock one writer's stale load
    /// clobbers the other's field (the archive//exit lost-update bug).
    #[tokio::test]
    async fn update_serializes_concurrent_writers() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("abc.jsonl");
        let (a, b) = tokio::join!(
            update(dir.path(), &session, |meta| meta.archived = true),
            update(dir.path(), &session, |meta| meta.pinned = true),
        );
        a.unwrap();
        b.unwrap();
        let meta = load(dir.path(), &session).await.unwrap();
        assert!(meta.archived && meta.pinned, "lost update: {meta:?}");
    }

    /// A corrupt sidecar must not brick the session: `update` overwrites it
    /// from the fresh-session default while persisting the mutation.
    #[tokio::test]
    async fn update_self_heals_corrupt_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("abc.jsonl");
        tokio::fs::write(meta_path(dir.path(), &session), "{\"broken\":")
            .await
            .unwrap();
        update(dir.path(), &session, |meta| meta.archived = true)
            .await
            .unwrap();
        let meta = load(dir.path(), &session).await.unwrap();
        assert!(meta.archived);
    }
}
