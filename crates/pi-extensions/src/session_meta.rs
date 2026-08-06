//! Per-session UI metadata stored beside the pi jsonl transcript.
//!
//! The pi core owns the conversation (jsonl session files); sidebar-only
//! flags — pin, archive, unread, error, display title — have no home in the
//! transcript schema, so they live in a small sidecar keyed by session id.
//! Loading tolerates a missing file (a fresh session has no sidecar yet) but
//! not a corrupt one: a truncated file is a real fault, not an absence.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// UI-only flags the sidebar renders for one session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub unread: bool,
    #[serde(default)]
    pub errored: bool,
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
            pinned: true,
            unread: true,
            ..Default::default()
        };
        save(dir.path(), &session, &meta).await.unwrap();
        let loaded = load(dir.path(), &session).await.unwrap();
        assert_eq!(loaded.title.as_deref(), Some("fix the widget"));
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
}
