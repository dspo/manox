// End-to-end tests of the hashline tool plumbing: read/write/edit/grep tools
// driving a real filesystem env and a shared ToolState through the public
// AgentTool::execute path.

use std::path::{Path, PathBuf};

use serde_json::json;
use tokio_util::sync::CancellationToken;

use pi::env::{ExecutionEnv, TokioExecutionEnv};
use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolState};
use pi::tools::edit::EditTool;
use pi::tools::grep::GrepTool;
use pi::tools::read::ReadTool;
use pi::tools::write::WriteTool;
use pi::types::ContentBlock;

struct TestCtx {
    env: TokioExecutionEnv,
    cwd: PathBuf,
    state: ToolState,
}

impl TestCtx {
    fn new(dir: &Path) -> Self {
        TestCtx {
            env: TokioExecutionEnv::new(dir),
            cwd: dir.to_path_buf(),
            state: ToolState::new(),
        }
    }
}

impl ToolContext for TestCtx {
    fn env(&self) -> &dyn ExecutionEnv {
        &self.env
    }
    fn cwd(&self) -> &Path {
        &self.cwd
    }
    fn tool_state(&self) -> &ToolState {
        &self.state
    }
}

fn text_of(result: &AgentToolResult) -> &str {
    let ContentBlock::Text { text, .. } = &result.content[0] else {
        panic!("expected text content");
    };
    text
}

/// The 6-hex tag from a `[path#TAG]` header line.
fn tag_of(output: &str) -> &str {
    let first = output.lines().next().expect("non-empty output");
    let start = first.rfind('#').expect("header has `#`") + 1;
    let end = first.rfind(']').expect("header has `]`");
    &first[start..end]
}

#[tokio::test]
async fn read_edit_reedit_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("main.rs");
    std::fs::write(&file, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
    let ctx = TestCtx::new(dir.path());

    let read_out = ReadTool
        .execute(
            "1",
            json!({"path": "main.rs"}),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .unwrap();
    let read_text = text_of(&read_out);
    assert!(
        read_text.contains("1:fn main() {"),
        "numbered rows: {read_text}"
    );
    let tag = tag_of(read_text).to_string();

    let patch = format!(
        "[{}#{}]\nSWAP 2.=2:\n+    println!(\"hello\");",
        file.display(),
        tag
    );
    let edit_out = EditTool
        .execute("2", json!({"patch": patch}), CancellationToken::new(), &ctx)
        .await
        .unwrap();
    assert!(!edit_out.is_error, "edit failed: {}", text_of(&edit_out));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "fn main() {\n    println!(\"hello\");\n}\n"
    );

    // Chain a second edit on the fresh tag returned by the first edit.
    let new_tag = tag_of(text_of(&edit_out)).to_string();
    assert_ne!(new_tag, tag);
    let patch2 = format!("[{}#{}]\nINS.TAIL:\n+main();", file.display(), new_tag);
    let edit_out2 = EditTool
        .execute(
            "3",
            json!({"patch": patch2}),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        !edit_out2.is_error,
        "re-edit failed: {}",
        text_of(&edit_out2)
    );
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .ends_with("}\nmain();\n")
    );
}

#[tokio::test]
async fn stale_tag_recovers_via_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.rs");
    std::fs::write(&file, "fn a() {\n    x();\n}\n").unwrap();
    let ctx = TestCtx::new(dir.path());

    let read_out = ReadTool
        .execute("1", json!({"path": "a.rs"}), CancellationToken::new(), &ctx)
        .await
        .unwrap();
    let tag = tag_of(text_of(&read_out)).to_string();

    // External edit between read and edit shifts the target line down.
    std::fs::write(&file, "// header\nfn a() {\n    x();\n}\n").unwrap();

    let patch = format!("[{}#{}]\nSWAP 2.=2:\n+    y();", file.display(), tag);
    let edit_out = EditTool
        .execute("2", json!({"patch": patch}), CancellationToken::new(), &ctx)
        .await
        .unwrap();
    assert!(
        !edit_out.is_error,
        "stale tag should recover: {}",
        text_of(&edit_out)
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "// header\nfn a() {\n    y();\n}\n"
    );
}

