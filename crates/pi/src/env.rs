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