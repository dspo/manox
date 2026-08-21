// Platform abstraction for the harness — filesystem and shell operations.
//
// The harness never touches the real filesystem or spawns processes directly.
// It calls through this trait, which keeps the core loop runtime-agnostic and
// testable via mocks.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// The result of a shell command execution.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// File metadata.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
}

/// The environment the harness executes in: filesystem + shell.
///
/// All methods are async and return `Result`; the harness treats errors as
/// operational failures, not bugs.
#[async_trait::async_trait]
pub trait ExecutionEnv: Send + Sync {
    /// The current working directory for relative path resolution.
    fn cwd(&self) -> &Path;

    /// Resolve a path to an absolute form.
    async fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError>;

    /// Join path segments (platform-aware).
    fn join_path(&self, parts: &[&str]) -> PathBuf;

    /// Read a file as a UTF-8 string, optionally with offset and limit.
    async fn read_file(
        &self,
        path: &Path,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<String, FileError>;

    /// Write content to a file, creating parent directories as needed.
    async fn write_file(&self, path: &Path, content: &str) -> Result<(), FileError>;

    /// Check whether a path exists.
    async fn exists(&self, path: &Path) -> Result<bool, FileError>;

    /// Get file metadata.
    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError>;

    /// List directory contents.
    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError>;

    /// Create a directory, including parents.
    async fn create_dir(&self, path: &Path) -> Result<(), FileError>;

    /// Remove a file or directory.
    async fn remove(&self, path: &Path) -> Result<(), FileError>;

    /// Execute a shell command with a timeout. A cancelled `signal` kills the
    /// process tree rather than waiting for the command to finish.
    async fn exec(
        &self,
        command: &str,
        timeout: Duration,
        signal: CancellationToken,
    ) -> Result<CommandResult, ExecutionError>;
}

/// Errors from filesystem operations.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("file not found: {0}")]
    NotFound(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("is a directory: {0}")]
    IsDirectory(String),
    #[error("not a directory: {0}")]
    NotDirectory(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Errors from shell command execution.
#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("command timed out after {0:?}")]
    Timeout(Duration),
    /// The cancellation token fired while the command ran; the process tree
    /// was killed.
    #[error("aborted")]
    Aborted,
    #[error("command exited with code {exit_code}: {stderr}")]
    NonZeroExit { exit_code: i32, stderr: String },
    #[error("spawn error: {0}")]
    Spawn(String),
    #[error("{0}")]
    Other(String),
}

// ── Tokio-based production implementation ────────────────────────────────────

use std::io;

/// The default `ExecutionEnv` backed by tokio — real filesystem and shell.
///
/// Uses `tokio::fs` for file operations and `tokio::process::Command` for
/// shell execution. Timeouts are enforced via `tokio::time::timeout`.
pub struct TokioExecutionEnv {
    cwd: PathBuf,
}

impl TokioExecutionEnv {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        TokioExecutionEnv { cwd: cwd.into() }
    }
}

#[async_trait::async_trait]
impl ExecutionEnv for TokioExecutionEnv {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn join_path(&self, parts: &[&str]) -> PathBuf {
        let mut path = self.cwd.clone();
        for part in parts {
            path.push(part);
        }
        path
    }

