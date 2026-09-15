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
    /// Additional working directories granted to this session (multi-
    /// root). Restored on thread load so a resumed session's sandbox
    /// fence keeps admitting them. Empty (the default) = single-cwd.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub working_directories: Vec<String>,
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
    /// away) and restarts. Serialized `manox_agent::plan::PlanSnapshot`; `None`
    /// after the model clears its plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_snapshot: Option<serde_json::Value>,
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
    /// Unix seconds of the last human-authored prompt or steer. The sidebar's
    /// recency key: no other write — assistant output, tool results, injected
    /// agent turns, titles, flags — advances it. Absent = never stamped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interacted_at: Option<i64>,
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

/// The sidecar path for a session file: `<dir>/<id>.meta.json`.
pub fn meta_path(session_dir: &Path, session_path: &Path) -> PathBuf {
    let id = session_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    session_dir.join(format!("{id}.meta.json"))
}

/// Read the sidecar; a missing file yields the default (fresh session).
/// A (size, mtime) fingerprint cache fronts the read: an unchanged sidecar
/// costs one `stat`, and every successful `save` refreshes the entry under
/// the same path so this process never re-reads what it just wrote.
pub async fn load(session_dir: &Path, session_path: &Path) -> Result<SessionMeta, anyhow::Error> {
    let path = meta_path(session_dir, session_path);
    if let Some(hit) = cached(&path).await {
        return Ok((*hit).clone());
    }
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SessionMeta::default());
        }
        Err(e) => return Err(e.into()),
    };
    let meta: SessionMeta = serde_json::from_slice(&bytes)?;
    remember(&path, bytes.len() as u64, mtime_of(&path).await, &meta);
    Ok(meta)
}

/// Write the sidecar atomically (write temp + rename) so a crash cannot
/// leave a truncated file behind. Serialized cross-process: two manox
/// processes saving the same sidecar would otherwise interleave on the
/// shared tmp sibling.
pub async fn save(
    session_dir: &Path,
    session_path: &Path,
    meta: &SessionMeta,
) -> Result<(), anyhow::Error> {
    let path = meta_path(session_dir, session_path);
    let _file_lock = acquire_sidecar_lock(&path).await?;
    save_unlocked(&path, meta).await
}

/// The lock-free write core — callers must already hold the sidecar's
/// cross-process flock (flock is per open file description: re-locking
/// from the same process self-deadlocks). The lock acquire has already
/// materialized the directory: a deferred-fresh session's transcript dir
/// may not exist yet (the journal materializes at the first turn) while
/// its sidecar is already addressable.
async fn save_unlocked(path: &Path, meta: &SessionMeta) -> Result<(), anyhow::Error> {
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
    remember(path, bytes.len() as u64, mtime_of(path).await, meta);
    Ok(())
}

/// Bounded exclusive flock over the sidecar's sibling `<name>.lock` —
/// spans a whole read-modify-write cycle so a concurrent process's fields
/// cannot be lost to a stale read.
async fn acquire_sidecar_lock(path: &Path) -> Result<crate::fs_lock::FileLock, anyhow::Error> {
    crate::fs_lock::lock_exclusive_async(
        &crate::fs_lock::lock_path_for(path),
        std::time::Duration::from_secs(2),
    )
    .await
    .map_err(|error| anyhow::anyhow!("sidecar lock unavailable ({}): {error}", path.display()))
}

/// One cached sidecar: its (size, mtime_ns) fingerprint and parsed content.
type SidecarCache = HashMap<PathBuf, (u64, i64, Arc<SessionMeta>)>;

/// The sidecar fingerprint cache. Purely an accelerator — a stale entry
/// self-heals on the next fingerprint mismatch, and correctness never
/// depends on it.
static FINGERPRINTS: OnceLock<StdMutex<SidecarCache>> = OnceLock::new();

fn fingerprints() -> &'static StdMutex<SidecarCache> {
    FINGERPRINTS.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// The cached sidecar when the file's (size, mtime) still matches.
async fn cached(path: &Path) -> Option<Arc<SessionMeta>> {
    let stat = tokio::fs::metadata(path).await.ok()?;
    let (size, mtime) = (stat.len(), mtime_of(path).await);
    let map = fingerprints().lock().unwrap_or_else(|e| e.into_inner());
    match map.get(path) {
        Some((cached_size, cached_mtime, meta))
            if *cached_size == size && *cached_mtime == mtime =>
        {
            Some(Arc::clone(meta))
        }
        _ => None,
    }
}

fn remember(path: &Path, size: u64, mtime: i64, meta: &SessionMeta) {
    fingerprints()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_path_buf(), (size, mtime, Arc::new(meta.clone())));
}

async fn mtime_of(path: &Path) -> i64 {
    tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|stat| stat.modified().ok())
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i64)
        .unwrap_or(0)
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
/// the fresh-session default and materializes on first write. The
/// cross-process flock spans the whole cycle (another manox process's
/// fields must not be lost to a stale read).
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
    let path = meta_path(session_dir, session_path);
    let _file_lock = acquire_sidecar_lock(&path).await?;
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
    save_unlocked(&path, &meta).await
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

    /// A deferred-fresh session's sidecar is addressable BEFORE its
    /// journal directory materializes — both `save` and `update` must
    /// create the directory themselves (a017e77f; the lock acquire must
    /// not ENOENT on the missing parent).
    #[tokio::test]
    async fn save_into_a_missing_directory_materializes_it() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let session = sessions.join("abc.jsonl");

        save(&sessions, &session, &SessionMeta::default())
            .await
            .unwrap();
        let loaded = load(&sessions, &session).await.unwrap();
        assert!(loaded.title.is_none() && !loaded.pinned && !loaded.archived);

        let missing_sub = dir.path().join("sessions/subagents");
        let sub_session = missing_sub.join("def.jsonl");
        update(&missing_sub, &sub_session, |meta| meta.pinned = true)
            .await
            .unwrap();
        assert!(load(&missing_sub, &sub_session).await.unwrap().pinned);
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

        // Persist a snapshot (serialized `manox_agent::plan::PlanSnapshot` shape).
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

    /// The interaction stamp round-trips and stays absent until written: it is
    /// the sidebar's only recency key, so a lost or defaulted read would
    /// silently re-sort rows.
    #[tokio::test]
    async fn interacted_at_round_trips_and_defaults_absent() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("abc.jsonl");
        assert!(
            load(dir.path(), &session)
                .await
                .unwrap()
                .interacted_at
                .is_none()
        );

        let mut meta = load(dir.path(), &session).await.unwrap();
        meta.interacted_at = Some(1_700_000_500);
        save(dir.path(), &session, &meta).await.unwrap();
        assert_eq!(
            load(dir.path(), &session).await.unwrap().interacted_at,
            Some(1_700_000_500)
        );

        update(dir.path(), &session, |meta| meta.unread = true)
            .await
            .unwrap();
        let settled = load(dir.path(), &session).await.unwrap();
        assert_eq!(
            settled.interacted_at,
            Some(1_700_000_500),
            "an unrelated flag write must not drop the interaction stamp"
        );
    }
}
