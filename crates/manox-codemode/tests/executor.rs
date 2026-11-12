//! End-to-end tests: executor + bridge + prelude against a live QuickJS VM.
//!
//! These exercise the whole stack the way the harness will: real VM thread,
//! real promise bridge, real cancellation paths. Script sources use raw JS on
//! purpose — the sandbox contract is the thing under test.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use manox_codemode::declarations::DEFAULT_DECLARATION_BUDGET_TOKENS;
use manox_codemode::store::StoreSnapshot;
use manox_codemode::types::{CallStatus, OutputItem};
use manox_codemode::{
    CodemodeErrorKind, CodemodeExecutor, CodemodeResult, CodemodeTool, to_js_identifier,
};

type ToolFuture = Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>;

/// A tool with a canned response and an optional pre-response delay.
struct TestTool {
    name: String,
    description: String,
    schema: Value,
    delay_ms: u64,
    response: Response,
}

enum Response {
    /// Echo `{"callId", "args"}` back.
    Echo,
    Value(Value),
    Error(String),
}

impl TestTool {
    fn echo(name: &str) -> Arc<dyn CodemodeTool> {
        Arc::new(Self {
            name: name.to_owned(),
            description: format!("Echoes its arguments back ({name})."),
            schema: json!({"type": "object"}),
            delay_ms: 0,
            response: Response::Echo,
        })
    }

    fn value(name: &str, value: Value) -> Arc<dyn CodemodeTool> {
        Arc::new(Self {
            name: name.to_owned(),
            description: format!("Returns a fixed value ({name})."),
            schema: json!({"type": "object"}),
            delay_ms: 0,
            response: Response::Value(value),
        })
    }

    fn error(name: &str, message: &str) -> Arc<dyn CodemodeTool> {
        Arc::new(Self {
            name: name.to_owned(),
            description: format!("Always fails ({name})."),
            schema: json!({"type": "object"}),
            delay_ms: 0,
            response: Response::Error(message.to_owned()),
        })
    }

    fn slow(name: &str, delay_ms: u64) -> Arc<dyn CodemodeTool> {
        Arc::new(Self {
            name: name.to_owned(),
            description: format!("Sleeps then echoes ({name})."),
            schema: json!({"type": "object"}),
            delay_ms,
            response: Response::Echo,
        })
    }
}

#[async_trait::async_trait]
impl CodemodeTool for TestTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(
        &self,
        call_id: &str,
        args: Value,
        _signal: CancellationToken,
    ) -> Result<Value, String> {
        if self.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        }
        match &self.response {
            Response::Echo => Ok(json!({"callId": call_id, "args": args})),
            Response::Value(value) => Ok(value.clone()),
            Response::Error(message) => Err(message.clone()),
        }
    }
}

/// A tool backed by a closure, for custom behaviors (barriers, cancellation
/// observation, counters).
struct FnTool<F> {
    name: String,
    f: F,
}

#[async_trait::async_trait]
impl<F> CodemodeTool for FnTool<F>
where
    F: Fn(String, Value, CancellationToken) -> ToolFuture + Send + Sync,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "Closure-driven test tool."
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn execute(
        &self,
        call_id: &str,
        args: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        (self.f)(call_id.to_owned(), args, signal).await
    }
}

fn executor_with(tools: Vec<Arc<dyn CodemodeTool>>) -> CodemodeExecutor {
    CodemodeExecutor::new(tools).expect("valid tool set")
}

fn echo_executor() -> CodemodeExecutor {
    executor_with(vec![
        TestTool::echo("echo"),
        TestTool::value("answer", json!({"n": 42})),
        TestTool::error("broken", "boom"),
        TestTool::slow("slow", 50),
        TestTool::value("weird-name", json!({"ok": true})),
    ])
}

async fn run(executor: &CodemodeExecutor, source: &str) -> CodemodeResult {
    executor
        .execute(source, &StoreSnapshot::new(), CancellationToken::new())
        .await
}

