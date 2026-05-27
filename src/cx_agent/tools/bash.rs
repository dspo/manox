use std::process::Stdio;
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::timeout;

use crate::cx_agent::approval::ToolCategory;

use super::{Tool, display_path, parse_args, validate_path};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

pub struct BashTool;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    cwd: Option<String>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Execute
    }

    fn description(&self) -> &'static str {
        "Run a shell command with a 60 second timeout and return combined stdout/stderr."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional working directory for the command."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    async fn invoke(&self, arguments: Value) -> Result<String> {
        let args: BashArgs = parse_args(self.name(), arguments)?;
        if args.command.trim().is_empty() {
            return Err(anyhow!("bash command must not be empty"));
        }
        run_command(&args.command, args.cwd.as_deref(), DEFAULT_TIMEOUT).await
    }
}

async fn run_command(command: &str, cwd: Option<&str>, timeout_window: Duration) -> Result<String> {
    let mut cmd = shell_command(command);
    cmd.kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(cwd) = cwd {
        let path = validate_path("bash", cwd)?;
        if !path.exists() {
            return Err(anyhow!("bash cwd does not exist: {}", display_path(&path)));
        }
        cmd.current_dir(&path);
    }

    let output = timeout(timeout_window, cmd.output())
        .await
        .map_err(|_| anyhow!("bash timed out after {} seconds", timeout_window.as_secs()))?
        .map_err(|err| anyhow!("bash failed to spawn command: {err}"))?;

    let combined = combine_output(&output.stdout, &output.stderr);
    if output.status.success() {
        if combined.is_empty() {
            Ok("Command completed successfully with no output.".to_string())
        } else {
            Ok(combined)
        }
    } else {
        Err(anyhow!(
            "bash exited with status {}{}",
            render_status(&output.status),
            if combined.is_empty() {
                String::new()
            } else {
                format!("\n{combined}")
            }
        ))
    }
}

fn shell_command(command: &str) -> Command {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut cmd = Command::new("bash");
        cmd.arg("-lc").arg(command);
        cmd
    }
}

fn combine_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn render_status(status: &std::process::ExitStatus) -> String {
    status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated by signal".to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::{BashTool, run_command};
    use crate::cx_agent::tools::Tool;

    #[tokio::test(flavor = "current_thread")]
    async fn returns_combined_stdout_and_stderr() {
        let tool = BashTool;
        let output = tool
            .invoke(json!({
                "command": "printf 'alpha'; printf 'beta' 1>&2"
            }))
            .await
            .expect("bash success");

        assert!(output.contains("alpha"));
        assert!(output.contains("beta"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn times_out_commands() {
        let err = run_command("sleep 1", None, Duration::from_millis(10))
            .await
            .expect_err("command should time out");

        assert!(err.to_string().contains("timed out"));
    }
}
