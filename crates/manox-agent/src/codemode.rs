//! Codemode store replay (D8): the `store`/`load` map's resume authority is
//! the transcript itself.
//!
//! One Codemode execution journals a tool result whose `details.storeWrites`
//! carries the writes a successful script made (`set` overrides, `delete`
//! removes; failed scripts never produce writes). Folding those rows in chain
//! order rebuilds the snapshot the next execution starts from — the same
//! replay shape the UpdatePlan snapshot uses (`replay_thread_state`), with
//! the same compression exposure: compaction drops the tool rows, so the
//! store evaporates with them exactly like the plan snapshot does. No
//! sidecar, no extra journal row type, no compat read.
//!
//! Branch semantics follow for free: each branch's active chain folds only
//! its own writes.

use std::collections::BTreeMap;
use std::path::Path;

use manox_harness::session::Event;
use manox_harness::session::jsonl::JournalRecord;
use manox_harness::types::AgentMessage;

/// Fold every Codemode tool result row's `details.storeWrites`, in chain
/// order, into the initial snapshot (key -> serialized JSON). Rows the tool
/// wrote while failing carry no writes and change nothing. Any other row is
/// ignored; a corrupt `storeWrites` payload degrades to a warning (the
/// transcript is data, not code — one bad row must not sink the session).
pub fn codemode_store_from_history(records: &[JournalRecord]) -> BTreeMap<String, String> {
    let mut store = BTreeMap::new();
    for record in records {
        let Event::Message { message, .. } = &record.entry else {
            continue;
        };
        let AgentMessage::ToolResult {
            tool_name,
            details: Some(details),
            is_error: false,
            ..
        } = message
        else {
            continue;
        };
        if tool_name != crate::tools::CODEMODE {
            continue;
        }
        let Some(writes) = details.get("storeWrites") else {
            continue;
        };
        match serde_json::from_value::<manox_codemode::StoreWrites>(writes.clone()) {
            Ok(writes) => {
                for (key, json) in writes.set {
                    store.insert(key, json);
                }
                for key in writes.delete {
                    store.remove(&key);
                }
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "corrupt codemode storeWrites in the journal; the row is skipped"
                );
            }
        }
    }
    store
}

