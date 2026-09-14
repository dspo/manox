// Session repository: directory-scoped create / open / list / delete / fork /
// branch over JSONL session files — the TS `SessionRepository` surface for a
// per-cwd session folder. New and branched sessions defer their file to the
// first assistant message (TS `_persist`), so an empty session never appears
// in `list`.
//
// `list`/`info` are BOUNDED reads: the header line plus the first user
// message, never the whole transcript. The TS upstream parses every entry —
// fine for a CLI's handful of sessions, O(entire store) for a long-lived
// multi-host desktop state. The full parse stays available through `open`
// (the transcript is the authority); only the list's bounded facts deviate.

use std::path::{Path, PathBuf};

use tokio::fs::File;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

use crate::session::Session;
use crate::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};

/// How much of a session file `list` reads before falling back to chunked
/// streaming: the first user message sits inside 64 KiB for ~95% of a real
/// store (measured on 544 files / 412 MiB: median 1.2 KiB, p90 6.4 KiB,
/// p95 66 KiB, p99 600 KiB, max 955 KiB).
const FIRST_USER_PREFIX: usize = 64 * 1024;
/// The streaming chunk after the prefix misses (p99 territory).
const FIRST_USER_CHUNK: usize = 1024 * 1024;
/// The header line is read with room to grow; a valid header is far below
/// this bound, so hitting it is corruption, not slowness.
const HEADER_READ_CAP: usize = 1024 * 1024;
/// Concurrent per-file scans inside one `list` — enough to keep the block
/// layer busy without burying the runtime in spawned reads.
const SCAN_CONCURRENCY: usize = 12;

/// A session summary as `list` reports it — the TS non-UI core `SessionInfo`,
/// shrunk to the facts a list renders. The title's authority is the
/// `.meta.json` sidecar; the transcript-derived display name and the joined
/// `all_messages_text` had no consumers and are gone.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    /// Working directory where the session was started.
    pub cwd: String,
    /// Path of the session this one forked from, when it has one.
    pub parent_session_path: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Last durable write (file mtime — an append-only journal's mtime IS
    /// its last append); falls back to the header timestamp pre-write.
    pub modified_at: chrono::DateTime<chrono::Utc>,
    /// Whether any message entry exists in the transcript.
    pub has_messages: bool,
    /// Text of the first user message, or `"(no messages)"`.
    pub first_message: String,
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
    /// `open` and see the error — but never SILENTLY: each skip warns with
    /// the path and the reason, because a skipped file is a thread
    /// vanishing from the sidebar. Per-file scans run
    /// [`SCAN_CONCURRENCY`] wide; each file costs one header read plus a
    /// bounded first-user-message scan, never a full parse.
    pub async fn list(&self) -> Result<Vec<SessionInfo>, anyhow::Error> {
        let paths = session_files(&self.dir).await?;
        let mut out = Vec::with_capacity(paths.len());
        for chunk in paths.chunks(SCAN_CONCURRENCY) {
            let mut handles = Vec::with_capacity(chunk.len());
            for path in chunk {
                let scanned = path.clone();
                let logged = path.clone();
                handles.push(tokio::spawn(async move {
                    match build_session_info(&scanned).await {
                        Ok(info) => Some(info),
                        Err(error) => {
                            tracing::warn!(
                                path = %logged.display(),
                                %error,
                                "session file skipped by list(); open it directly for the full error"
                            );
                            None
                        }
                    }
                }));
            }
            for handle in handles {
                if let Some(info) = handle.await.unwrap() {
                    out.push(info);
                }
            }
        }
        out.sort_by(|a, b| {
            b.modified_at
                .cmp(&a.modified_at)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| b.id.cmp(&a.id))
        });
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

    /// The session header alone (id, cwd, timestamps, fork parent, free-form
    /// metadata) — one line read, microsecond scale. For host-membership
    /// checks and explicit opens that never need the transcript; the only
    /// correct answer to "which host owns this file" without paying for a
    /// list.
    pub async fn header(&self, path: &Path) -> Result<JsonlSessionMetadata, anyhow::Error> {
        Ok(read_header(path).await?.0)
    }
}

