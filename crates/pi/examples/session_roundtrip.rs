// Round-trip a v3 JSONL session: append messages, rebuild context, switch
// the leaf to branch, and reopen from disk. Covers #372 (storage format) and
// #373 (entry types / leaf-as-entry).
//
// Usage:
//   cargo run -p pi --example session_roundtrip

use pi::AgentMessage;
use pi::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};
use pi::session::{Session, SessionStorage, SessionTreeEntry};

#[tokio::main]
async fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let meta = JsonlSessionMetadata {
        id: "demo".into(),
        cwd: dir.path().to_string_lossy().into_owned(),
        created_at: chrono::Utc::now(),
        parent_session_path: None,
        metadata: None,
    };

    let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta)
        .await
        .expect("open");
    let session = Session::new(storage);

    // Append a linear thread: user → assistant → user → assistant.
    let m1 = session
        .append_message(AgentMessage::user("first question"))
        .await
        .expect("append");
    let m2 = session
        .append_message(AgentMessage::user("first answer"))
        .await
        .expect("append");
    session
        .append_message(AgentMessage::user("second question"))
        .await
        .expect("append");
    let m4 = session
        .append_message(AgentMessage::user("second answer"))
        .await
        .expect("append");

    print_path("linear", &session.build_context().await.expect("context"));

    // Switch the leaf back to the first answer and grow a branch from it.
    session
        .storage()
        .set_leaf_id(Some(&m2))
        .await
        .expect("set leaf");
    session
        .append_message(AgentMessage::user("branched follow-up"))
        .await
        .expect("append");
    print_path(
        "branched from m2",
        &session.build_context().await.expect("context"),
    );

    // Reopen from disk: the leaf and all entries survived.
    let storage = JsonlSessionStorage::open(&dir.path().join("session.jsonl"))
        .await
        .expect("reopen");
    let entries = storage.get_entries().await.expect("entries");
    println!(
        "reopened: {} entries, leaf={:?}",
        entries.len(),
        storage.get_leaf_id().await
    );

    println!("ids: m1={m1} m2={m2} m4={m4}");
}

fn print_path(label: &str, path: &[SessionTreeEntry]) {
    let names: Vec<String> = path
        .iter()
        .map(|e| match e {
            SessionTreeEntry::Message { message, .. } => match message {
                AgentMessage::User { content, .. } => {
                    format!("user:{}", first_text(content))
                }
                _ => "assistant".into(),
            },
            SessionTreeEntry::Compaction { .. } => "compaction".into(),
            _ => "other".into(),
        })
        .collect();
    println!("{label}: [{}]", names.join(", "));
}

fn first_text(content: &[pi::types::ContentBlock]) -> &str {
    content
        .iter()
        .find_map(|b| match b {
            pi::types::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}
