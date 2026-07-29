// Platform abstraction for the harness — filesystem and shell operations.
//
// The harness never touches the real filesystem or spawns processes directly.
// It calls through this trait, which keeps the core loop runtime-agnostic and
// testable via mocks.

use std::path::{Path, PathBuf};
use std::time::Duration;

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

    /// Execute a shell command with a timeout.
    async fn exec(
        &self,
        command: &str,
        timeout: Duration,
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
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| map_io_error(e, parent))?;
            }
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
    ) -> Result<CommandResult, ExecutionError> {
        let result = tokio::time::timeout(timeout_dur, async {
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&self.cwd)
                .output()
                .await
                .map_err(|e| ExecutionError::Spawn(format!("{e}")))?;

            Ok::<_, ExecutionError>(CommandResult {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                exit_code: output.status.code().unwrap_or(-1),
            })
        })
        .await
        .map_err(|_| ExecutionError::Timeout(timeout_dur))?;

        result
    }
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