#[tokio::test]
async fn write_strips_pasted_read_output() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pasted.rs");
    let ctx = TestCtx::new(dir.path());

    let pasted = "[src/pasted.rs#ABCD]\n1:fn main() {\n2:    println!(\"hi\");\n3:}";
    let out = WriteTool
        .execute(
            "1",
            json!({"path": "pasted.rs", "content": pasted}),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!out.is_error, "write failed: {}", text_of(&out));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "fn main() {\n    println!(\"hi\");\n}"
    );
    // The write records a snapshot whose tag a follow-up edit can claim.
    let head = ctx
        .state
        .snapshots
        .lock()
        .unwrap()
        .head(&file)
        .expect("snapshot recorded")
        .tag
        .clone();
    let patch = format!(
        "[{}#{}]\nSWAP 2.=2:\n+    println!(\"yo\");",
        file.display(),
        head
    );
    let edit_out = EditTool
        .execute("2", json!({"patch": patch}), CancellationToken::new(), &ctx)
        .await
        .unwrap();
    assert!(
        !edit_out.is_error,
        "edit after write failed: {}",
        text_of(&edit_out)
    );
}

#[tokio::test]
async fn grep_snapshots_matched_files_for_direct_edit() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("g.rs");
    std::fs::write(&file, "fn target() {\n    needle();\n}\n").unwrap();
    let ctx = TestCtx::new(dir.path());

    let out = GrepTool
        .execute(
            "1",
            json!({"pattern": "needle", "path": dir.path().to_str().unwrap()}),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!out.is_error, "grep failed: {}", text_of(&out));

    // No read happened, yet the grep-matched file has a snapshot whose tag
    // an edit can claim directly.
    let tag = ctx
        .state
        .snapshots
        .lock()
        .unwrap()
        .head(&file)
        .expect("grep recorded a snapshot")
        .tag
        .clone();
    let patch = format!(
        "[{}#{}]\nSWAP 2.=2:\n+    replacement();",
        file.display(),
        tag
    );
    let edit_out = EditTool
        .execute("2", json!({"patch": patch}), CancellationToken::new(), &ctx)
        .await
        .unwrap();
    assert!(
        !edit_out.is_error,
        "edit on grep snapshot failed: {}",
        text_of(&edit_out)
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "fn target() {\n    replacement();\n}\n"
    );
}

#[tokio::test]
async fn edit_restores_crlf_and_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("win.rs");
    std::fs::write(&file, "fn a() {\r\n    x();\r\n}\r\n").unwrap();
    let ctx = TestCtx::new(dir.path());

    let read_out = ReadTool
        .execute(
            "1",
            json!({"path": "win.rs"}),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .unwrap();
    let tag = tag_of(text_of(&read_out)).to_string();

    let patch = format!("[{}#{}]\nSWAP 2.=2:\n+    y();", file.display(), tag);
    let edit_out = EditTool
        .execute("2", json!({"patch": patch}), CancellationToken::new(), &ctx)
        .await
        .unwrap();
    assert!(!edit_out.is_error, "edit failed: {}", text_of(&edit_out));
    // CRLF endings and the trailing newline survive the edit.
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "fn a() {\r\n    y();\r\n}\r\n"
    );
}

#[tokio::test]
async fn partial_read_gates_unseen_lines_then_reveal_retry_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("p.rs");
    let content: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    std::fs::write(&file, content).unwrap();
    let ctx = TestCtx::new(dir.path());

    // Read lines 1..=3; the numbered body displays those plus trailing
    // context. Line 20 is never shown.
    let read_out = ReadTool
        .execute(
            "1",
            json!({"path": "p.rs", "offset": 1, "limit": 3}),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .unwrap();
    let tag = tag_of(text_of(&read_out)).to_string();

    // Edit line 20 — never displayed — the gate rejects with a reveal.
    let patch = format!("[{}#{}]\nSWAP 20.=20:\n+edited", file.display(), tag);
    let err = EditTool
        .execute("2", json!({"patch": patch}), CancellationToken::new(), &ctx)
        .await
        .expect_err("unseen line must be gated")
        .to_string();
    assert!(err.contains("never displayed"), "{err}");
    assert!(err.contains("20:line 20"), "reveal inlines content: {err}");
    // File untouched by the rejected edit.
    assert!(!std::fs::read_to_string(&file).unwrap().contains("edited"));
    // The full-width reveal merged line 20 into seen_lines: the same patch
    // retries straight through without a re-read.
    let retried = EditTool
        .execute("3", json!({"patch": patch}), CancellationToken::new(), &ctx)
        .await
        .unwrap();
    assert!(
        !retried.is_error,
        "straight retry after reveal must succeed: {}",
        text_of(&retried)
    );
    assert!(std::fs::read_to_string(&file).unwrap().contains("edited"));
}