/// The canonical journal file name for a session id (`<id>.jsonl`) — the
/// single source shared by creation, repository scans, and on-disk identity
/// probes (a cold `CreateSession` must find and restore this file, never
/// re-mint over it).
pub fn session_file_name(id: &str) -> String {
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

/// Build the [`SessionInfo`] for one file from BOUNDED reads: the header
/// line, the first user message (a raw-byte needle scan — see
/// [`scan_first_user_message`]), and the file's mtime. Never parses the
/// transcript; the full load stays `open`'s job. A corrupt file errors and
/// is skipped by `list`.
async fn build_session_info(path: &Path) -> Result<SessionInfo, anyhow::Error> {
    let (metadata, _version) = read_header(path).await?;
    let file_metadata = tokio::fs::metadata(path).await?;
    let modified_at = file_metadata
        .modified()
        .ok()
        .map(chrono::DateTime::from)
        .filter(|at: &chrono::DateTime<chrono::Utc>| *at > metadata.created_at)
        .unwrap_or(metadata.created_at);
    let (first_user, has_messages) = scan_first_user_message(path).await?;
    Ok(SessionInfo {
        path: path.to_path_buf(),
        id: metadata.id,
        cwd: metadata.cwd,
        // The summary keeps a String: its consumers treat the parent link
        // as a display/grouping key. Files can only hold valid UTF8 here
        // (the JSON boundary errored loudly at create time on non-UTF8
        // paths), so this conversion is never lossy in practice.
        parent_session_path: metadata
            .parent_session_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        created_at: metadata.created_at,
        modified_at,
        has_messages,
        first_message: first_user.unwrap_or_else(|| "(no messages)".to_string()),
        metadata: metadata.metadata,
    })
}

/// Read and validate the first (header) line of a session file, growing the
/// read until the newline arrives. Returns the parsed metadata plus the
/// on-disk format version.
async fn read_header(path: &Path) -> Result<(JsonlSessionMetadata, u32), anyhow::Error> {
    let mut file = File::open(path).await?;
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = vec![0u8; 4096];
    loop {
        let read = file.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.contains(&b'\n') {
            break;
        }
        anyhow::ensure!(
            buffer.len() < HEADER_READ_CAP,
            "session header line exceeds {} bytes: {}",
            HEADER_READ_CAP,
            path.display()
        );
    }
    let line_end = buffer
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(buffer.len());
    crate::session::jsonl::parse_header_line(&buffer[..line_end])
}

/// The entry-type prefix of a message line on the wire — serde's tagged
/// enums serialize `type` first, so this filter rejects non-message lines
/// without parsing them.
const MESSAGE_LINE_PREFIX: &[u8] = br#"{"type":"message""#;
/// The user-role needle, matched on the raw line before any parse.
const USER_ROLE_NEEDLE: &[u8] = br#""role":"user""#;

/// Find the first user message's text with raw-byte scans: read a
/// [`FIRST_USER_PREFIX`] window (then [`FIRST_USER_CHUNK`] windows) and, per
/// complete line, check the `{"type":"message"` prefix and the
/// `"role":"user"` needle; only the ONE hit line is deserialized. Returns
/// `(first user text, any-message-seen)`; the text is `None` when no user
/// message with non-empty text exists.
async fn scan_first_user_message(path: &Path) -> Result<(Option<String>, bool), anyhow::Error> {
    let mut file = File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(0)).await?;
    let mut window = vec![0u8; FIRST_USER_PREFIX];
    let mut pending: Vec<u8> = Vec::new();
    let mut has_messages = false;
    loop {
        let read = file.read(&mut window).await?;
        if read == 0 {
            break;
        }
        pending.extend_from_slice(&window[..read]);
        // Consume every complete line in the pending bytes, keeping the
        // (at most one) trailing partial line for the next round.
        let mut consumed = 0usize;
        while let Some(rel) = pending[consumed..].iter().position(|&b| b == b'\n') {
            let line = &pending[consumed..consumed + rel];
            if let Some(text) = classify_line(line) {
                if let Some(text) = text {
                    return Ok((Some(text), true));
                }
                has_messages = true;
            }
            consumed += rel + 1;
        }
        if consumed > 0 {
            pending.drain(..consumed);
        }
        if window.len() < FIRST_USER_CHUNK {
            window.resize(FIRST_USER_CHUNK, 0);
        }
    }
    if !pending.is_empty()
        && let Some(text) = classify_line(&pending)
    {
        if let Some(text) = text {
            return Ok((Some(text), true));
        }
        has_messages = true;
    }
    Ok((None, has_messages))
}

