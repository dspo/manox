// The Py tool's behavioral contract. These live in their own test binary on
// purpose: the tool moves the PROCESS working directory for the duration of a
// script (the embedded interpreter has no per-instance cwd), so running them
// beside the persistent-shell tests would leak a cwd into unrelated tests.
// A separate process per test binary keeps that effect contained.

use std::sync::Arc;

use manox_harness::env::TokioExecutionEnv;
use manox_harness::tool::{
    AgentTool, AgentToolResult, ExecutionMode, LocalToolContext, ToolError, ToolState,
};
use manox_harness::tools::py::PyTool;
use manox_harness::types::ContentBlock;
use tokio_util::sync::CancellationToken;

fn ctx() -> (LocalToolContext, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let context = LocalToolContext::new(
        Arc::new(TokioExecutionEnv::new(dir.path())),
        dir.path().to_path_buf(),
        Arc::new(ToolState::new()),
    );
    (context, dir)
}

fn text(result: &AgentToolResult) -> String {
    match &result.content[0] {
        ContentBlock::Text { text, .. } => text.clone(),
        _ => panic!("expected a text block"),
    }
}

async fn run(script: &str) -> AgentToolResult {
    let (context, _dir) = ctx();
    PyTool
        .execute(
            "c1",
            serde_json::json!({ "script": script, "cwd": [""] }),
            CancellationToken::new(),
            &context,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn a_script_prints_into_the_result() {
    let result = run("print('hello from rustpython')\nprint(sum(range(10)))").await;
    assert!(!result.is_error);
    assert_eq!(text(&result), "hello from rustpython\n45");
}

/// The script runs as `__main__` with the documented argv, and the call's cwd
/// is the script's cwd.
#[tokio::test]
async fn the_script_runs_as_main_inside_the_call_directory() {
    let result = run("import os, sys, json\n\
         print(__name__, sys.argv)\n\
         print(os.getcwd())\n\
         print(json.dumps({'k': [1, 2]}))")
    .await;
    assert!(!result.is_error, "out: {}", text(&result));
    let out = text(&result);
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some("__main__ ['<py>']"));
    assert!(
        lines.next().unwrap_or_default().starts_with('/'),
        "absolute cwd: {out}"
    );
    assert_eq!(lines.next(), Some("{\"k\": [1, 2]}"));
}

/// An uncaught exception is the error face: the traceback names the failing
/// line of the SCRIPT (not a wrapper), and output printed before the raise is
/// preserved.
#[tokio::test]
async fn an_exception_fails_with_a_traceback_and_partial_output() {
    let result = run("print('before')\nraise ValueError('boom')").await;
    let out = text(&result);
    assert!(result.is_error, "the exception is an error result: {out}");
    assert!(out.starts_with("before\n"), "partial output first: {out}");
    assert!(
        out.contains("ValueError: boom"),
        "the traceback is in: {out}"
    );
    assert!(
        out.contains("<py>"),
        "the failing frame names the script: {out}"
    );
}

#[tokio::test]
async fn missing_script_is_an_argument_error() {
    let (context, _dir) = ctx();
    let err = PyTool
        .execute(
            "c1",
            serde_json::json!({"cwd": [""]}),
            CancellationToken::new(),
            &context,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArguments(_)));
}

/// `sys.exit(n)` is a status: no traceback, the code is the error flag, and
/// the partial output survives.
#[tokio::test]
async fn sys_exit_is_a_status_not_a_traceback() {
    let zero = run("print('done')\nimport sys\nsys.exit(0)").await;
    assert!(!zero.is_error);
    assert_eq!(text(&zero), "done");

    let three = run("print('partial')\nimport sys\nsys.exit(3)").await;
    assert!(three.is_error);
    assert_eq!(text(&three), "partial\n[exit 3]");
    assert!(!text(&three).contains("Traceback"));
}

/// The standard library is the contract, and `sqlite3` rides it.
#[tokio::test]
async fn the_standard_library_and_sqlite_are_available() {
    let result = run("import sqlite3\n\
         con = sqlite3.connect(':memory:')\n\
         print(con.execute('select 41 + 1').fetchone()[0])\n\
         import hashlib, json, re, csv, difflib\n\
         print(hashlib.md5(b'x').hexdigest()[:8], len(json.dumps([1])))")
    .await;
    let out = text(&result);
    assert!(!result.is_error, "out: {out}");
    assert!(out.starts_with("42\n"), "sqlite runs first: {out}");
}

/// The 120-line contract the description states is enforced, so the model
/// never learns the limit by losing a turn.
#[tokio::test]
async fn scripts_over_the_line_limit_are_rejected() {
    let (context, _dir) = ctx();
    let script = "x = 1\n".repeat(121);
    let err = PyTool
        .execute(
            "c1",
            serde_json::json!({ "script": script, "cwd": [""] }),
            CancellationToken::new(),
            &context,
        )
        .await
        .unwrap_err();
    match err {
        ToolError::InvalidArguments(message) => {
            assert!(message.contains("120"), "the limit is named: {message}");
            assert!(message.contains("121"), "the size is named: {message}");
        }
        other => panic!("expected an argument error, got {other:?}"),
    }
}

/// A batch rewrite is the shape Py exists for: many files, one call, and the
/// assertions are the script's own.
#[tokio::test]
async fn a_batch_rewrite_reads_and_writes_many_files_in_one_call() {
    let (context, dir) = ctx();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.path().join(name), "old-name\nkeep\n").unwrap();
    }
    let result = PyTool
        .execute(
            "c1",
            serde_json::json!({
                "script": "from pathlib import Path\nchanged = 0\n\
                           for path in sorted(Path('.').glob('*.txt')):\n    \
                           text = path.read_text()\n    \
                           assert text.count('old-name') == 1, path\n    \
                           path.write_text(text.replace('old-name', 'new-name'))\n    \
                           changed += 1\n\
                           print(f'{changed} files')",
                "cwd": [""]
            }),
            CancellationToken::new(),
            &context,
        )
        .await
        .unwrap();
    assert!(!result.is_error, "out: {}", text(&result));
    assert_eq!(text(&result), "3 files");
    for name in ["a.txt", "b.txt", "c.txt"] {
        let body = std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(body, "new-name\nkeep\n");
    }
}

/// A script that outlives its budget is abandoned at the wall clock; the tool
/// says so instead of hanging. The interpreter cannot be preempted, so the
/// script finishes in the background and releases the lock — this pins the
/// message, not a kill.
#[tokio::test]
async fn a_script_past_its_budget_is_abandoned() {
    let (context, _dir) = ctx();
    let err = PyTool
        .execute(
            "c1",
            serde_json::json!({
                "script": "import time\ntime.sleep(2)",
                "cwd": [""],
                "timeout": 100
            }),
            CancellationToken::new(),
            &context,
        )
        .await
        .unwrap_err();
    match err {
        ToolError::ExecutionFailed(message) => {
            assert!(message.contains("budget"), "{message}");
            assert!(message.contains("still be running"), "{message}");
        }
        other => panic!("expected a timeout failure, got {other:?}"),
    }
}

/// The tool is a plain executor: gated by the host mode, not by itself, and
/// never run in parallel with other calls (it moves the process cwd).
#[test]
fn the_tool_is_sequential_and_declares_no_approval() {
    assert_eq!(PyTool.name(), "Py");
    assert!(!PyTool.is_read_only());
    assert!(!PyTool.requires_approval(&serde_json::json!({"script": "1"})));
    assert_eq!(PyTool.execution_mode(), ExecutionMode::Sequential);
    let schema = PyTool.parameters_schema();
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(required, vec!["script", "cwd"]);
}
