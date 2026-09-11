//! Host-capability tools — the agent's surface for the frontend-provided
//! clipboard and opener capabilities (`manox_agent::capability`).
//!
//! Each tool rides the same round-trip architecture as the permission gate
//! and the web tools: the tool (tokio) sends a `BackendNotice` with a
//! responder channel, the facade executes the capability against the
//! registered provider (scoping the session task-local so the
//! AgentServer's provider can route the `ServerCall` to the owning client)
//! and replies through the channel. The tools themselves are registered
//! only when a capability provider is present — a headless context with no
//! host surface exposes neither tool rather than failing at call time.

use std::sync::Arc;

use manox_harness::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::thread_engine::BackendNotice;

fn schema<T: JsonSchema>() -> serde_json::Value {
    let mut value = serde_json::to_value(schemars::schema_for!(T)).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("$schema");
        obj.remove("$defs");
    }
    value
}

/// Send a notice and await the facade's reply. Clean errors for the two
/// "nobody is listening" cases (engine gone / user abort), mirroring the web
/// tools' round-trip helper.
async fn notice_round_trip<T>(
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    make_notice: impl FnOnce(async_channel::Sender<Result<T, String>>) -> BackendNotice,
    signal: &CancellationToken,
) -> Result<T, ToolError> {
    if signal.is_cancelled() {
        return Err(ToolError::Aborted);
    }
    let (tx, rx) = async_channel::bounded(1);
    notice_tx
        .send(make_notice(tx))
        .map_err(|_| ToolError::ExecutionFailed("engine actor gone".into()))?;
    tokio::select! {
        reply = rx.recv() => reply
            .map_err(|_| ToolError::ExecutionFailed("capability request dropped".into()))?
            .map_err(ToolError::ExecutionFailed),
        () = signal.cancelled() => Err(ToolError::Aborted),
    }
}

/// `ClipboardRead`: read the host's system clipboard as plain text.
pub struct ClipboardReadTool {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
}

#[derive(Deserialize, JsonSchema)]
struct ClipboardReadInput {}

#[async_trait::async_trait]
impl AgentTool for ClipboardReadTool {
    fn name(&self) -> &str {
        "ClipboardRead"
    }
    fn description(&self) -> &str {
        "Read the host's system clipboard as plain text. Returns empty output \
         when the clipboard is empty or holds non-text content. Read-only: \
         never mutates anything."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<ClipboardReadInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let _: ClipboardReadInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let text = notice_round_trip(
            &self.notice_tx,
            |responder| BackendNotice::ClipboardRequest { responder },
            &signal,
        )
        .await?;
        Ok(AgentToolResult::text(text.unwrap_or_default()))
    }
}

/// `Open`: ask the host to open a URL / file path in the OS default handler.
/// Approval-gated — it surfaces something on the user's machine.
pub struct OpenExternalTool {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
}

#[derive(Deserialize, JsonSchema)]
struct OpenExternalInput {
    /// URL or absolute file path to open in the OS default handler.
    url: String,
}

#[async_trait::async_trait]
impl AgentTool for OpenExternalTool {
    fn name(&self) -> &str {
        "Open"
    }
    fn description(&self) -> &str {
        "Open a URL or file path in the host's default handler (browser / \
         file opener). Gated by the thread's permission mode."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<OpenExternalInput>()
    }
    fn requires_approval(&self, _params: &serde_json::Value) -> bool {
        true
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: OpenExternalInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        notice_round_trip(
            &self.notice_tx,
            |responder| BackendNotice::OpenExternalRequest {
                url: input.url.clone(),
                responder,
            },
            &signal,
        )
        .await?;
        Ok(AgentToolResult::text("opened"))
    }
}

/// Register the host-capability tools when (and only when) a capability
/// provider is present. Kept as one helper so the gate itself is unit-
/// testable against the process-wide provider, independent of the engine's
/// tool-assembly fixture. The clipboard read lands in `tools` ungated; the
/// opener is returned for the caller to wrap in the approval gate.
pub fn append(
    tools: &mut Vec<Arc<dyn AgentTool>>,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
) -> Option<Arc<dyn AgentTool>> {
    crate::capability::provider()?;
    tools.push(Arc::new(ClipboardReadTool {
        notice_tx: notice_tx.clone(),
    }));
    Some(Arc::new(OpenExternalTool { notice_tx }))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopProvider;

    #[async_trait::async_trait]
    impl crate::capability::CapabilityClient for NoopProvider {
        fn browser_op(
            &self,
            _op: crate::thread_engine::BrowserOp,
        ) -> futures::future::BoxFuture<'static, Result<crate::thread_engine::BrowserReply, String>>
        {
            Box::pin(async { Err("noop".to_string()) })
        }
        fn clipboard_read(
            &self,
        ) -> futures::future::BoxFuture<'static, Result<Option<String>, String>> {
            Box::pin(async { Ok(Some("clip".to_string())) })
        }
        fn open_external(
            &self,
            _url: String,
        ) -> futures::future::BoxFuture<'static, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn notice_tx() -> mpsc::UnboundedSender<BackendNotice> {
        let (tx, _rx) = mpsc::unbounded_channel();
        tx
    }

    // Registration gate, both halves in ONE test: the provider is a
    // process-global, so the absent/present halves must not race each
    // other across parallel test threads.
    #[test]
    fn tools_gate_on_provider_presence() {
        crate::capability::drop_provider_for_test();
        let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
        let opener = append(&mut tools, notice_tx());
        assert!(tools.is_empty(), "no provider → no host-capability tools");
        assert!(opener.is_none());

        crate::capability::set_provider(Arc::new(NoopProvider));
        let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
        let opener = append(&mut tools, notice_tx());
        assert_eq!(tools.len(), 1, "the clipboard tool lands ungated");
        assert!(opener.is_some(), "the opener is returned for gating");
        crate::capability::drop_provider_for_test();
    }

    // The round trip fails closed when the facade is gone (engine channel
    // dropped): the model sees an actionable error, never a hang.
    #[test]
    fn clipboard_tool_fails_closed_without_facade() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx); // the facade is gone
        let tool = ClipboardReadTool { notice_tx: tx };
        let err = rt
            .block_on(async {
                tool.execute(
                    "tc-1",
                    serde_json::json!({}),
                    CancellationToken::new(),
                    &NullCtx,
                )
                .await
            })
            .unwrap_err();
        assert!(
            matches!(err, ToolError::ExecutionFailed(ref m) if m.contains("engine actor gone")),
            "got {err:?}"
        );
    }

    /// A minimal `ToolContext` for driving capability tools in-process —
    /// they never touch the env or the tool state.
    struct NullCtx;
    impl ToolContext for NullCtx {
        fn env(&self) -> &dyn manox_harness::env::ExecutionEnv {
            unreachable!("capability tools never touch the env")
        }
        fn cwd(&self) -> &std::path::Path {
            std::path::Path::new("/")
        }
        fn tool_state(&self) -> &manox_harness::tool::ToolState {
            unreachable!("capability tools never touch the tool state")
        }
    }
}
