// Session repository: directory-scoped create / open / list / delete / fork /
// branch over JSONL session files — the TS `SessionRepository` surface for a
// per-cwd session folder. New and branched sessions defer their file to the
// first assistant message (TS `_persist`), so an empty session never appears
// in `list`; `fork_from` materializes immediately with the source as parent.

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

    /// Fork a session from another project into this repository — the TS
    /// `forkFrom`: a fresh id and timestamp, the target cwd, the source file
    /// path as `parentSession`, and every non-header entry copied verbatim.
    /// Unlike a new session the fork materializes immediately (TS writes the
    /// header and entries eagerly).
    pub async fn fork_from(
        &self,
        source: &Path,
        target_cwd: &str,
    ) -> Result<Session<JsonlSessionStorage>, anyhow::Error> {
        let source_storage = JsonlSessionStorage::open(source).await?;
        let new_id = uuid::Uuid::new_v4().to_string();
        let path = self.dir.join(session_file_name(&new_id));
        let storage = JsonlSessionStorage::create(
            &path,
            JsonlSessionMetadata {
                id: new_id,
                cwd: target_cwd.to_string(),
                created_at: chrono::Utc::now(),
                parent_session_path: Some(source.to_string_lossy().into_owned()),
                metadata: source_storage.metadata.metadata.clone(),
            },
        )
        .await?;
        for entry in source_storage.get_entries(Default::default()).await? {
            storage.append_entry(&entry).await?;
        }
        Ok(Session::new(storage))
    }

    /// Fork a single root→leaf path into a new session — the TS
    /// `createBranchedSession`. Label entries are stripped and the retained
    /// path re-chained (a label is a real tree node whose removal would orphan
    /// its subtree), then label entries are rebuilt for the retained targets,
    /// chained after the tail. The new session defers its file like any new
    /// session: it materializes on the first assistant message.
    pub async fn create_branched_session(
        &self,
        source: &Path,
        leaf_id: &str,
    ) -> Result<Session<JsonlSessionStorage>, anyhow::Error> {
        let source_storage = JsonlSessionStorage::open(source).await?;
        let branch = source_storage.get_path(Some(leaf_id)).await?;
        if branch.is_empty() {
            anyhow::bail!("entry {leaf_id} not found");
        }

        // Strip labels, re-chaining parents so the retained path stays linear.
        let mut retained: Vec<SessionTreeEntry> = Vec::new();
        let mut parent: Option<String> = None;
        for mut entry in branch {
            if matches!(entry, SessionTreeEntry::Label { .. }) {
                continue;
            }
            entry.set_parent_id(parent.take());
            parent = Some(entry.id().to_string());
            retained.push(entry);
        }

        // Rebuild the FINAL label state per target (TS `labelsById`): labels
        // apply in entry order, later labels replace earlier ones, and a
        // `label: null` clears the target's label. Only retained targets keep
        // their labels; the rebuilt entries chain after the retained tail.
        let retained_ids: std::collections::HashSet<String> =
            retained.iter().map(|e| e.id().to_string()).collect();
        let mut final_labels: std::collections::HashMap<
            String,
            (String, chrono::DateTime<chrono::Utc>),
        > = std::collections::HashMap::new();
        for e in source_storage.get_entries(Default::default()).await?.iter() {
            if let SessionTreeEntry::Label {
                target_id,
                label,
                timestamp,
                ..
            } = e
            {
                if !retained_ids.contains(target_id) {
                    continue;
                }
                match label {
                    Some(label) => {
                        final_labels.insert(target_id.clone(), (label.clone(), *timestamp));
                    }
                    None => {
                        final_labels.remove(target_id);
                    }
                }
            }
        }
        let labels: Vec<(String, String, chrono::DateTime<chrono::Utc>)> = final_labels
            .into_iter()
            .map(|(target, (label, timestamp))| (target, label, timestamp))
            .collect();
        let mut tail_parent = retained.last().map(|e| e.id().to_string());
        for (target_id, label, timestamp) in labels {
            retained.push(SessionTreeEntry::Label {
                id: uuid::Uuid::new_v4().to_string(),
                parent_id: tail_parent,
                timestamp,
                target_id,
                label: Some(label),
            });
            tail_parent = retained.last().map(|e| e.id().to_string());
        }

        let new_id = uuid::Uuid::new_v4().to_string();
        let path = self.dir.join(session_file_name(&new_id));
        let storage = JsonlSessionStorage::create_deferred(
            &path,
            JsonlSessionMetadata {
                id: new_id,
                cwd: source_storage.metadata.cwd.clone(),
                created_at: chrono::Utc::now(),
                parent_session_path: Some(source.to_string_lossy().into_owned()),
                metadata: source_storage.metadata.metadata.clone(),
            },
        )
        .await?;
        for entry in retained {
            storage.append_entry(&entry).await?;
        }
        Ok(Session::new(storage))
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
    async fn test_repository_fork_from_uses_source_path_as_parent() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SessionRepository::new(dir.path());

        let session = repo.create(meta()).await.unwrap();
        session
            .append_message(AgentMessage::user("shared history"))
            .await
            .unwrap();
        session
            .append_message(assistant("shared reply"))
            .await
            .unwrap();
        let listed = repo.list().await.unwrap();
        let source_path = listed[0].path.clone();
        let source_id = listed[0].id.clone();

        // Cross-project fork: target cwd differs, parentSession is the source
        // file path, and the fork carries the full history.
        let target_cwd = "/other/project";
        let fork = repo.fork_from(&source_path, target_cwd).await.unwrap();
        assert_eq!(fork.storage().metadata.cwd, target_cwd);
        assert_eq!(
            fork.storage().metadata.parent_session_path.as_deref(),
            Some(source_path.to_str().unwrap()),
            "parentSession must be the source file path, not its id"
        );
        assert_ne!(fork.storage().metadata.id, source_id);
        assert_eq!(fork.build_context_entries().await.unwrap().len(), 2);

        // The fork materialized immediately and appears in list.
        let listed = repo.list().await.unwrap();
        assert_eq!(listed.len(), 2);
        let fork_summary = listed.iter().find(|s| s.id != source_id).unwrap();
        assert_eq!(
            fork_summary.parent_session_path.as_deref(),
            Some(source_path.to_str().unwrap())
        );
    }

    #[tokio::test]
    async fn test_repository_create_branched_session_keeps_path_and_labels() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SessionRepository::new(dir.path());

        let session = repo.create(meta()).await.unwrap();
        session
            .append_message(AgentMessage::user("u1"))
            .await
            .unwrap();
        session.append_message(assistant("a1")).await.unwrap();
        session
            .append_message(AgentMessage::user("u2"))
            .await
            .unwrap();
        session.append_message(assistant("a2")).await.unwrap();
        // A label on the first reply and a second branch exploring elsewhere.
        let entries = session
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let a1_id = entries
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::Assistant { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .unwrap();
        session
            .append_label(&a1_id, Some("checkpoint".into()))
            .await
            .unwrap();

        // Fork the path up to a1: u1 + a1, labels re-chained.
        let fork = repo
            .create_branched_session(&repo.list().await.unwrap()[0].path, &a1_id)
            .await
            .unwrap();
        let fork_entries = fork
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let fork_ids: Vec<&str> = fork_entries.iter().map(|e| e.id()).collect();
        assert_eq!(fork_ids.len(), 3, "u1 + a1 + rebuilt label");
        // The retained path is linear: a1's parent is u1, not the stripped
        // label.
        let fork_a1 = fork_entries
            .iter()
            .find(|e| {
                matches!(
                    e,
                    SessionTreeEntry::Message {
                        message: AgentMessage::Assistant { .. },
                        ..
                    }
                )
            })
            .unwrap();
        let fork_u1 = fork_entries
            .iter()
            .find(|e| {
                matches!(
                    e,
                    SessionTreeEntry::Message {
                        message: AgentMessage::User { .. },
                        ..
                    }
                )
            })
            .unwrap();
        assert_eq!(fork_a1.parent_id(), Some(fork_u1.id()));
        // The rebuilt label points at the retained a1 and chains after it.
        let fork_label = fork_entries
            .iter()
            .find(|e| matches!(e, SessionTreeEntry::Label { .. }))
            .expect("label rebuilt");
        let SessionTreeEntry::Label {
            target_id, label, ..
        } = fork_label
        else {
            unreachable!()
        };
        assert_eq!(target_id, fork_a1.id());
        assert_eq!(label.as_deref(), Some("checkpoint"));
        assert_eq!(fork_label.parent_id(), Some(fork_a1.id()));
    }

    /// A fork keeps only the FINAL label per target: a rename replaces the
    /// old label, and a `label: null` clear removes it entirely.
    #[tokio::test]
    async fn test_fork_keeps_final_label_state_per_target() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SessionRepository::new(dir.path());
        let session = repo.create(meta()).await.unwrap();
        session
            .append_message(AgentMessage::user("u1"))
            .await
            .unwrap();
        session.append_message(assistant("a1")).await.unwrap();
        let entries = session
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let a1_id = entries
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::Assistant { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .unwrap();
        // Label, rename, then clear: only the clear survives on the target.
        session
            .append_label(&a1_id, Some("old".into()))
            .await
            .unwrap();
        session
            .append_label(&a1_id, Some("new".into()))
            .await
            .unwrap();
        session.append_label(&a1_id, None).await.unwrap();

        let fork = repo
            .create_branched_session(&repo.list().await.unwrap()[0].path, &a1_id)
            .await
            .unwrap();
        let fork_entries = fork
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        assert!(
            !fork_entries
                .iter()
                .any(|e| matches!(e, SessionTreeEntry::Label { .. })),
            "cleared labels must not survive the fork: {fork_entries:?}"
        );

        // Rename only: the fork keeps just the latest value.
        let session2 = repo.create(meta()).await.unwrap();
        session2
            .append_message(AgentMessage::user("u1"))
            .await
            .unwrap();
        session2.append_message(assistant("a1")).await.unwrap();
        let entries = session2
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let a1_id = entries
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::Assistant { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .unwrap();
        session2
            .append_label(&a1_id, Some("old".into()))
            .await
            .unwrap();
        session2
            .append_label(&a1_id, Some("new".into()))
            .await
            .unwrap();
        let fork = repo
            .create_branched_session(session2.storage().path(), &a1_id)
            .await
            .unwrap();
        let fork_entries = fork
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let labels: Vec<&str> = fork_entries
            .iter()
            .filter_map(|e| match e {
                SessionTreeEntry::Label { label: Some(l), .. } => Some(l.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["new"], "{fork_entries:?}");
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
    async fn test_repository_branch_unknown_leaf_errors() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SessionRepository::new(dir.path());
        let session = repo.create(meta()).await.unwrap();
        session.append_message(assistant("hi")).await.unwrap();
        let listed = repo.list().await.unwrap();
        let err = match repo
            .create_branched_session(&listed[0].path, "no-such-entry")
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("expected error for unknown leaf"),
        };
        assert!(err.to_string().contains("not found"), "{err}");
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

    #[tokio::test]
    async fn test_repository_forks_inherit_the_source_header_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SessionRepository::new(dir.path());
        let session = repo
            .create(meta_with(serde_json::json!({ "host": "vscode" })))
            .await
            .unwrap();
        session
            .append_message(AgentMessage::user("u1"))
            .await
            .unwrap();
        session.append_message(assistant("a1")).await.unwrap();
        let listed = repo.list().await.unwrap();
        let source_path = listed[0].path.clone();

        // fork_from: eager materialization, the header metadata carries over.
        let fork = repo.fork_from(&source_path, "/other").await.unwrap();
        assert_eq!(
            fork.storage().metadata.metadata,
            Some(serde_json::json!({ "host": "vscode" }))
        );

        // create_branched_session: deferred materialization, the in-memory
        // header metadata carries over (the file writes it on materialize).
        let entries = session
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        let a1_id = entries
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Message {
                    message: AgentMessage::Assistant { .. },
                    id,
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .expect("assistant entry");
        let branch = repo
            .create_branched_session(&source_path, &a1_id)
            .await
            .unwrap();
        assert_eq!(
            branch.storage().metadata.metadata,
            Some(serde_json::json!({ "host": "vscode" }))
        );
    }
}