/// Fold the session journal at `path` into the initial codemode store, for
/// the assembly to hand `CodemodeTool::new`. `None` (a fresh session whose
/// journal never materialized) or an unreadable file folds to an empty map —
/// a fresh session has no history to replay, and an unreadable one must not
/// block the assembly.
pub async fn codemode_store_from_journal(journal: Option<&Path>) -> BTreeMap<String, String> {
    let Some(path) = journal else {
        return BTreeMap::new();
    };
    if !path.exists() {
        return BTreeMap::new();
    }
    let storage = match manox_harness::session::jsonl::JsonlSessionStorage::open(path).await {
        Ok(storage) => storage,
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "could not open the session journal for the codemode store replay; starting empty"
            );
            return BTreeMap::new();
        }
    };
    let session = manox_harness::session::Session::new(storage);
    match session.storage().journal_range(0, u64::MAX).await {
        Ok(records) => codemode_store_from_history(&records),
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "could not read the active chain for the codemode store replay; starting empty"
            );
            BTreeMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::CODEMODE;
    use chrono::Utc;
    use manox_harness::session::Event as E;
    use manox_harness::session::jsonl::JournalRecord;

    fn record(entry: Event) -> JournalRecord {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        JournalRecord {
            seq: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            entry,
        }
    }

    fn codemode_result_row(call_id: &str, is_error: bool, writes: serde_json::Value) -> E {
        let message = AgentMessage::ToolResult {
            tool_call_id: call_id.to_string(),
            tool_name: CODEMODE.to_string(),
            content: Vec::new(),
            is_error,
            details: Some(serde_json::json!({
                "calls": [],
                "storeWrites": writes,
                "options": null,
                "wallTimeMs": 5,
                "droppedTools": [],
            })),
            usage: None,
            added_tool_names: None,
            timestamp: Utc::now(),
        };
        E::Message {
            id: uuid::Uuid::new_v4().to_string(),
            parent_id: None,
            timestamp: Utc::now(),
            message,
            origin: None,
            ui: None,
        }
    }

    fn other_tool_row(call_id: &str) -> E {
        let message = AgentMessage::ToolResult {
            tool_call_id: call_id.to_string(),
            tool_name: crate::tools::WRITE.to_string(),
            content: Vec::new(),
            is_error: false,
            details: Some(serde_json::json!({"storeWrites": {"set": [["hijacked", "1"]]}})),
            usage: None,
            added_tool_names: None,
            timestamp: Utc::now(),
        };
        E::Message {
            id: uuid::Uuid::new_v4().to_string(),
            parent_id: None,
            timestamp: Utc::now(),
            message,
            origin: None,
            ui: None,
        }
    }

    #[test]
    fn set_delete_and_failure_rows_fold_in_chain_order() {
        let records = vec![
            record(codemode_result_row(
                "c1",
                false,
                serde_json::json!({"set": [["a", "1"], ["b", "2"]], "delete": []}),
            )),
            // A non-Codemode row with a storeWrites-shaped detail is ignored.
            record(other_tool_row("w1")),
            // A failed Codemode row carries no writes and changes nothing.
            record(codemode_result_row("c2", true, serde_json::json!(null))),
            // A later successful row overwrites and deletes.
            record(codemode_result_row(
                "c3",
                false,
                serde_json::json!({"set": [["a", "9"]], "delete": ["b"]}),
            )),
            // A non-codemode message row is ignored outright.
            record(E::Title {
                id: uuid::Uuid::new_v4().to_string(),
                parent_id: None,
                timestamp: Utc::now(),
                title: "t".into(),
            }),
        ];
        let store = codemode_store_from_history(&records);
        assert_eq!(store.get("a").map(String::as_str), Some("9"));
        assert!(!store.contains_key("b"));
        assert!(!store.contains_key("hijacked"));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn empty_history_and_untouched_sessions_fold_to_empty() {
        assert!(codemode_store_from_history(&[]).is_empty());
        let records = vec![record(codemode_result_row(
            "c1",
            false,
            serde_json::json!(null),
        ))];
        assert!(codemode_store_from_history(&records).is_empty());
    }

    #[test]
    fn a_corrupt_writes_row_is_skipped_without_sinking_the_fold() {
        let records = vec![
            record(codemode_result_row(
                "c1",
                false,
                serde_json::json!({"set": [["good", "1"]], "delete": []}),
            )),
            record(codemode_result_row(
                "c2",
                false,
                serde_json::json!("garbage"),
            )),
        ];
        let store = codemode_store_from_history(&records);
        assert_eq!(store.get("good").map(String::as_str), Some("1"));
    }

    #[tokio::test]
    async fn journal_reader_folds_a_real_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.v1.jsonl");
        fn codemode_message(
            call_id: &str,
            is_error: bool,
            writes: serde_json::Value,
        ) -> AgentMessage {
            AgentMessage::ToolResult {
                tool_call_id: call_id.to_string(),
                tool_name: CODEMODE.to_string(),
                content: Vec::new(),
                is_error,
                details: Some(serde_json::json!({
                    "calls": [],
                    "storeWrites": writes,
                    "options": null,
                    "wallTimeMs": 5,
                    "droppedTools": [],
                })),
                usage: None,
                added_tool_names: None,
                timestamp: Utc::now(),
            }
        }
        {
            let meta = manox_harness::session::jsonl::JsonlSessionMetadata {
                id: "codemode-replay".into(),
                cwd: dir.path().to_string_lossy().into_owned(),
                created_at: Utc::now(),
                parent_session_path: None,
                metadata: None,
            };
            let storage = manox_harness::session::jsonl::JsonlSessionStorage::create(&path, meta)
                .await
                .unwrap();
            let session = manox_harness::session::Session::new(storage);
            session
                .append_message(codemode_message(
                    "c1",
                    false,
                    serde_json::json!({"set": [["k", "{\"v\":1}"]], "delete": []}),
                ))
                .await
                .unwrap();
            session
                .append_message(codemode_message(
                    "c2",
                    false,
                    serde_json::json!({"set": [], "delete": ["stale"]}),
                ))
                .await
                .unwrap();
            // Appends write through; dropping the session releases the file.
            drop(session);
        }
        let store = codemode_store_from_journal(Some(&path)).await;
        assert_eq!(store.get("k").map(String::as_str), Some("{\"v\":1}"));
        assert!(!store.contains_key("stale"));

        // No journal / missing file fold to empty without touching the disk.
        assert!(codemode_store_from_journal(None).await.is_empty());
        assert!(
            codemode_store_from_journal(Some(&dir.path().join("absent.jsonl")))
                .await
                .is_empty()
        );
    }
}