#[tokio::test]
async fn displayed_context_lines_pass_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("c.rs");
    let content: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    std::fs::write(&file, content).unwrap();
    let ctx = TestCtx::new(dir.path());

    // Read lines 1..=3; format_numbered_range appends 3 trailing context
    // lines, so line 5 (context) was displayed even though it was outside
    // the requested range.
    let read_out = ReadTool
        .execute(
            "1",
            json!({"path": "c.rs", "offset": 1, "limit": 3}),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .unwrap();
    let tag = tag_of(text_of(&read_out)).to_string();

    let patch = format!("[{}#{}]\nSWAP 5.=5:\n+edited 5", file.display(), tag);
    let out = EditTool
        .execute("2", json!({"patch": patch}), CancellationToken::new(), &ctx)
        .await
        .unwrap();
    assert!(
        !out.is_error,
        "displayed context line must pass the gate: {}",
        text_of(&out)
    );
}

#[tokio::test]
async fn grep_provenance_gates_non_matched_lines() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("s.rs");
    let content: String = (1..=10).map(|i| format!("row {i}\n")).collect();
    std::fs::write(&file, content).unwrap();
    let ctx = TestCtx::new(dir.path());

    // Grep displays only the matched line (no context): line 5.
    let out = GrepTool
        .execute(
            "1",
            json!({"pattern": "row 5", "path": dir.path().to_str().unwrap()}),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!out.is_error, "grep failed: {}", text_of(&out));

    let tag = ctx
        .state
        .snapshots
        .lock()
        .unwrap()
        .head(&file)
        .expect("grep recorded a snapshot")
        .tag
        .clone();

    // Editing a never-displayed line is gated; the reveal merges the line
    // into seen_lines so the same patch retries clean (no write on reject).
    let gate_patch = format!("[{}#{}]\nSWAP 9.=9:\n+other", file.display(), tag);
    let gated = EditTool
        .execute(
            "3",
            json!({"patch": gate_patch}),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .expect_err("non-matched line must be gated")
        .to_string();
    assert!(gated.contains("never displayed"), "{gated}");
    assert!(
        !std::fs::read_to_string(&file).unwrap().contains("other"),
        "rejected edit must not touch the file"
    );
    let retried = EditTool
        .execute(
            "4",
            json!({"patch": gate_patch}),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        !retried.is_error,
        "straight retry after reveal must succeed: {}",
        text_of(&retried)
    );

    // Fresh context: grep again and edit the matched line — passes the gate.
    let ctx2 = TestCtx::new(dir.path());
    let out2 = GrepTool
        .execute(
            "5",
            json!({"pattern": "row 5", "path": dir.path().to_str().unwrap()}),
            CancellationToken::new(),
            &ctx2,
        )
        .await
        .unwrap();
    assert!(!out2.is_error, "grep failed: {}", text_of(&out2));
    let tag2 = ctx2
        .state
        .snapshots
        .lock()
        .unwrap()
        .head(&file)
        .expect("grep recorded a snapshot")
        .tag
        .clone();
    let ok_patch = format!("[{}#{}]\nSWAP 5.=5:\n+hit", file.display(), tag2);
    let ok = EditTool
        .execute(
            "6",
            json!({"patch": ok_patch}),
            CancellationToken::new(),
            &ctx2,
        )
        .await
        .unwrap();
    assert!(!ok.is_error, "matched line must pass: {}", text_of(&ok));
}

#[tokio::test]
async fn edit_chain_after_full_seen_recording_passes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("w.rs");
    let ctx = TestCtx::new(dir.path());

    // Write mints a snapshot with every line seen.
    WriteTool
        .execute(
            "1",
            json!({"path": "w.rs", "content": "a\nb\nc\n"}),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .unwrap();
    let tag = ctx
        .state
        .snapshots
        .lock()
        .unwrap()
        .head(&file)
        .expect("write recorded a snapshot")
        .tag
        .clone();

    // A follow-up edit on any line passes the gate.
    let patch = format!("[{}#{}]\nSWAP 2.=2:\n+B", file.display(), tag);
    let out = EditTool
        .execute("2", json!({"patch": patch}), CancellationToken::new(), &ctx)
        .await
        .unwrap();
    assert!(
        !out.is_error,
        "edit after write-authored seen must pass: {}",
        text_of(&out)
    );
}