fn text_items(result: &CodemodeResult) -> Vec<&str> {
    result
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::Text {
                text,
                console: false,
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn joined_text(result: &CodemodeResult) -> String {
    text_items(result).join("\n")
}

#[tokio::test]
async fn sandbox_isolation_has_no_authority_globals() {
    let executor = echo_executor();
    let source = r#"
        const names = ["process", "fetch", "require", "setTimeout", "setInterval", "XMLHttpRequest",
            "__hostCall", "__hostOutput", "__hostDone", "__settle"];
        text(names.map((name) => name + "=" + typeof globalThis[name]).join(" "));
        try {
            await import("node:fs");
            text("import: resolved");
        } catch (error) {
            text("import: blocked");
        }
        text("eval: " + eval("1 + 1"));
        text("evalhost: " + eval("typeof __hostCall"));
        return "isolated";
    "#;
    let result = run(&executor, source).await;
    assert!(
        result.error.is_none(),
        "unexpected error: {:?}",
        result.error
    );
    let text = joined_text(&result);
    for name in [
        "process",
        "fetch",
        "require",
        "setTimeout",
        "__hostCall",
        "__settle",
    ] {
        assert!(
            text.contains(&format!("{name}=undefined")),
            "global {name} should be absent: {text}"
        );
    }
    assert!(
        text.contains("import: blocked"),
        "dynamic import must not resolve: {text}"
    );
    assert!(
        text.contains("eval: 2"),
        "eval exists inside the VM: {text}"
    );
    assert!(
        text.contains("evalhost: undefined"),
        "bridge natives must not leak: {text}"
    );
    assert_eq!(result.value.as_deref(), Some("\"isolated\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn promise_all_runs_calls_concurrently_and_results_pair_by_id() {
    let entered = Arc::new(AtomicUsize::new(0));
    let max_concurrent = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let entered_for_tool = Arc::clone(&entered);
    let max_for_tool = Arc::clone(&max_concurrent);
    let barrier_for_tool = Arc::clone(&barrier);
    let barrier_tool = Arc::new(FnTool {
        name: "meet".to_owned(),
        f: move |_call_id: String, args: Value, _signal: CancellationToken| {
            let entered = Arc::clone(&entered_for_tool);
            let max = Arc::clone(&max_for_tool);
            let barrier = Arc::clone(&barrier_for_tool);
            Box::pin(async move {
                let now = entered.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(now, Ordering::SeqCst);
                // Every call waits for all eight: the test fails by timeout if
                // the bridge serialized them.
                barrier.wait().await;
                Ok(json!({"saw": now, "arg": args}))
            }) as ToolFuture
        },
    });
    let executor = executor_with(vec![barrier_tool]);
    let source = r#"
        const calls = [];
        for (let i = 0; i < 8; i++) calls.push(tools.meet({ index: i }));
        const results = await Promise.all(calls);
        return results.map((result) => result.arg.index);
    "#;
    let result = run(&executor, source).await;
    assert!(
        result.error.is_none(),
        "unexpected error: {:?}",
        result.error
    );
    assert_eq!(
        entered.load(Ordering::SeqCst),
        8,
        "all eight calls entered their tool"
    );
    assert_eq!(
        max_concurrent.load(Ordering::SeqCst),
        8,
        "all eight calls were in flight at once (Promise.all must not serialize)"
    );
    // The return value carries every result, paired by call id.
    let returned: Vec<u64> =
        serde_json::from_str(result.value.as_deref().expect("return value")).expect("json array");
    let mut sorted = returned.clone();
    sorted.sort();
    assert_eq!(sorted, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(result.calls.len(), 8);
    assert!(
        result
            .calls
            .iter()
            .all(|call| call.status == CallStatus::Ok)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn out_of_order_completions_pair_back_correctly() {
    // Completions land in reverse order; each promise must still resolve with
    // its own argument's value.
    let tool = Arc::new(FnTool {
        name: "delayed".to_owned(),
        f: move |_call_id: String, args: Value, _signal: CancellationToken| {
            Box::pin(async move {
                let wait = 80 - args["index"].as_u64().unwrap_or(0) * 10;
                tokio::time::sleep(Duration::from_millis(wait)).await;
                Ok(json!({"index": args["index"]}))
            }) as ToolFuture
        },
    });
    let executor = executor_with(vec![tool]);
    let source = r#"
        const calls = [0, 1, 2, 3, 4, 5].map((i) => tools.delayed({ index: i }));
        const results = await Promise.all(calls);
        return results.map((result) => result.index);
    "#;
    let result = run(&executor, source).await;
    assert!(
        result.error.is_none(),
        "unexpected error: {:?}",
        result.error
    );
    let returned: Vec<u64> =
        serde_json::from_str(result.value.as_deref().expect("return value")).expect("json array");
    assert_eq!(
        returned,
        vec![0, 1, 2, 3, 4, 5],
        "results resolved with their own values despite reversed completion order"
    );
}

#[tokio::test]
async fn cancellation_aborts_a_pure_js_loop() {
    let executor = echo_executor();
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        canceller.cancel();
    });
    let result = executor
        .execute("while (true) {}", &StoreSnapshot::new(), cancel)
        .await;
    assert_eq!(
        result.error.as_ref().map(|error| error.kind),
        Some(CodemodeErrorKind::Aborted)
    );
}

#[tokio::test]
async fn cancellation_aborts_a_script_awaiting_a_tool_result() {
    // The tool parks until its signal fires, so the only way out is the
    // cancellation path through the recv loop.
    let parking = Arc::new(FnTool {
        name: "park".to_owned(),
        f: move |_call_id: String, _args: Value, signal: CancellationToken| {
            Box::pin(async move {
                signal.cancelled().await;
                Err("cancelled".to_owned()) as Result<Value, String>
            }) as ToolFuture
        },
    });
    let executor = executor_with(vec![parking]);
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        canceller.cancel();
    });
    let result = executor
        .execute("await tools.park({});", &StoreSnapshot::new(), cancel)
        .await;
    assert_eq!(
        result.error.as_ref().map(|error| error.kind),
        Some(CodemodeErrorKind::Aborted)
    );
    // The interrupted call is recorded as cancelled.
    assert_eq!(result.calls.len(), 1);
    assert_eq!(result.calls[0].status, CallStatus::Cancelled);
}

#[tokio::test]
async fn timeout_deadline_ends_the_execution() {
    let executor = echo_executor();
    let source = "// @options: {\"timeout_ms\": 100}\nwhile (true) {}";
    let result = run(&executor, source).await;
    assert_eq!(
        result.error.as_ref().map(|error| error.kind),
        Some(CodemodeErrorKind::Timeout)
    );
    assert!(result.error.unwrap().message.contains("100 ms"));
    assert!(
        result.wall_time >= Duration::from_millis(90),
        "the deadline must actually gate the run"
    );
}

#[tokio::test]
async fn hang_detection_fails_a_script_nothing_can_resume() {
    let executor = echo_executor();
    let result = run(&executor, "await new Promise(() => {});").await;
    let error = result.error.expect("hang must fail");
    assert_eq!(error.kind, CodemodeErrorKind::Script);
    assert!(
        error.message.contains("never settle"),
        "message must explain the hang: {}",
        error.message
    );
    assert!(
        result.wall_time < Duration::from_secs(2),
        "hang detection must fire immediately, took {:?}",
        result.wall_time
    );
}

#[tokio::test]
async fn memory_limit_throws_a_catchable_error() {
    let executor = echo_executor();
    let source = r#"
        try {
            const chunks = [];
            for (let i = 0; i < 4096; i++) chunks.push(new Array(1024 * 1024).fill(1));
            text("allocated everything");
        } catch (error) {
            text("caught: " + error.name + " " + error.message);
        }
        return "alive";
    "#;
    let result = run(&executor, source).await;
    let text = joined_text(&result);
    // quickjs-ng's allocator limit raises a catchable InternalError naming memory.
    assert!(
        text.contains("caught: InternalError out of memory"),
        "the 256 MiB limit must throw a catchable out-of-memory error: {text}"
    );
    assert!(
        result.error.is_none(),
        "a caught allocation failure must not fail the execution: {:?}",
        result.error
    );
}

#[tokio::test]
async fn deep_recursion_is_a_catchable_error() {
    let executor = echo_executor();
    let source = r#"
        let name = "";
        let message = "";
        try {
            const recurse = () => recurse();
            recurse();
        } catch (error) {
            name = error.name;
            message = error.message;
        }
        text("caught " + name + ": " + message);
        return "survived";
    "#;
    let result = run(&executor, source).await;
    let text = joined_text(&result);
    // quickjs-ng's stack guard raises a catchable RangeError.
    assert!(
        text.contains("caught RangeError"),
        "deep recursion must throw a catchable RangeError: {text}"
    );
    assert!(
        result.error.is_none(),
        "a caught stack overflow must not fail the execution: {:?}",
        result.error
    );
}

#[tokio::test]
async fn soft_output_limit_truncates_and_carries_full_text() {
    let executor = echo_executor();
    let source = r#"// @options: {"max_output_tokens": 10}
text("H".repeat(30));
text("T".repeat(30));
return "done";
"#;
    let result = run(&executor, source).await;
    assert!(
        result.error.is_none(),
        "the soft limit must not fail the script: {:?}",
        result.error
    );
    assert!(
        result.text.contains("characters omitted"),
        "truncated text keeps an omission marker"
    );
    assert!(result.text.starts_with("H"), "head kept");
    assert!(result.text.ends_with("T"), "tail kept");
    let full = result
        .full_output
        .as_deref()
        .expect("full output carried separately");
    let mut expected = String::new();
    expected.push_str(&"H".repeat(30));
    expected.push('\n');
    expected.push_str(&"T".repeat(30));
    assert_eq!(full, expected);
}

#[tokio::test]
async fn hard_output_char_limit_fails_the_script() {
    let executor = echo_executor();
    let source = r#"
        for (let i = 0; i < 20000; i++) text("x".repeat(1024));
        return "unreachable";
    "#;
    let result = run(&executor, source).await;
    let error = result.error.expect("hard limit must fail");
    assert_eq!(error.kind, CodemodeErrorKind::Script);
    assert!(
        error.message.contains("16") && error.message.contains("characters"),
        "{}",
        error.message
    );
    assert_ne!(result.value.as_deref(), Some("\"unreachable\""));
    assert!(
        !result.output.is_empty(),
        "output produced before the limit is kept (partial output)"
    );
}

#[tokio::test]
async fn hard_output_item_limit_fails_the_script() {
    let executor = echo_executor();
    let source = r#"
        for (let i = 0; i < 100010; i++) text("");
        return "unreachable";
    "#;
    let result = run(&executor, source).await;
    let error = result.error.expect("hard limit must fail");
    assert_eq!(error.kind, CodemodeErrorKind::Script);
    assert!(
        error.message.contains("100000") && error.message.contains("calls"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn console_block_renders_after_body() {
    let executor = echo_executor();
    let source = r#"
        text("body1");
        console.log("log line");
        text("body2");
        console.warn("warn", "line");
        return "ok";
    "#;
    let result = run(&executor, source).await;
    assert!(result.error.is_none());
    assert_eq!(
        result.text,
        "body1\nbody2\n<console_output>\nlog line\nwarn line\n</console_output>"
    );
}

#[tokio::test]
async fn image_rejects_remote_urls_and_accepts_valid_data_urls() {
    let executor = echo_executor();
    let source = r#"
        try {
            image("https://example.com/cat.png");
            text("remote accepted");
        } catch (error) {
            text("remote rejected: " + error.message);
        }
        try {
            image("data:image/png;base64,iVBORw0KGgo=");
            text("png accepted");
        } catch (error) {
            text("png rejected: " + error.message);
        }
        try {
            image({ type: "image", data: "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7", mimeType: "image/gif" });
            text("block accepted");
        } catch (error) {
            text("block rejected: " + error.message);
        }
    "#;
    let result = run(&executor, source).await;
    let text = joined_text(&result);
    assert!(
        text.contains("remote rejected"),
        "remote URLs must be rejected: {text}"
    );
    assert!(
        text.contains("png accepted"),
        "valid data URL must be accepted: {text}"
    );
    assert!(
        text.contains("block accepted"),
        "image blocks must be accepted: {text}"
    );
    let images: Vec<&str> = result
        .output
        .iter()
        .filter_map(|item| match item {
            OutputItem::Image { mime_type, .. } => Some(mime_type.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        images,
        vec!["image/png", "image/gif"],
        "signatures must be detected"
    );
}

#[tokio::test]
async fn options_drive_budget_and_timeout() {
    let executor = echo_executor();
    // 10 estimated tokens = 40 chars of budget; the script emits 60 chars of body.
    let source = format!(
        "// @options: {{\"max_output_tokens\": 10}}\ntext(\"{}{}\");\ntext(\"tail\");\n",
        "a".repeat(30),
        "b".repeat(30)
    );
    let result = run(&executor, &source).await;
    assert!(
        result.error.is_none(),
        "the soft limit must not fail the script: {:?}",
        result.error
    );
    assert!(
        result.text.contains("characters omitted"),
        "the options budget must gate the text: {}",
        result.text
    );
    assert!(result.full_output.is_some());

    let options_error = run(&executor, "// @options: {\"timeout_ms\": 1}").await;
    let error = options_error
        .error
        .expect("options without a body must fail");
    assert_eq!(error.kind, CodemodeErrorKind::Script);
    assert!(
        error
            .message
            .contains("must be followed by JavaScript source"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn store_round_trips_through_the_snapshot() {
    let executor = echo_executor();
    let mut store = StoreSnapshot::new();
    store.insert("count".to_owned(), "41".to_owned());
    store.insert("gone".to_owned(), "\"bye\"".to_owned());
    let source = r#"
        const previous = load("count");
        store("count", previous + 1);
        store("fresh", {"nested": true});
        store("gone", undefined);
        text("loaded: " + previous);
        return "stored";
    "#;
    let result = executor
        .execute(source, &store, CancellationToken::new())
        .await;
    assert!(
        result.error.is_none(),
        "unexpected error: {:?}",
        result.error
    );
    assert_eq!(joined_text(&result), "loaded: 41");
    assert_eq!(
        result.store_writes.set,
        vec![
            ("count".to_owned(), "42".to_owned()),
            ("fresh".to_owned(), "{\"nested\":true}".to_owned()),
        ]
    );
    assert_eq!(result.store_writes.delete, vec!["gone".to_owned()]);
}

#[tokio::test]
async fn failed_executions_produce_no_store_writes() {
    let executor = echo_executor();
    let source = r#"
        store("halfway", 1);
        throw new Error("blew up after storing");
    "#;
    let result = run(&executor, source).await;
    let error = result
        .error
        .expect("the thrown error must fail the execution");
    assert_eq!(error.kind, CodemodeErrorKind::Script);
    assert_eq!(error.message, "blew up after storing");
    assert_eq!(
        result.store_writes,
        manox_codemode::types::StoreWrites::default(),
        "failed runs never write"
    );
}

#[tokio::test]
async fn store_limits_throw_catchable_range_errors() {
    let executor = echo_executor();
    let source = r#"
        try {
            store("big", "v".repeat(256 * 1024 + 1));
            text("big accepted");
        } catch (error) {
            text("big caught: " + error.name);
        }
        try {
            for (let i = 0; i < 5; i++) store("key" + i, "v".repeat(256 * 1024));
            text("total accepted");
        } catch (error) {
            text("total caught: " + error.name);
        }
        return "survived";
    "#;
    let result = run(&executor, source).await;
    let text = joined_text(&result);
    assert!(
        text.contains("big caught: RangeError"),
        "single-value limit: {text}"
    );
    assert!(
        text.contains("total caught: RangeError"),
        "total limit: {text}"
    );
    assert!(
        result.error.is_none(),
        "caught range errors must not fail the run: {:?}",
        result.error
    );
}

#[tokio::test]
async fn all_tools_and_describe_tool_discover_the_surface() {
    let executor = echo_executor();
    let source = r#"
        text("count=" + ALL_TOOLS.length);
        text("frozen=" + Object.isFrozen(ALL_TOOLS));
        const sample = describeTool("echo");
        text("sample has declaration: " + (sample.includes("declare const tools")));
        text("sample has description: " + (sample.includes("Echoes its arguments back")));
        text("unknown is undefined: " + (describeTool("no-such-tool") === undefined));
        text("normalized lookup: " + (describeTool("weird_name") !== undefined));
    "#;
    let result = run(&executor, source).await;
    assert!(
        result.error.is_none(),
        "unexpected error: {:?}",
        result.error
    );
    let text = joined_text(&result);
    assert!(text.contains("count=5"), "every tool is listed: {text}");
    assert!(text.contains("frozen=true"), "{text}");
    assert!(text.contains("sample has declaration: true"), "{text}");
    assert!(text.contains("sample has description: true"), "{text}");
    assert!(text.contains("unknown is undefined: true"), "{text}");
    assert!(text.contains("normalized lookup: true"), "{text}");
}

#[tokio::test]
async fn tool_errors_and_unknown_names_reject() {
    let executor = echo_executor();
    let source = r#"
        try {
            await tools.broken({});
            text("error call resolved");
        } catch (error) {
            text("tool error: " + error.message);
        }
        try {
            await tools.no_such_tool({});
            text("unknown resolved");
        } catch (error) {
            text("unknown rejected: " + error.message);
        }
        const viaBracket = await tools["weird-name"]({});
        const viaDot = await tools.weird_name({});
        text("original name: " + viaBracket.ok + " normalized: " + viaDot.ok);
    "#;
    let result = run(&executor, source).await;
    let text = joined_text(&result);
    assert!(
        text.contains("tool error: boom"),
        "the tool's error text must become the rejection: {text}"
    );
    assert!(
        text.contains("unknown rejected: tools.no_such_tool is not an available tool"),
        "unknown members reject with a helpful TypeError: {text}"
    );
    assert!(
        text.contains("original name: true normalized: true"),
        "original and normalized names both resolve: {text}"
    );
    // Unknown names are rejected inside the VM by the tools proxy, so they
    // never leave it: no call record, and no host dispatch.
    assert!(result.calls.iter().all(|call| call.name != "no_such_tool"));
}

#[tokio::test]
async fn in_flight_cap_rejects_overflow_calls() {
    let tool = TestTool::slow("slow", 100);
    let executor = executor_with(vec![tool]);
    let source = r#"
        const calls = [];
        for (let i = 0; i < 40; i++) calls.push(tools.slow({ index: i }));
        const settled = await Promise.allSettled(calls);
        const rejected = settled.filter((outcome) => outcome.status === "rejected");
        return { total: settled.length, rejected: rejected.length, reason: String(rejected[0].reason.message) };
    "#;
    let result = run(&executor, source).await;
    assert!(
        result.error.is_none(),
        "allSettled must keep the script alive: {:?}",
        result.error
    );
    let returned: Value =
        serde_json::from_str(result.value.as_deref().expect("return value")).expect("json object");
    assert_eq!(returned["total"], 40);
    assert_eq!(
        returned["rejected"], 8,
        "calls beyond the 32 cap reject immediately: {returned}"
    );
    assert!(
        returned["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("limit is 32"),
        "the rejection explains the cap: {returned}"
    );
}

#[tokio::test]
async fn return_and_exit_semantics() {
    let executor = echo_executor();
    let result = run(&executor, "return {a: [1, 2]};").await;
    assert!(result.error.is_none());
    assert_eq!(result.value.as_deref(), Some("{\"a\":[1,2]}"));

    let no_return = run(&executor, "text(\"side effect\");").await;
    assert!(no_return.error.is_none());
    assert_eq!(no_return.value, None);

    let source = r#"
        text("before");
        exit();
        text("after");
    "#;
    let exited = run(&executor, source).await;
    assert!(
        exited.error.is_none(),
        "exit() is a successful end: {:?}",
        exited.error
    );
    assert_eq!(
        joined_text(&exited),
        "before",
        "statements after exit() never run"
    );
}

#[tokio::test]
async fn arguments_and_echo_travel_as_json() {
    let executor = echo_executor();
    let source = r#"
        const result = await tools.echo({ path: "a/b.txt", nested: { n: 3 } });
        text(result.args.path + " " + result.args.nested.n + " id:" + (typeof result.callId));
        return typeof result;
    "#;
    let result = run(&executor, source).await;
    assert!(
        result.error.is_none(),
        "unexpected error: {:?}",
        result.error
    );
    assert_eq!(joined_text(&result), "a/b.txt 3 id:string");
    assert_eq!(result.value.as_deref(), Some("\"object\""));
}

#[tokio::test]
async fn syntax_errors_fail_as_script_errors_with_line_info() {
    let executor = echo_executor();
    let source = "const ok = 1;\nthis is not javascript;\n";
    let result = run(&executor, source).await;
    let error = result.error.expect("a syntax error must fail");
    assert_eq!(error.kind, CodemodeErrorKind::Script);
    let location = error.stack.as_deref().unwrap_or_default();
    assert!(
        location.contains("codemode.js:2") || error.message.contains("line 2"),
        "the error should point at line 2: msg={:?} stack={:?}",
        error.message,
        error.stack
    );
}

#[tokio::test]
async fn declarations_render_into_describe_tool() {
    let executor = echo_executor();
    let source = r#"
        const sample = describeTool("answer");
        return sample;
    "#;
    let result = run(&executor, source).await;
    let sample = result.value.as_deref().expect("describeTool sample");
    assert!(sample.contains("declare const tools"));
    assert!(sample.contains("```ts"));
    assert!(sample.contains("answer(args:"));
    let _ = DEFAULT_DECLARATION_BUDGET_TOKENS;
    let _ = to_js_identifier;
}
