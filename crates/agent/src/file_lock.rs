//! Process-global file write lock registry (ported from the retired manox
//! harness) + the pi-path enforcement wrapper.
//!
//! NOWAIT try-lock: concurrent writes to the same path are rejected with an
//! error so agents coordinate disjoint write ranges instead of silently
//! clobbering one another. The lock is the enforced backstop behind the
//! system-prompt convention "assign disjoint write ranges"; contention is
//! expected to be near-zero, and a conflict is a signal to re-coordinate,
//! not a silent stall.
//!
//! Reads are not locked — a torn read is recovered by the hashline stale-TAG
//! re-read path, and adding a shared read lock would entangle with the
//! NOWAIT write semantics for no real benefit. `bash` writes are also out of
//! scope: a shell command's touched paths are not statically knowable, so
//! bash-heavy work is coordinated by assigning it to disjoint directories.
//!
//! Pi wiring: the kernel's Write/Edit tools are wrapped in
//! [`FileLockedTool`], which acquires the path's lock for the duration of
//! the actual execution window (approval round trips happen outside the
//! lock) and rejects with the holder's name on conflict. The retired manox
//! harness acquired inside its write/edit tools; the wrapper keeps the same
//! scope without touching kernel tools.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use tokio_util::sync::CancellationToken;

/// Who currently holds the exclusive write lock on a path.
#[derive(Clone, Debug)]
pub struct HeldBy {
    /// The owning agent's label (member name / subagent_type / "lead").
    pub owner: String,
    pub acquired_at: Instant,
}

struct Registry {
    entries: Mutex<HashMap<PathBuf, HeldBy>>,
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| Registry {
        entries: Mutex::new(HashMap::new()),
    })
}

/// Normalize a resolved path to a stable lock key. Canonicalizes when the
/// file (or its parent) exists so two writers that spell the same target
/// differently still collide; falls back to the resolved absolute path for
/// not-yet-created files, matching the hashline snapshot key stance.
fn key(path: &Path) -> PathBuf {
    if let Ok(canon) = path.canonicalize() {
        return canon;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(canon_parent) = parent.canonicalize()
    {
        return canon_parent.join(name);
    }
    path.to_path_buf()
}

/// Try to acquire an exclusive write lock on `path` for `owner`. On success
/// returns a guard that releases on drop; on conflict returns the current
/// holder so the caller can name it in the error.
pub fn try_acquire(path: &Path, owner: &str) -> Result<FileWriteGuard, HeldBy> {
    let key = key(path);
    let mut entries = registry()
        .entries
        .lock()
        .expect("file write lock registry poisoned");
    if let Some(held) = entries.get(&key) {
        return Err(held.clone());
    }
    entries.insert(
        key.clone(),
        HeldBy {
            owner: owner.to_string(),
            acquired_at: Instant::now(),
        },
    );
    Ok(FileWriteGuard { key: Some(key) })
}

/// RAII guard that releases the held write lock on drop.
#[derive(Debug)]
pub struct FileWriteGuard {
    key: Option<PathBuf>,
}

impl FileWriteGuard {
    /// Release the lock early. The guard's `Drop` is a no-op after this.
    pub fn release(mut self) {
        if let Some(key) = self.key.take() {
            let mut entries = registry()
                .entries
                .lock()
                .expect("file write lock registry poisoned");
            entries.remove(&key);
        }
    }
}

impl Drop for FileWriteGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            let mut entries = registry()
                .entries
                .lock()
                .expect("file write lock registry poisoned");
            entries.remove(&key);
        }
    }
}

/// Wrapper that guards a path-mutating tool (Write/Edit) with the process
/// write lock for the duration of its execution. Delegates every trait
/// surface to the inner tool; only `execute` interposes the lock.
///
/// Calls without a string `path` argument pass through unlocked (the
/// wrapped tools always carry one; this keeps the wrapper total).
pub struct FileLockedTool {
    inner: Arc<dyn AgentTool>,
    owner: String,
}

impl FileLockedTool {
    pub fn new(inner: Arc<dyn AgentTool>, owner: impl Into<String>) -> Self {
        Self {
            inner,
            owner: owner.into(),
        }
    }

    fn resolve_path(&self, params: &serde_json::Value, ctx: &dyn ToolContext) -> Option<PathBuf> {
        let raw = params.get("path")?.as_str()?;
        let p = Path::new(raw);
        Some(if p.is_absolute() {
            p.to_path_buf()
        } else {
            ctx.cwd().join(p)
        })
    }
}

