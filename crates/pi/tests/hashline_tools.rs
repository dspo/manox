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
