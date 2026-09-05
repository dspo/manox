// Session repository: directory-scoped create / open / list / delete / fork /
// branch over JSONL session files — the TS `SessionRepository` surface for a
// per-cwd session folder. New and branched sessions defer their file to the
// first assistant message (TS `_persist`), so an empty session never appears
// in `list`.

use std::path::{Path, PathBuf};

use crate::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};
use crate::session::{Session, SessionStorage, SessionTreeEntry};
use crate::types::AgentMessage;

/// A session summary as `list` reports it — the TS non-UI core `SessionInfo`.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    /// Working directory where the session was started.
    pub cwd: String,
    /// The latest `session_info` display name, when one was set.
    pub name: Option<String>,
    /// Path of the session this one forked from, when it has one.
    pub parent_session_path: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Last message activity; falls back to the header timestamp.
    pub modified_at: chrono::DateTime<chrono::Utc>,
    /// Number of `message` entries (user, assistant, and tool results).
    pub message_count: usize,
    /// Text of the first user message, or `"(no messages)"`.
    pub first_message: String,
    /// All user and assistant text contents joined by spaces.
    pub all_messages_text: String,
    /// Free-form header metadata (agent identity, environment snapshot),
    /// surfaced so host layers can route sessions without reopening files.
    pub metadata: Option<serde_json::Value>,
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

    /// Create a new session in the repository directory. The file is deferred
    /// until the first assistant message, so an empty session is invisible to
    /// [`Self::list`] and never touches disk — TS `newSession` + `_persist`.
    pub async fn create(
        &self,
        metadata: JsonlSessionMetadata,
    ) -> Result<Session<JsonlSessionStorage>, anyhow::Error> {
        let path = self.dir.join(session_file_name(&metadata.id));
        let storage = JsonlSessionStorage::create_deferred(&path, metadata).await?;
        Ok(Session::new(storage))
    }

    /// Open a session file by path.
    pub async fn open(&self, path: &Path) -> Result<Session<JsonlSessionStorage>, anyhow::Error> {
        let storage = JsonlSessionStorage::open(path).await?;
        Ok(Session::new(storage))
    }

    /// List every session file in the repository directory, newest activity
    /// first. A corrupt file surfaces as missing — callers that need it can
    /// `open` and see the error.
    pub async fn list(&self) -> Result<Vec<SessionInfo>, anyhow::Error> {
        let mut out = Vec::new();
        for path in session_files(&self.dir).await? {
            if let Ok(info) = build_session_info(&path).await {
                out.push(info);
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.modified_at));
        Ok(out)
    }

    /// Delete a session file.
    pub async fn delete(&self, path: &Path) -> Result<(), anyhow::Error> {
        tokio::fs::remove_file(path)
            .await
            .map_err(|e| anyhow::anyhow!("failed to delete session {}: {e}", path.display()))
    }

    /// The [`SessionInfo`] for one transcript, without scanning the
    /// directory. An explicit open only ever needs this — a store-wide
    /// [`Self::list`] is O(every file) and must never gate it.
    pub async fn info(&self, path: &Path) -> Result<SessionInfo, anyhow::Error> {
        build_session_info(path).await
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

/// Build the [`SessionInfo`] for one file, mirroring the TS `buildSessionInfo`:
/// the latest session name, message count, first user text, all text, and the
/// last activity time (falling back to the header timestamp). A corrupt file
/// yields `None` and is skipped by `list`.
async fn build_session_info(path: &Path) -> Result<SessionInfo, anyhow::Error> {
    let storage = JsonlSessionStorage::open(path).await?;
    let entries = storage.get_entries(Default::default()).await?;
    let mut name: Option<String> = None;
    let mut message_count = 0usize;
    let mut first_message = String::new();
    let mut all_texts: Vec<String> = Vec::new();
    let mut last_activity: Option<chrono::DateTime<chrono::Utc>> = None;
    for entry in &entries {
        match entry {
            SessionTreeEntry::SessionInfo { name: n, .. } => {
                let trimmed = n.as_deref().unwrap_or("").trim();
                name = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            }
            SessionTreeEntry::Message {
                message, timestamp, ..
            } => {
                message_count += 1;
                last_activity = Some(last_activity.map_or(*timestamp, |t| t.max(*timestamp)));
                let text = message_text(message);
                if text.is_empty() {
                    continue;
                }
                if first_message.is_empty() && matches!(message, AgentMessage::User { .. }) {
                    first_message = text.clone();
                }
                all_texts.push(text);
            }
            _ => {}
        }
    }
    let modified_at = last_activity.unwrap_or(storage.metadata.created_at);
    Ok(SessionInfo {
        path: path.to_path_buf(),
        id: storage.metadata.id.clone(),
        cwd: storage.metadata.cwd.clone(),
        name,
        parent_session_path: storage.metadata.parent_session_path.clone(),
        created_at: storage.metadata.created_at,
        modified_at,
        message_count,
        first_message: if first_message.is_empty() {
            "(no messages)".to_string()
        } else {
            first_message
        },
        all_messages_text: all_texts.join(" "),
        metadata: storage.metadata.metadata.clone(),
    })
}

/// All text blocks of a message, joined by newlines. Shell executions
/// contribute nothing: the session list searches conversation text, and a
/// command's output would swamp it.
fn message_text(message: &AgentMessage) -> String {
    let content = match message {
        AgentMessage::User { content, .. }
        | AgentMessage::Assistant { content, .. }
        | AgentMessage::ToolResult { content, .. }
        | AgentMessage::Custom { content, .. } => content,
        AgentMessage::BashExecution { .. } => return String::new(),
    };
    content
        .iter()
        .filter_map(|b| match b {
            crate::types::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ContentBlock;

    fn meta() -> JsonlSessionMetadata {
        JsonlSessionMetadata {
            id: uuid::Uuid::new_v4().to_string(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata: None,
        }
    }

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: text.into(),
                signature: None,
            }],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(crate::types::StopReason::Stop),
            usage: Default::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_repository_create_defers_file_until_first_assistant() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SessionRepository::new(dir.path());

        let session = repo.create(meta()).await.unwrap();
        // An empty session (no assistant message yet) never appears in list.
        session
            .append_message(AgentMessage::user("first"))
            .await
            .unwrap();
        session.set_session_name("my session").await.unwrap();
        assert!(repo.list().await.unwrap().is_empty(), "file deferred");

        // The first assistant message materializes the file.
        session.append_message(assistant("hello")).await.unwrap();
        let listed = repo.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].message_count, 2, "user + assistant");
        assert_eq!(listed[0].name.as_deref(), Some("my session"));
        assert_eq!(listed[0].first_message, "first");
        assert_eq!(listed[0].all_messages_text, "first hello");

        // The materialized file reopens with the same content.
        let reopened = repo.open(&listed[0].path).await.unwrap();
        assert_eq!(reopened.build_context_entries().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn test_repository_list_sorts_by_modified_desc() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SessionRepository::new(dir.path());

        let older = repo.create(meta()).await.unwrap();
        older
            .append_message(AgentMessage::user("older"))
            .await
            .unwrap();
        older.append_message(assistant("old reply")).await.unwrap();

        let newer = repo.create(meta()).await.unwrap();
        newer
            .append_message(AgentMessage::user("newer"))
            .await
            .unwrap();
        newer.append_message(assistant("new reply")).await.unwrap();

        let listed = repo.list().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].first_message, "newer");
        assert_eq!(listed[1].first_message, "older");
    }

    #[tokio::test]
    async fn test_repository_list_skips_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("corrupt.jsonl"), "not a session file\n")
            .await
            .unwrap();
        let repo = SessionRepository::new(dir.path());
        assert!(
            repo.list().await.unwrap().is_empty(),
            "corrupt file skipped"
        );
    }

    #[tokio::test]
    async fn test_repository_delete_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SessionRepository::new(dir.path());
        let session = repo.create(meta()).await.unwrap();
        session.append_message(assistant("hi")).await.unwrap();
        let listed = repo.list().await.unwrap();
        assert_eq!(listed.len(), 1);

        repo.delete(&listed[0].path).await.unwrap();
        assert!(repo.list().await.unwrap().is_empty());
    }

    fn meta_with(metadata: serde_json::Value) -> JsonlSessionMetadata {
        JsonlSessionMetadata {
            metadata: Some(metadata),
            ..meta()
        }
    }

    #[tokio::test]
    async fn test_repository_list_surfaces_header_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SessionRepository::new(dir.path());
        let session = repo
            .create(meta_with(serde_json::json!({ "host": "vscode" })))
            .await
            .unwrap();
        session
            .append_message(AgentMessage::user("first"))
            .await
            .unwrap();
        session.append_message(assistant("hello")).await.unwrap();
        let listed = repo.list().await.unwrap();
        assert_eq!(
            listed[0].metadata,
            Some(serde_json::json!({ "host": "vscode" }))
        );
    }
}