    async fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        tokio::fs::canonicalize(&resolved)
            .await
            .map_err(|e| map_io_error(e, &resolved))
    }

    async fn read_file(
        &self,
        path: &Path,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<String, FileError> {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| map_io_error(e, path))?;

        // An unqualified read is a raw read: line endings, BOM, and the
        // trailing newline survive verbatim (hashline tools depend on them
        // for tag validation and minimal-delta writes).
        if offset.is_none() && limit.is_none() {
            return Ok(content);
        }

        let lines: Vec<&str> = content.lines().collect();
        let start = offset.unwrap_or(0).min(lines.len());
        let end = match limit {
            Some(l) => (start + l).min(lines.len()),
            None => lines.len(),
        };

        if start >= lines.len() {
            return Ok(String::new());
        }

        Ok(lines[start..end].join("\n"))
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<(), FileError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| map_io_error(e, parent))?;
        }
        tokio::fs::write(path, content)
            .await
            .map_err(|e| map_io_error(e, path))
    }

    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        match tokio::fs::metadata(path).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(map_io_error(e, path)),
        }
    }

    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|e| map_io_error(e, path))?;
        Ok(FileInfo {
            path: path.to_path_buf(),
            is_dir: meta.is_dir(),
            size: meta.len(),
        })
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        let mut entries = tokio::fs::read_dir(path)
            .await
            .map_err(|e| map_io_error(e, path))?;
        let mut result = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let meta = entry
                .metadata()
                .await
                .map_err(|e| map_io_error(e, &entry.path()))?;
            result.push(FileInfo {
                path: entry.path(),
                is_dir: meta.is_dir(),
                size: meta.len(),
            });
        }
        result.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(result)
    }

    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        tokio::fs::create_dir_all(path)
            .await
            .map_err(|e| map_io_error(e, path))
    }

    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|e| map_io_error(e, path))?;
        if meta.is_dir() {
            tokio::fs::remove_dir_all(path)
                .await
                .map_err(|e| map_io_error(e, path))
        } else {
            tokio::fs::remove_file(path)
                .await
                .map_err(|e| map_io_error(e, path))
        }
    }

    async fn exec(
        &self,
        command: &str,
        timeout_dur: Duration,
        signal: CancellationToken,
    ) -> Result<CommandResult, ExecutionError> {
        use tokio::io::AsyncReadExt;

        if signal.is_cancelled() {
            return Err(ExecutionError::Aborted);
        }
        // Own process group: the whole tree the command spawns dies together
        // on timeout or cancellation. `kill_on_drop` alone would not do: it
        // reaches only the direct child, while the tree kill must cover
        // grandchildren too — the armed guard below performs the group kill
        // even when this future is dropped from outside at an await point.
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&self.cwd)
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| ExecutionError::Spawn(format!("{e}")))?;
        let mut kill_guard = TreeKillGuard::arm(child.id());

        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");
        // The pipes fill independently of the wait, so both drain concurrently
        // or a chatty child deadlocks against a full buffer.
        let out_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf).await;
            buf
        });
        let err_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            buf
        });

        let status = tokio::select! {
            status = child.wait() => status.map_err(|e| ExecutionError::Spawn(format!("{e}")))?,
            () = tokio::time::sleep(timeout_dur) => {
                kill_process_tree(&mut child).await;
                let _ = child.wait().await;
                let _ = out_task.await;
                let _ = err_task.await;
                return Err(ExecutionError::Timeout(timeout_dur));
            }
            () = signal.cancelled() => {
                kill_process_tree(&mut child).await;
                let _ = child.wait().await;
                let _ = out_task.await;
                let _ = err_task.await;
                return Err(ExecutionError::Aborted);
            }
        };
        // The command exited on its own; deliberately backgrounded
        // descendants keep their usual shell survival semantics.
        kill_guard.defuse();

        let stdout = out_task
            .await
            .map_err(|e| ExecutionError::Other(format!("{e}")))?;
        let stderr = err_task
            .await
            .map_err(|e| ExecutionError::Other(format!("{e}")))?;
        Ok(CommandResult {
            stdout: String::from_utf8_lossy(&stdout).to_string(),
            stderr: String::from_utf8_lossy(&stderr).to_string(),
            exit_code: status.code().unwrap_or(-1),
        })
    }
}

/// SIGKILL the child's whole process group when dropped while armed.
///
/// The kernel's cancel race drops a cancelled tool's execution future from
/// outside, at any await point — code after the await never runs, so the
/// tree kill must live in a Drop guard. `process_group(0)` makes the child
/// its group leader, so `kill(-pgid)` reaches the whole tree.
struct TreeKillGuard {
    pgid: Option<i32>,
}

impl TreeKillGuard {
    fn arm(pid: Option<u32>) -> Self {
        TreeKillGuard {
            pgid: pid.map(|p| p as i32),
        }
    }

    fn defuse(&mut self) {
        self.pgid = None;
    }
}

impl Drop for TreeKillGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            // Best-effort: the group may already be gone by the time the
            // guard drops.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

