// Persistent shell backend for the bash tool.
//
// A `brush_core::Shell` lives for the lifetime of this backend: `cd`,
// `export`, and function definitions persist across commands, so the model
// does not have to re-pin the cwd on every call. Commands are serialized
// through the one shell; each external command runs in its own process group
// so cancel/timeout can reap the whole tree.

use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use brush_builtins::ShellBuilderExt;
use brush_core::openfiles::{self, OpenFile, OpenFiles};
use brush_core::{ExecutionExitCode, ExecutionResult, ProcessGroupPolicy, Shell, SourceInfo};
use pi::env::{CommandResult, ExecutionError};
use pi::tools::bash::{BashExecRequest, BashOperations};
use tokio::sync::Mutex;

/// Outcome of a brush run; the captured streams are drained separately.
enum Outcome {
    Ran(Result<ExecutionResult, brush_core::Error>),
    TimedOut,
    Cancelled,
}

/// The grace window given to a SIGTERM'd process group before escalating to
/// SIGKILL.
const CANCELLATION_GRACE_MS: u64 = 50;
/// Upper bound on draining the pipes after a cancelled run whose process
/// group could not be reaped.
const IO_DRAIN_TIMEOUT_SECS: u64 = 2;

/// A `BashOperations` backend backed by a persistent brush shell session.
pub struct PersistentShellOperations {
    /// Lazily-initialized brush shell; one session per backend instance.
    shell: Arc<Mutex<Option<Shell>>>,
    /// The working directory the shell is seeded with on first use.
    base_cwd: PathBuf,
    /// Exported env vars injected into the shell on first use.
    env: Vec<(String, String)>,
}

impl PersistentShellOperations {
    pub fn new(base_cwd: impl Into<PathBuf>) -> Self {
        PersistentShellOperations {
            shell: Arc::new(Mutex::new(None)),
            base_cwd: base_cwd.into(),
            env: Vec::new(),
        }
    }

