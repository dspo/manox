//! `BashOutput` tool — poll a background shell started by `Bash` with
//! `run_in_background: true`, returning incremental stdout/stderr since the
//! last poll plus the shell's running/exit status. Mirrors Claude Code's
//! `BashOutput` tool.
//!
//! This tool is stateless (the background shell state lives in
//! [`background_shell`]); it is registered in `base_tools` so both the main
//! thread and sub-agents can poll background shells. Read-only and
//! approval-free — polling output does not mutate the world.

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;
use crate::tools::background_shell;
use crate::tools::schema;

pub struct BashOutputTool;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BashOutputInput {
    /// The shell id returned by `Bash` when it started the command with
    /// `run_in_background: true`.
    shell_id: String,
    /// When true, block (up to `timeout_secs`) waiting for new output before
    /// returning. When false (default), return immediately with whatever
    /// output has accumulated since the last poll. The blocking form is useful
    /// when the model knows the process emits output slowly and wants to avoid
    /// a busy-poll loop.
    #[serde(default)]
    block: Option<bool>,
    /// Seconds to wait when `block` is true (default 10, max 60). Ignored when
    /// `block` is false.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

impl AgentTool for BashOutputTool {
    fn name(&self) -> &str {
        "BashOutput"
    }

    fn description(&self) -> &str {
        "Poll a background shell started by `Bash` with `run_in_background: true`. \
         Returns the incremental stdout/stderr produced since the last poll, plus \
         the shell's running/exit status. Pass the `shell_id` returned by the \
         original `Bash` call. Poll repeatedly until the status shows the process \
         has exited, then read the final output. A 10+ minute run is normal — \
         keep polling patiently; never return a placeholder like \"still running\" \
         as the result."
    }

    fn input_schema(&self) -> serde_json::Value {
        schema::<BashOutputInput>()
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn run(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        _ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parsed = match serde_json::from_value::<BashOutputInput>(input) {
            Ok(p) => p,
            Err(e) => {
                return cx.background_spawn(async move { Err(format!("input parse failed: {e}")) });
            }
        };
        let shell_id = parsed.shell_id;
        let block = parsed.block.unwrap_or(false);
        let timeout_secs = parsed.timeout_secs.unwrap_or(10).min(60);
        let timeout = std::time::Duration::from_secs(timeout_secs);

        bridge_blocking_poll(cx, shell_id, block, timeout, cancel)
    }
}

/// Bridge the (optionally blocking) poll to the gpui executor. A blocking poll
/// spins on `background_shell::poll` with a short sleep until new output is
/// available, the process exits, or the timeout/cancel fires.
fn bridge_blocking_poll(
    cx: &mut App,
    shell_id: String,
    block: bool,
    timeout: std::time::Duration,
    cancel: CancellationToken,
) -> Task<Result<String, String>> {
    let (tx, rx) = async_channel::bounded(1);
    crate::runtime::handle().spawn(async move {
        let result: Result<String, String> = async {
            if block {
                let deadline = std::time::Instant::now() + timeout;
                loop {
                    let r = background_shell::poll(&shell_id)?;
                    if !r.new_output.is_empty() || !r.is_running {
                        return Ok(r.render());
                    }
                    if std::time::Instant::now() >= deadline {
                        return Ok(r.render());
                    }
                    if cancel.is_cancelled() {
                        return Err("BashOutput cancelled".to_string());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            } else {
                let r = background_shell::poll(&shell_id)?;
                Ok(r.render())
            }
        }
        .await;
        let _ = tx.send(result).await;
    });
    cx.background_spawn(async move {
        rx.recv()
            .await
            .map_err(|_| "BashOutput cancelled".to_string())
            .and_then(|r| r)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_bashoutput() {
        assert_eq!(BashOutputTool.name(), "BashOutput");
    }

    #[test]
    fn is_read_only() {
        assert!(BashOutputTool.is_read_only());
    }

    #[test]
    fn does_not_require_approval() {
        assert!(!BashOutputTool.requires_approval(&serde_json::json!({"shell_id": "x"})));
    }

    #[test]
    fn input_parses_shell_id() {
        let v = serde_json::json!({"shell_id": "bash_1"});
        let p: BashOutputInput = serde_json::from_value(v).unwrap();
        assert_eq!(p.shell_id, "bash_1");
        assert_eq!(p.block, None);
    }

    #[test]
    fn input_parses_block_flag() {
        let v = serde_json::json!({"shell_id": "bash_1", "block": true, "timeout_secs": 30});
        let p: BashOutputInput = serde_json::from_value(v).unwrap();
        assert_eq!(p.block, Some(true));
        assert_eq!(p.timeout_secs, Some(30));
    }
}