#[async_trait::async_trait]
impl AgentTool for FileLockedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }
    fn requires_approval(&self, params: &serde_json::Value) -> bool {
        self.inner.requires_approval(params)
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let Some(path) = self.resolve_path(&params, ctx) else {
            return self.inner.execute(tool_call_id, params, signal, ctx).await;
        };
        let guard = match try_acquire(&path, &self.owner) {
            Ok(guard) => guard,
            Err(held) => {
                return Err(ToolError::ExecutionFailed(format!(
                    "`{}` is write-locked by {} (held for {}s). Coordinate disjoint \
                     write ranges, or wait for the holder to finish, then retry.",
                    path.display(),
                    held.owner,
                    held.acquired_at.elapsed().as_secs()
                )));
            }
        };
        let result = self.inner.execute(tool_call_id, params, signal, ctx).await;
        guard.release();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::env::TokioExecutionEnv;
    use pi::tool::{LocalToolContext, ToolState};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn first_acquire_wins_second_gets_holder() {
        let a = Path::new("/tmp/manox-file-lock-acquire");
        let g = try_acquire(a, "lead").expect("first acquire");
        let err = try_acquire(a, "plan").unwrap_err();
        assert_eq!(err.owner, "lead");
        drop(g);
    }

    #[test]
    fn drop_releases() {
        let p = Path::new("/tmp/manox-file-lock-drop");
        {
            let _g = try_acquire(p, "lead").expect("acquire");
            assert!(try_acquire(p, "plan").is_err());
        }
        assert!(try_acquire(p, "plan").is_ok(), "released after drop");
    }

    #[test]
    fn release_is_idempotent_with_drop() {
        let p = Path::new("/tmp/manox-file-lock-release");
        let g = try_acquire(p, "lead").expect("acquire");
        g.release();
        assert!(try_acquire(p, "plan").is_ok(), "released early");
    }

    #[test]
    fn distinct_paths_do_not_collide() {
        let a = Path::new("/tmp/manox-file-lock-distinct-a");
        let b = Path::new("/tmp/manox-file-lock-distinct-b");
        let _ga = try_acquire(a, "lead").expect("a");
        let gb = try_acquire(b, "plan").expect("b should not collide with a");
        gb.release();
    }

    // ── wrapper ─────────────────────────────────────────────────────────────

    struct StubTool {
        ran: Arc<AtomicUsize>,
        /// While `true` the stub holds the path's write lock for the duration
        /// of its execution, simulating a slow concurrent writer.
        hold_lock_during_run: bool,
        owner: String,
    }

    #[async_trait::async_trait]
    impl AgentTool for StubTool {
        fn name(&self) -> &str {
            "Write"
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            params: serde_json::Value,
            _signal: CancellationToken,
            ctx: &dyn ToolContext,
        ) -> Result<AgentToolResult, ToolError> {
            let path = Path::new(params["path"].as_str().unwrap());
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                ctx.cwd().join(path)
            };
            let guard = self.hold_lock_during_run.then(|| {
                // Simulate a second writer that grabbed the lock first.
                try_acquire(&path, &self.owner).expect("stub pre-acquire")
            });
            self.ran.fetch_add(1, Ordering::SeqCst);
            drop(guard);
            Ok(AgentToolResult::text("ok"))
        }
    }

    fn tool_ctx() -> LocalToolContext {
        LocalToolContext::new(
            Arc::new(TokioExecutionEnv::new(std::env::temp_dir())),
            std::env::temp_dir(),
            Arc::new(ToolState::new()),
        )
    }

    #[tokio::test]
    async fn wrapper_conflict_names_holder_and_skips_inner() {
        let path = std::env::temp_dir().join("manox-file-lock-wrapper-conflict");
        let ran = Arc::new(AtomicUsize::new(0));
        let tool = FileLockedTool::new(
            Arc::new(StubTool {
                ran: Arc::clone(&ran),
                hold_lock_during_run: false,
                owner: "inner".into(),
            }),
            "main",
        );
        // A rival holds the lock before the call starts.
        let rival = try_acquire(&path, "rival-agent").expect("rival acquire");
        let err = tool
            .execute(
                "c1",
                serde_json::json!({ "path": path.to_str().unwrap() }),
                CancellationToken::new(),
                &tool_ctx(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("rival-agent"), "names the holder: {msg}");
        assert_eq!(ran.load(Ordering::SeqCst), 0, "inner never ran");
        rival.release();
        // Released: the same call now succeeds.
        let ok = tool
            .execute(
                "c2",
                serde_json::json!({ "path": path.to_str().unwrap() }),
                CancellationToken::new(),
                &tool_ctx(),
            )
            .await
            .unwrap();
        let _ = ok;
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn wrapper_releases_after_execution() {
        let path = std::env::temp_dir().join("manox-file-lock-wrapper-release");
        let ran = Arc::new(AtomicUsize::new(0));
        let tool = FileLockedTool::new(
            Arc::new(StubTool {
                ran: Arc::clone(&ran),
                hold_lock_during_run: false,
                owner: "inner".into(),
            }),
            "main",
        );
        for id in ["r1", "r2"] {
            tool.execute(
                id,
                serde_json::json!({ "path": path.to_str().unwrap() }),
                CancellationToken::new(),
                &tool_ctx(),
            )
            .await
            .expect("sequential calls both succeed");
        }
        assert_eq!(ran.load(Ordering::SeqCst), 2);
        // The lock is free after the wrapper finishes.
        let g = try_acquire(&path, "probe").expect("free after execution");
        g.release();
    }
}