    /// Seed an exported env var into the shell on first use. Exported vars
    /// reach child processes; plain shell variables do not.
    pub fn with_env_var(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

#[async_trait::async_trait]
impl BashOperations for PersistentShellOperations {
    async fn exec(&self, request: BashExecRequest<'_>) -> Result<CommandResult, ExecutionError> {
        // Pipes carry the external command's stdout/stderr: brush maps an
        // in-memory `OpenFile::Stream` to `Stdio::null()` for children, so a
        // pipe is the only way their output reaches us.
        let (out_r, out_w) = std::io::pipe().map_err(io_err)?;
        let (err_r, err_w) = std::io::pipe().map_err(io_err)?;

        // Reader threads only forward bytes into channels; the async loop
        // below aggregates and forwards to `on_data`, so the callback's
        // lifetime never leaks into a spawned task.
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (err_tx, mut err_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        // Detached reader threads: they exit once their pipe hits EOF, and a
        // std thread is not tracked by the runtime, so a hung command whose
        // process group could not be reaped does not stall shutdown.
        let _ = std::thread::spawn(move || read_pipe(out_r, out_tx));
        let _ = std::thread::spawn(move || read_pipe(err_r, err_tx));

        let mut out_buf: Vec<u8> = Vec::new();
        let mut err_buf: Vec<u8> = Vec::new();

        // Serialize through the one shell: a shell session cannot run two
        // commands concurrently without interleaving their state.
        let mut guard = self.shell.lock().await;
        if guard.is_none() {
            let mut s = Shell::builder()
                .default_builtins(brush_builtins::BuiltinSet::BashMode)
                .build()
                .await
                .map_err(brush_err)?;
            s.set_working_dir(&self.base_cwd).map_err(brush_err)?;
            for (k, v) in &self.env {
                let mut var = brush_core::ShellVariable::new(v.clone());
                var.export();
                s.set_env_global(k, var).map_err(brush_err)?;
            }
            *guard = Some(s);
        }

        let outcome = {
            let sh = guard.as_mut().expect("shell initialized");
            // A cwd override re-pins the shell; `None` keeps the current
            // directory so `cd` persists across calls.
            if let Some(cwd) = request.cwd {
                sh.set_working_dir(cwd).map_err(brush_err)?;
            }

            let mut params = sh.default_exec_params();
            params.process_group_policy = ProcessGroupPolicy::NewProcessGroup;
            params.set_fd(OpenFiles::STDIN_FD, openfiles::null().map_err(brush_err)?);
            params.set_fd(OpenFiles::STDOUT_FD, OpenFile::from(out_w));
            params.set_fd(OpenFiles::STDERR_FD, OpenFile::from(err_w));
            let source = SourceInfo::default();

            let cancel = request.signal.clone();
            let mut run_fut = Box::pin(sh.run_string(request.command, &source, &params));
            let mut sleep_fut = request.timeout.map(|t| Box::pin(tokio::time::sleep(t)));
            let outcome = loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break Outcome::Cancelled,
                    () = sleep_fut.as_mut().expect("guarded"), if sleep_fut.is_some() => break Outcome::TimedOut,
                    r = run_fut.as_mut() => break Outcome::Ran(r),
                    data = out_rx.recv() => {
                        if let Some(data) = data {
                            out_buf.extend_from_slice(&data);
                            forward_on_data(&request.on_data, &data);
                        }
                    }
                    data = err_rx.recv() => {
                        if let Some(data) = data {
                            err_buf.extend_from_slice(&data);
                            forward_on_data(&request.on_data, &data);
                        }
                    }
                }
            };

            // Drop the run future first: it borrows the shell and the params
            // (and thus the pipe write ends).
            drop(run_fut);

            // brush does not kill_on_drop, so a cancelled/timed-out run leaves
            // the spawned groups orphaned. Reap them now that the `run_string`
            // borrow is released.
            if matches!(outcome, Outcome::Cancelled | Outcome::TimedOut) {
                // brush only tracks stopped/background jobs, so a foreground
                // command may not be reaped here; it exits on its own. The
                // job table is left intact — a previous `&` task must stay
                // waitable across a cancelled run.
                kill_jobs(sh, libc::SIGTERM);
                tokio::time::sleep(Duration::from_millis(CANCELLATION_GRACE_MS)).await;
                kill_jobs(sh, libc::SIGKILL);
            }

            // `params` drops here, closing the pipe write ends so the readers
            // hit EOF.
            outcome
        };
        drop(guard);

        // A cancelled command's process group can outlive the reap: brush
        // only tracks stopped/background jobs, so a foreground process is not
        // in `jobs` and keeps its pipes open. Bound the drain so a hung
        // command cannot stall the turn (the process reaps on its own exit).
        let _ = tokio::time::timeout(Duration::from_secs(IO_DRAIN_TIMEOUT_SECS), async {
            loop {
                tokio::select! {
                    data = out_rx.recv() => if let Some(d) = data {
                        out_buf.extend_from_slice(&d);
                        forward_on_data(&request.on_data, &d);
                    },
                    data = err_rx.recv() => if let Some(d) = data {
                        err_buf.extend_from_slice(&d);
                        forward_on_data(&request.on_data, &d);
                    },
                    else => break,
                }
            }
        })
        .await;

        let stdout = String::from_utf8_lossy(&out_buf).to_string();
        let stderr = String::from_utf8_lossy(&err_buf).to_string();

        match outcome {
            Outcome::Ran(Ok(result)) => Ok(CommandResult {
                stdout,
                stderr,
                exit_code: exit_code_num(result.exit_code) as i32,
            }),
            Outcome::Ran(Err(e)) => Err(ExecutionError::Other(format!("{e}"))),
            Outcome::TimedOut => Err(ExecutionError::Timeout(request.timeout.unwrap_or_default())),
            Outcome::Cancelled => Err(ExecutionError::Aborted),
        }
    }
}

/// Forward a chunk to the request's streaming callback, if any.
fn forward_on_data(on_data: &Option<pi::tools::bash::BashDataCallback<'_>>, data: &[u8]) {
    if let Some(f) = on_data {
        f(data);
    }
}

/// Blockingly drain a pipe into an unbounded channel until EOF.
fn read_pipe(mut r: std::io::PipeReader, tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>) {
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send(chunk[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

/// Signal every tracked job's process group. With `NewProcessGroup` each
/// external command leads its own group (pgid == child pid), so `kill(-pgid)`
/// reaches the whole tree.
#[cfg(unix)]
fn kill_jobs(shell: &Shell, sig: i32) {
    for job in &shell.jobs().jobs {
        if let Some(pgid) = job.process_group_id() {
            // Best-effort: the group may already be gone by the time we escalate.
            unsafe {
                let _ = libc::kill(-pgid, sig);
            }
        }
    }
}

#[cfg(not(unix))]
fn kill_jobs(_shell: &Shell, _sig: i32) {}

/// Numeric exit code. brush maps well-known codes to named variants;
/// `Custom(u8)` carries anything else.
fn exit_code_num(code: ExecutionExitCode) -> u8 {
    match code {
        ExecutionExitCode::Success => 0,
        ExecutionExitCode::GeneralError => 1,
        ExecutionExitCode::InvalidUsage => 2,
        ExecutionExitCode::Unimplemented => 99,
        ExecutionExitCode::CannotExecute => 126,
        ExecutionExitCode::NotFound => 127,
        ExecutionExitCode::Interrupted => 130,
        ExecutionExitCode::BrokenPipe => 141,
        ExecutionExitCode::Custom(c) => c,
    }
}

fn io_err(e: std::io::Error) -> ExecutionError {
    ExecutionError::Other(format!("pipe error: {e}"))
}

fn brush_err(e: brush_core::Error) -> ExecutionError {
    ExecutionError::Other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn request<'a>(
        command: &'a str,
        cwd: Option<&'a Path>,
        timeout: Option<Duration>,
        signal: tokio_util::sync::CancellationToken,
    ) -> BashExecRequest<'a> {
        BashExecRequest {
            command,
            cwd,
            timeout,
            signal,
            on_data: None,
        }
    }

    #[tokio::test]
    async fn cd_persists_across_commands() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("marker"), "x").unwrap();

        let ops = PersistentShellOperations::new(dir.path());
        let signal = tokio_util::sync::CancellationToken::new();

        let r1 = ops
            .exec(request(
                "cd sub",
                Some(dir.path()),
                Some(Duration::from_secs(5)),
                signal.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(r1.exit_code, 0);

        // The shell still sits in `sub` from the previous call: no cwd
        // override keeps the `cd`.
        let r2 = ops
            .exec(request(
                "pwd",
                None,
                Some(Duration::from_secs(5)),
                signal.clone(),
            ))
            .await
            .unwrap();
        assert!(
            r2.stdout.trim().ends_with("sub"),
            "cwd persists: {}",
            r2.stdout
        );
    }

    #[tokio::test]
    async fn export_persists_across_commands() {
        let dir = tempfile::tempdir().unwrap();
        let ops = PersistentShellOperations::new(dir.path());
        let signal = tokio_util::sync::CancellationToken::new();

        let r1 = ops
            .exec(request(
                "export FOO=bar",
                Some(dir.path()),
                Some(Duration::from_secs(5)),
                signal.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(r1.exit_code, 0);

        let r2 = ops
            .exec(request(
                "echo $FOO",
                Some(dir.path()),
                Some(Duration::from_secs(5)),
                signal.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(r2.stdout.trim(), "bar");
    }

    #[tokio::test]
    async fn function_definition_persists() {
        let dir = tempfile::tempdir().unwrap();
        let ops = PersistentShellOperations::new(dir.path());
        let signal = tokio_util::sync::CancellationToken::new();

        let r1 = ops
            .exec(request(
                "f() { echo defined; }",
                Some(dir.path()),
                Some(Duration::from_secs(5)),
                signal.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(r1.exit_code, 0);

        let r2 = ops
            .exec(request(
                "f",
                Some(dir.path()),
                Some(Duration::from_secs(5)),
                signal.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(r2.stdout.trim(), "defined");
    }

    #[tokio::test]
    async fn merges_stderr_and_reports_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let ops = PersistentShellOperations::new(dir.path());
        let signal = tokio_util::sync::CancellationToken::new();

        let r = ops
            .exec(request(
                "echo out; echo err >&2; exit 3",
                Some(dir.path()),
                Some(Duration::from_secs(5)),
                signal,
            ))
            .await
            .unwrap();
        assert_eq!(r.exit_code, 3);
        assert_eq!(r.stdout.trim(), "out");
        assert_eq!(r.stderr.trim(), "err");
    }

    #[tokio::test]
    async fn cancel_kills_the_running_command() {
        let dir = tempfile::tempdir().unwrap();
        let ops = PersistentShellOperations::new(dir.path());
        let token = tokio_util::sync::CancellationToken::new();
        let signal = token.clone();

        let started = std::time::Instant::now();
        let exec = tokio::spawn(async move {
            ops.exec(request(
                "sleep 30",
                Some(dir.path()),
                Some(Duration::from_secs(60)),
                signal,
            ))
            .await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        token.cancel();
        let result = exec.await.unwrap();
        assert!(matches!(result, Err(ExecutionError::Aborted)));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the cancelled run returns promptly, not after the command exits"
        );
    }

    #[tokio::test]
    async fn timeout_kills_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let ops = PersistentShellOperations::new(dir.path());
        let started = std::time::Instant::now();
        let result = ops
            .exec(request(
                "sleep 30",
                Some(dir.path()),
                Some(Duration::from_millis(200)),
                tokio_util::sync::CancellationToken::new(),
            ))
            .await;
        assert!(matches!(result, Err(ExecutionError::Timeout(_))));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn streams_chunks_to_on_data() {
        let dir = tempfile::tempdir().unwrap();
        let ops = PersistentShellOperations::new(dir.path());
        let signal = tokio_util::sync::CancellationToken::new();
        let seen: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);

        let on_data: &(dyn Fn(&[u8]) + Send + Sync) = &move |data: &[u8]| {
            seen2.lock().unwrap().extend_from_slice(data);
        };
        let result = ops
            .exec(BashExecRequest {
                command: "echo hello",
                cwd: Some(dir.path()),
                timeout: Some(Duration::from_secs(5)),
                signal,
                on_data: Some(on_data),
            })
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        let captured = seen.lock().unwrap().clone();
        assert!(
            String::from_utf8_lossy(&captured).contains("hello"),
            "on_data receives the output chunks"
        );
    }
}