/// One raw line's verdict: `None` = not a message entry; `Some(None)` = a
/// message entry whose text is empty or not user-authored (keep scanning);
/// `Some(Some(text))` = the first user message's text.
fn classify_line(line: &[u8]) -> Option<Option<String>> {
    if !line.starts_with(MESSAGE_LINE_PREFIX) {
        return None;
    }
    let is_user = contains_subslice(line, USER_ROLE_NEEDLE);
    if !is_user {
        return Some(None);
    }
    let value: serde_json::Value = match serde_json::from_slice(line) {
        Ok(value) => value,
        // A line that passed the prefix and needle but does not parse is
        // corruption the full `open` will report properly; the scan stays
        // bounded and moves on.
        Err(_) => return Some(None),
    };
    let text = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if text.is_empty() {
        Some(None)
    } else {
        Some(Some(text))
    }
}

/// Slice-contains for byte patterns (`[u8]::windows` would allocate nothing
/// but is slower than a simple scan; std has no `contains` for subslices).
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentMessage, ContentBlock};

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
        assert!(listed[0].has_messages, "user + assistant");
        assert_eq!(listed[0].first_message, "first");

        // The materialized file reopens with the same content.
        let reopened = repo.open(&listed[0].path).await.unwrap();
        assert_eq!(reopened.build_context_entries().await.unwrap().len(), 3);
    }

    /// The bounded scan's facts match the full parse's: `header()` carries
    /// the identity metadata, `has_messages` covers assistant-only
    /// transcripts, and a first user message buried past the 64 KiB prefix
    /// still surfaces (the chunked fallback).
    #[tokio::test]
    async fn test_bounded_scan_facts_across_prefix_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SessionRepository::new(dir.path());

        // An assistant-only transcript: has messages, no first user text.
        let assistant_only = repo.create(meta()).await.unwrap();
        assistant_only
            .append_message(assistant("hi"))
            .await
            .unwrap();

        // A session whose first user message sits BEYOND the 64 KiB prefix:
        // one large custom entry first, then the user prompt.
        let buried = repo
            .create(meta_with(serde_json::json!({ "host": "manox" })))
            .await
            .unwrap();
        let bulk = "x".repeat(FIRST_USER_PREFIX + FIRST_USER_PREFIX / 2);
        buried
            .append_custom("bulk", Some(serde_json::json!({ "pad": bulk })))
            .await
            .unwrap();
        buried
            .append_message(AgentMessage::user("buried prompt"))
            .await
            .unwrap();
        // The file materializes on the first ASSISTANT message (TS
        // `_persist`) — the user prompt alone stays deferred.
        buried.append_message(assistant("reply")).await.unwrap();

        let mut listed = repo.list().await.unwrap();
        listed.sort_by(|a, b| a.cwd.cmp(&b.cwd).then(a.id.cmp(&b.id)));
        for info in &listed {
            let header = repo.header(&info.path).await.unwrap();
            assert_eq!(header.id, info.id);
            assert_eq!(header.cwd, info.cwd);
        }
        let assistant_only = listed
            .iter()
            .find(|i| i.first_message == "(no messages)")
            .expect("assistant-only row present");
        assert!(assistant_only.has_messages);
        let buried_row = listed
            .iter()
            .find(|i| i.first_message == "buried prompt")
            .expect("the past-prefix user message surfaced");
        assert!(buried_row.has_messages);
        assert_eq!(
            buried_row.metadata.as_ref(),
            Some(&serde_json::json!({ "host": "manox" })),
            "header metadata rides the bounded scan"
        );
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
