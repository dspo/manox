// Session repository: directory-scoped create / open / list / delete / fork /
// search over JSONL session files — the TS `SessionRepository` surface for a
// per-cwd session folder.

use std::path::{Path, PathBuf};

use crate::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};
use crate::session::{Session, SessionStorage, SessionTreeEntry};

/// A summary of a session found by [`SessionRepository::list`].
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub id: String,
    pub name: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub entry_count: usize,
}

/// A search hit: the session file and the entry that matched.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: PathBuf,
    pub entry_id: String,
}

/// A repository over the JSONL session files in one directory.
pub struct SessionRepository {
    dir: PathBuf,
}

impl SessionRepository {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        SessionRepository { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Create a new session file in the repository directory.
    pub async fn create(
        &self,
        metadata: JsonlSessionMetadata,
    ) -> Result<Session<JsonlSessionStorage>, anyhow::Error> {
        let path = self.dir.join(session_file_name(&metadata.id));
        let storage = JsonlSessionStorage::create(&path, metadata).await?;
        Ok(Session::new(storage))
    }

    /// Open a session file by path.
    pub async fn open(&self, path: &Path) -> Result<Session<JsonlSessionStorage>, anyhow::Error> {
        let storage = JsonlSessionStorage::open(path).await?;
        Ok(Session::new(storage))
    }

    /// List every session file in the repository directory.
    pub async fn list(&self) -> Result<Vec<SessionSummary>, anyhow::Error> {
        let mut out = Vec::new();
        for path in session_files(&self.dir).await? {
            // A corrupt file surfaces in `list` as missing; callers that need
            // it can `open` and see the error.
            if let Ok(storage) = JsonlSessionStorage::open(&path).await {
                let entries = storage.get_entries().await.unwrap_or_default();
                let name = entries.iter().rev().find_map(|e| match e {
                    SessionTreeEntry::SessionInfo { name, .. } => name.clone(),
                    _ => None,
                });
                out.push(SessionSummary {
                    path,
                    id: storage.metadata.id.clone(),
                    name,
                    created_at: storage.metadata.created_at,
                    entry_count: entries.len(),
                });
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        Ok(out)
    }

    /// Delete a session file.
    pub async fn delete(&self, path: &Path) -> Result<(), anyhow::Error> {
        tokio::fs::remove_file(path)
            .await
            .map_err(|e| anyhow::anyhow!("failed to delete session {}: {e}", path.display()))
    }

    /// Fork a session: copy the file under a fresh id, recording the source
    /// as `parentSession`, and open the fork.
    pub async fn fork(&self, source: &Path) -> Result<Session<JsonlSessionStorage>, anyhow::Error> {
        let source_storage = JsonlSessionStorage::open(source).await?;
        let new_id = uuid::Uuid::new_v4().to_string();
        let path = self.dir.join(session_file_name(&new_id));

        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": new_id,
            "timestamp": source_storage.metadata.created_at,
            "cwd": source_storage.metadata.cwd,
            "parentSession": source_storage.metadata.id,
        });
        let mut body = serde_json::to_string(&header)?;
        let lines = tokio::fs::read_to_string(source).await?;
        // Drop the source header line; append every entry line verbatim.
        body.push('\n');
        for line in lines.lines().skip(1).filter(|l| !l.trim().is_empty()) {
            body.push_str(line);
            body.push('\n');
        }
        tokio::fs::write(&path, body).await?;
        let storage = JsonlSessionStorage::open(&path).await?;
        Ok(Session::new(storage))
    }

    /// Case-insensitive text search over every session's serialized entries.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>, anyhow::Error> {
        let needle = query.to_lowercase();
        let mut hits = Vec::new();
        for path in session_files(&self.dir).await? {
            let Ok(storage) = JsonlSessionStorage::open(&path).await else {
                continue;
            };
            let Ok(entries) = storage.get_entries().await else {
                continue;
            };
            for entry in entries {
                let haystack = serde_json::to_string(&entry)
                    .unwrap_or_default()
                    .to_lowercase();
                if haystack.contains(&needle) {
                    hits.push(SearchHit {
                        path: path.clone(),
                        entry_id: entry.id().to_string(),
                    });
                }
            }
        }
        Ok(hits)
    }
}

fn session_file_name(id: &str) -> String {
    format!("{id}.jsonl")
}

async fn session_files(dir: &Path) -> Result<Vec<PathBuf>, anyhow::Error> {
    let mut out = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "jsonl") && path.is_file() {
            out.push(path);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentMessage;

    fn meta() -> JsonlSessionMetadata {
        JsonlSessionMetadata {
            id: uuid::Uuid::new_v4().to_string(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata: None,
        }
    }

    #[tokio::test]
    async fn test_repository_create_list_open() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SessionRepository::new(dir.path());

        let session = repo.create(meta()).await.unwrap();
        session
            .append_message(AgentMessage::user("first"))
            .await
            .unwrap();
        session.set_session_name("my session").await.unwrap();

        let listed = repo.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].entry_count, 2, "message + session_info");
        assert_eq!(listed[0].name.as_deref(), Some("my session"));

        let reopened = repo.open(&listed[0].path).await.unwrap();
        assert_eq!(reopened.build_context_entries().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_repository_fork_copies_entries_with_new_identity() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SessionRepository::new(dir.path());

        let session = repo.create(meta()).await.unwrap();
        session
            .append_message(AgentMessage::user("shared history"))
            .await
            .unwrap();
        let listed = repo.list().await.unwrap();
        let source_path = listed[0].path.clone();
        let source_id = listed[0].id.clone();

        let fork = repo.fork(&source_path).await.unwrap();
        let entries = fork.build_context_entries().await.unwrap();
        assert_eq!(entries.len(), 1, "fork carries the message");
        assert!(
            fork.leaf_id().await.unwrap().is_some(),
            "the fork's cursor follows the copied message"
        );

        // The fork is a separate file with its own id; the source keeps its
        // own entry count.
        let listed = repo.list().await.unwrap();
        assert_eq!(listed.len(), 2);
        let fork_summary = listed.iter().find(|s| s.id != source_id).unwrap();
        assert!(fork_summary.path != source_path);
        let fork_storage = JsonlSessionStorage::open(&fork_summary.path).await.unwrap();
        assert_eq!(
            fork_storage.metadata.parent_session_path.as_deref(),
            Some(source_id.as_str()),
            "fork records its parent"
        );
    }

    #[tokio::test]
    async fn test_repository_search_finds_entry_text() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SessionRepository::new(dir.path());

        let session = repo.create(meta()).await.unwrap();
        session
            .append_message(AgentMessage::user("refactor the payment gateway"))
            .await
            .unwrap();

        let hits = repo.search("PAYMENT").await.unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        let misses = repo.search("nothing-matches-this").await.unwrap();
        assert!(misses.is_empty());
    }

    #[tokio::test]
    async fn test_repository_delete_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SessionRepository::new(dir.path());
        repo.create(meta()).await.unwrap();
        let listed = repo.list().await.unwrap();
        assert_eq!(listed.len(), 1);

        repo.delete(&listed[0].path).await.unwrap();
        assert!(repo.list().await.unwrap().is_empty());
    }
}
