//! Async tool world → synchronous rustwright facade bridge.
//!
//! The engine core owns its own tokio runtime and exposes blocking calls, so
//! every engine operation moves onto a blocking thread; tools never call the
//! facade from an async context. The pi cancellation token is mirrored onto
//! the engine's `CancelToken`, so a cancelled turn interrupts the in-flight
//! CDP wait instead of letting it run to the operation timeout.

use manox_harness::tool::ToolError;
use rustwright_core::CancelToken;
use tokio_util::sync::CancellationToken;

/// Run a blocking engine operation off the async runtime.
///
/// The closure receives the engine cancel token; cancelling `signal` flips it
/// from a poll task. Engine errors surface as `ExecutionFailed` text for the
/// model.
pub async fn run<F, R>(signal: CancellationToken, f: F) -> Result<R, ToolError>
where
    F: FnOnce(Option<&CancelToken>) -> Result<R, String> + Send + 'static,
    R: Send + 'static,
{
    let token = CancelToken::new();
    let mirror = token.clone();
    let poller = tokio::spawn(async move {
        signal.cancelled().await;
        mirror.cancel();
    });
    let joined = tokio::task::spawn_blocking(move || f(Some(&token))).await;
    poller.abort();
    match joined {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(message)) => Err(ToolError::ExecutionFailed(message)),
        Err(join) => Err(ToolError::ExecutionFailed(format!(
            "chrome task aborted: {join}"
        ))),
    }
}
