//! `monitor` tool — stream stdout/stderr from a long-running shell command line
//! by line, so live progress (a build, a deploy, a log tail) reaches the UI
//! while the command runs. Mirrors Claude Code's `Monitor` tool: it is a
//! main-thread tool, not a sub-agent, because the value is in the streaming
//! events rather than a final report.
//!
//! The command runs under `sh -c` on the global tokio runtime (`runtime::handle`).
//! stdout and stderr are each read line-by-line; every line is forwarded to the
//! [`ToolOutputSink`] (which the owning `Thread` drains into
//! `ThreadEvent::ToolOutput`) and appended to a capped capture buffer. On
//! cancel, timeout, or natural exit the child is reaped (`kill_on_drop` plus an
//! explicit `kill` on the cancel/timeout branches) and the captured text is
//! returned as the tool result.

use std::process::Stdio;
use std::time::Duration;

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::tool::{AgentTool, ToolOutputSink};
use crate::tools::schema;

/// Default wall-clock limit before a hung command is killed.
const DEFAULT_TIMEOUT_MS: u64 = 300_000;
/// Upper bound on the user-supplied timeout. `monitor` is for bounded watches;
/// an unbounded `tail -f` style listener is out of scope for the tool layer's
/// `Task` lifecycle.
const MAX_TIMEOUT_MS: u64 = 3_600_000;
/// Hard cap on the captured text so a runaway command cannot OOM the app or
/// flood the model's context. Mirrors `bash`'s cap.
const MONITOR_OUTPUT_MAX_BYTES: usize = 64 * 1024;

#[derive(Deserialize, JsonSchema)]
struct MonitorInput {
    /// Shell command to run (`sh -c`). stdout and stderr are streamed line by
    /// line; each line becomes a live update. Filter the command to emit only
    /// lines you would act on — do not pipe raw noisy logs.
    command: String,
    /// One-line summary of what is being monitored, shown in the approval
    /// prompt so the user can tell apart concurrent monitors. Read by the UI
    /// from the raw input, not by the tool itself.
    #[allow(dead_code)]
    description: String,
    /// Wall-clock limit in milliseconds before the command is killed. Default
    /// 5 min; clamped to 1 hour.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub struct MonitorTool;

impl AgentTool for MonitorTool {
    fn name(&self) -> &str {
        "monitor"
    }

    fn description(&self) -> &str {
        "Stream stdout/stderr from a long-running shell command line by line. Each \
         line becomes a live update while the command runs. Use for watching a \
         build, a deploy, a log tail, or any process whose intermediate output \
         matters. The command runs under `sh -c`. Filter the command so it emits \
         only lines you would act on — do not pipe raw noisy logs. Cover terminal \
         states: a crash, a hang, or an unexpected exit must all produce output. \
         Returns the full captured text as the tool result."
    }

    fn input_schema(&self) -> serde_json::Value {
        schema::<MonitorInput>()
    }

    /// Approval required: `monitor` spawns an arbitrary shell command, so the
    /// user must vet it — same contract as `bash`.
    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn run(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let (sink, _rx) = ToolOutputSink::channel("".into());
        drop(_rx);
        self.run_streaming(input, cancel, sink, cx)
    }

    fn run_streaming(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        sink: ToolOutputSink,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parsed: MonitorInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return cx.background_spawn(async move {
                    Err(format!("monitor input parse failed: {e}"))
                });
            }
        };
        let timeout_ms = parsed
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);
        let timeout = Duration::from_millis(timeout_ms);

        // Spawn the monitoring future on the tokio runtime (tokio::process needs
        // a tokio reactor); `sink.try_emit` is executor-agnostic, so live lines
        // reach the gpui-side `ThreadEvent::ToolOutput` regardless of which
        // executor drains the channel.
        let handle = crate::runtime::handle().clone();
        let (done_tx, done_rx) = async_channel::bounded::<Result<String, String>>(1);
        handle.spawn(async move {
            let result = run_monitor(parsed, timeout, cancel, sink).await;
            let _ = done_tx.send(result).await;
        });
        cx.background_spawn(async move {
            done_rx
                .recv()
                .await
                .map_err(|_| "monitor cancelled".to_string())
                .and_then(|r| r)
        })
    }
}

/// Drive one `sh -c` command: stream stdout+stderr lines to the sink, honor
/// cancel/timeout, return the captured text and exit status.
async fn run_monitor(
    parsed: MonitorInput,
    timeout: Duration,
    cancel: CancellationToken,
    sink: ToolOutputSink,
) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(&parsed.command);
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Reap grandchildren that inherited the pipes if we drop the handle
        // without a clean exit.
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Err(format!("failed to spawn monitor command: {e}")),
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    // Drain stderr on a separate task into the same sink so error lines surface
    // live alongside stdout. Order across the two streams is not synchronized
    // (and cannot be without a single merged fd), but liveness is the point.
    let stderr_sink = sink.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => stderr_sink.try_emit(&format!("{line}\n")),
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });

    let mut captured = String::new();
    let mut truncated = false;
    let mut reader = BufReader::new(stdout).lines();
    let exit_reason = loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                sink.try_emit("⚠ monitor cancelled\n");
                break "cancelled";
            }
            _ = tokio::time::sleep(timeout) => {
                let _ = child.kill().await;
                sink.try_emit(&format!(
                    "⚠ monitor timed out after {}ms\n",
                    timeout.as_millis()
                ));
                break "timeout";
            }
            line = reader.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        sink.try_emit(&format!("{l}\n"));
                        if captured.len() < MONITOR_OUTPUT_MAX_BYTES {
                            captured.push_str(&l);
                            captured.push('\n');
                        } else {
                            truncated = true;
                        }
                    }
                    Ok(None) => break "eof",
                    Err(e) => {
                        captured.push_str(&format!("read error: {e}\n"));
                        break "read-error";
                    }
                }
            }
        }
    };
    let _ = stderr_task.await;

    // Best-effort: wait for the child so its exit status is observed. After a
    // kill this completes promptly; after EOF it is the natural exit.
    let status = child.wait().await;
    if truncated {
        captured.push_str(&format!(
            "⚠ output exceeded {} bytes; later lines were dropped from this capture but still streamed live.\n",
            MONITOR_OUTPUT_MAX_BYTES
        ));
    }

    let _ = exit_reason;
    match status {
        Ok(s) if s.success() => Ok(captured),
        Ok(s) => Err(format!("command exited: {s}\n{captured}")),
        Err(e) => Err(format!("wait failed: {e}\n{captured}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_monitor_input() {
        let v = serde_json::json!({
            "command": "echo hi",
            "description": "watch the echo",
        });
        let m: MonitorInput = serde_json::from_value(v).unwrap();
        assert_eq!(m.command, "echo hi");
        assert_eq!(m.description, "watch the echo");
        assert!(m.timeout_ms.is_none());
    }

    #[test]
    fn parses_with_timeout() {
        let v = serde_json::json!({
            "command": "x",
            "description": "d",
            "timeout_ms": 5000,
        });
        let m: MonitorInput = serde_json::from_value(v).unwrap();
        assert_eq!(m.timeout_ms, Some(5000));
    }
}