/// SIGKILL the child's whole process group, falling back to the child alone
/// when the group is already gone.
async fn kill_process_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // Negative pid signals the process group — the same tree kill the TS
        // harness performs on abort/timeout.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
}

fn map_io_error(e: io::Error, path: &Path) -> FileError {
    match e.kind() {
        io::ErrorKind::NotFound => FileError::NotFound(format!("{}", path.display())),
        io::ErrorKind::PermissionDenied => {
            FileError::PermissionDenied(format!("{}", path.display()))
        }
        io::ErrorKind::IsADirectory => FileError::IsDirectory(format!("{}", path.display())),
        _ => FileError::Io(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_env() -> TokioExecutionEnv {
        TokioExecutionEnv::new(std::env::temp_dir())
    }

    #[tokio::test]
    async fn exec_cancel_kills_the_running_command() {
        let env = test_env();
        let token = CancellationToken::new();
        let signal = token.clone();
        let started = std::time::Instant::now();
        let exec =
            tokio::spawn(
                async move { env.exec("sleep 30", Duration::from_secs(60), signal).await },
            );
        // Cancel shortly after the child starts; without the signal the exec
        // would run for the full 30s.
        tokio::time::sleep(Duration::from_millis(100)).await;
        token.cancel();
        let result = exec.await.unwrap();
        assert!(
            matches!(result, Err(ExecutionError::Aborted)),
            "cancellation surfaces as Aborted: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the sleeping command is killed, not awaited"
        );
    }

    #[tokio::test]
    async fn exec_cancel_kills_the_whole_process_tree() {
        let env = test_env();
        let token = CancellationToken::new();
        let signal = token.clone();
        // The grandchild writes a marker after the shell and its direct child
        // have been killed; a tree kill prevents the write.
        let dir = std::env::temp_dir().join(format!("pi-exec-tree-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("grandchild-survived");
        let cmd = format!("sh -c 'sleep 2; touch {}' & sleep 30", marker.display());
        let exec =
            tokio::spawn(async move { env.exec(&cmd, Duration::from_secs(60), signal).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        token.cancel();
        let result = exec.await.unwrap();
        assert!(matches!(result, Err(ExecutionError::Aborted)));
        // Wait past the grandchild's scheduled write; the tree kill must have
        // taken it down with the rest.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !marker.exists(),
            "the detached grandchild dies with the process group"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// External-drop variant of the tree kill: the kernel's enforcement race
    /// abandons a cancelled tool by dropping its future, so the exec future
    /// can be dropped from outside at any await point — before its internal
    /// signal arm ever runs. The drop guard must still reap the whole tree.
    #[tokio::test]
    async fn exec_future_drop_at_cancel_kills_the_whole_process_tree() {
        let env = test_env();
        let token = CancellationToken::new();
        let signal = token.clone();
        // The grandchild writes a marker after the shell and its direct
        // child should have been killed; a tree kill prevents the write.
        let dir = std::env::temp_dir().join(format!("pi-exec-drop-tree-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("grandchild-survived");
        let cmd = format!("sh -c 'sleep 2; touch {}' & sleep 30", marker.display());
        let exec =
            tokio::spawn(async move { env.exec(&cmd, Duration::from_secs(60), signal).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Cancel and drop the future in the same instant: the task abort
        // drops the exec future from outside, exactly like the enforcement
        // race dropping a cancelled tool's execution.
        token.cancel();
        exec.abort();
        let _ = exec.await;
        // Wait past the grandchild's scheduled write; the tree kill must have
        // taken it down with the rest.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !marker.exists(),
            "the detached grandchild dies even when the exec future is dropped from outside"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn exec_timeout_kills_the_command() {
        let env = test_env();
        let started = std::time::Instant::now();
        let result = env
            .exec(
                "sleep 30",
                Duration::from_millis(100),
                CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(result, Err(ExecutionError::Timeout(_))),
            "a slow command surfaces as Timeout: {result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn exec_collects_stdout_and_stderr() {
        let env = test_env();
        let result = env
            .exec(
                "echo out; echo err >&2",
                Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout.trim(), "out");
        assert_eq!(result.stderr.trim(), "err");
    }
}
