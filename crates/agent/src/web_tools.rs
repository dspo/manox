//! `WebExplore*` tool set — the agent's outbound surface for driving the
//! built-in browser (ported from the retired manox harness).
//!
//! The browser host (`agent::webview_host::BrowserHost`) is a gpui
//! main-thread surface; pi tools run on tokio. Each call therefore rides the
//! same round-trip architecture as the permission gate: the tool sends a
//! `BackendNotice::BrowserRequest` with a responder channel, the facade
//! (gpui drainer) executes the op against the host and replies through the
//! channel, and the tool awaits the reply on its tokio thread.
//!
//! Read tools (`ReadText` / `ReadDom` / `Screenshot`) are ungated and
//! `is_read_only` so plan mode exposes them. Write tools declare
//! `requires_approval` and ride the owning thread's `PermissionMode` (the
//! engine wraps them in `ApprovalGatedTool`), so the outbound trust axis is
//! governed by the same mode that gates `Bash` / `Write`.

use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::thread_engine::{BackendNotice, BrowserOp, BrowserReply};
use crate::webview_host::BrowserTabId;

/// Send `op` to the facade and await the host's reply. Clean errors for the
/// two "nobody is listening" cases (no host registered / engine gone) and
/// `Aborted` on cancel, so the model gets an actionable message instead of a
/// hang.
async fn host_round_trip(
    notice_tx: &mpsc::UnboundedSender<BackendNotice>,
    op: BrowserOp,
    signal: &CancellationToken,
) -> Result<BrowserReply, ToolError> {
    // An already-cancelled run settles without touching the host.
    if signal.is_cancelled() {
        return Err(ToolError::Aborted);
    }
    if crate::webview_host::host().is_none() {
        return Err(ToolError::ExecutionFailed(
            "browser host not available (non-UI context)".into(),
        ));
    }
    let (tx, rx) = async_channel::bounded(1);
    notice_tx
        .send(BackendNotice::BrowserRequest { op, responder: tx })
        .map_err(|_| ToolError::ExecutionFailed("engine actor gone".into()))?;
    // The reply rides the gpui main thread and may never come (e.g. a yield
    // the user answers with cancel instead of handback): race the cancel
    // token so a user abort always settles the round trip.
    tokio::select! {
        reply = rx.recv() => reply
            .map_err(|_| ToolError::ExecutionFailed("browser request dropped".into()))?
            .map_err(ToolError::ExecutionFailed),
        () = signal.cancelled() => Err(ToolError::Aborted),
    }
}

fn schema<T: JsonSchema>() -> serde_json::Value {
    let mut value = serde_json::to_value(schemars::schema_for!(T)).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("$schema");
        obj.remove("$defs");
    }
    value
}

fn text_reply(reply: BrowserReply) -> Result<AgentToolResult, ToolError> {
    match reply {
        BrowserReply::Text(text) => Ok(AgentToolResult::text(text)),
        other => Err(ToolError::ExecutionFailed(format!(
            "unexpected browser reply: {other:?}"
        ))),
    }
}

fn unit_reply(reply: BrowserReply, confirmation: String) -> Result<AgentToolResult, ToolError> {
    match reply {
        BrowserReply::Unit => Ok(AgentToolResult::text(confirmation)),
        other => Err(ToolError::ExecutionFailed(format!(
            "unexpected browser reply: {other:?}"
        ))),
    }
}

// ─── inputs ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct OpenInput {
    /// Absolute URL to navigate the new tab to (`https://` recommended).
    url: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NavigateInput {
    /// The browser tab id returned by `WebExploreOpen`.
    tab_id: BrowserTabId,
    /// Absolute URL to navigate the tab to.
    url: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TabIdInput {
    /// The browser tab id returned by `WebExploreOpen`.
    tab_id: BrowserTabId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadDomInput {
    /// The browser tab id returned by `WebExploreOpen`.
    tab_id: BrowserTabId,
    /// CSS selector; the `outerHTML` of the first matching element is
    /// returned. Omit to read the whole document's `outerHTML`.
    #[serde(default)]
    selector: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ClickInput {
    /// The browser tab id returned by `WebExploreOpen`.
    tab_id: BrowserTabId,
    /// CSS selector of the element to click (first match).
    selector: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TypeInput {
    /// The browser tab id returned by `WebExploreOpen`.
    tab_id: BrowserTabId,
    /// CSS selector of the element to type into (first match).
    selector: String,
    /// Text to type into the focused element (sets value and dispatches
    /// input/change events).
    text: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScrollInput {
    /// The browser tab id returned by `WebExploreOpen`.
    tab_id: BrowserTabId,
    /// Horizontal scroll delta in device pixels (positive = right).
    dx: i32,
    /// Vertical scroll delta in device pixels (positive = down).
    dy: i32,
}

// ─── tools ──────────────────────────────────────────────────────────────────

/// Open a new browser tab; returns the tab id to the model.
pub struct WebExploreOpenTool {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
}
/// Navigate an existing tab to a new url.
pub struct WebExploreNavigateTool {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
}
/// Read the page's main text (readability-extracted).
pub struct WebExploreReadTextTool {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
}
/// Read outerHTML of the first match for a selector (or the whole document).
pub struct WebExploreReadDomTool {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
}
/// Click the first element matching a selector.
pub struct WebExploreClickTool {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
}
/// Focus the first element matching a selector and type text into it.
pub struct WebExploreTypeTool {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
}
/// Scroll the page by (dx, dy) device pixels.
pub struct WebExploreScrollTool {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
}
/// DOM snapshot of the visible viewport (structure + metadata).
pub struct WebExploreScreenshotTool {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
}
/// Yield the tab to the user until handback (login / captcha).
pub struct WebExploreYieldTool {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
}
/// Close and reclaim a tab.
pub struct WebExploreCloseTool {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
}

macro_rules! web_tool_impl {
    ($tool:ident) => {
        impl $tool {
            pub fn new(notice_tx: mpsc::UnboundedSender<BackendNotice>) -> Self {
                Self { notice_tx }
            }
        }
    };
}

web_tool_impl!(WebExploreOpenTool);
web_tool_impl!(WebExploreNavigateTool);
web_tool_impl!(WebExploreReadTextTool);
web_tool_impl!(WebExploreReadDomTool);
web_tool_impl!(WebExploreClickTool);
web_tool_impl!(WebExploreTypeTool);
web_tool_impl!(WebExploreScrollTool);
web_tool_impl!(WebExploreScreenshotTool);
web_tool_impl!(WebExploreYieldTool);
web_tool_impl!(WebExploreCloseTool);

#[async_trait::async_trait]
impl AgentTool for WebExploreOpenTool {
    fn name(&self) -> &str {
        "WebExploreOpen"
    }
    fn description(&self) -> &str {
        "Open a new browser tab in the manox sidebar navigated to `url` and return its \
         numeric `tab_id` (as JSON `{\"tab_id\": N}`). Pass that id to the other \
         WebExplore* tools to drive the tab. Gated by the thread's permission mode."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<OpenInput>()
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
        let input: OpenInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let reply = host_round_trip(
            &self.notice_tx,
            BrowserOp::Open {
                url: input.url.clone(),
            },
            &signal,
        )
        .await?;
        match reply {
            BrowserReply::TabId(id) => Ok(AgentToolResult::text(format!("{{\"tab_id\":{id}}}"))),
            other => Err(ToolError::ExecutionFailed(format!(
                "unexpected browser reply: {other:?}"
            ))),
        }
    }
}

#[async_trait::async_trait]
impl AgentTool for WebExploreNavigateTool {
    fn name(&self) -> &str {
        "WebExploreNavigate"
    }
    fn description(&self) -> &str {
        "Navigate an existing browser tab (identified by `tab_id`) to a new `url`. \
         Gated by the thread's permission mode."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<NavigateInput>()
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
        let input: NavigateInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let reply = host_round_trip(
            &self.notice_tx,
            BrowserOp::Navigate {
                id: input.tab_id,
                url: input.url.clone(),
            },
            &signal,
        )
        .await?;
        unit_reply(
            reply,
            format!("Navigated tab {} to {}", input.tab_id, input.url),
        )
    }
}

#[async_trait::async_trait]
impl AgentTool for WebExploreReadTextTool {
    fn name(&self) -> &str {
        "WebExploreReadText"
    }
    fn description(&self) -> &str {
        "Read the main text content of the tab's current page (readability-extracted) as \
         plain text. Read-only — no approval needed."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TabIdInput>()
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
        let input: TabIdInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let reply = host_round_trip(
            &self.notice_tx,
            BrowserOp::ReadText { id: input.tab_id },
            &signal,
        )
        .await?;
        text_reply(reply)
    }
}

#[async_trait::async_trait]
impl AgentTool for WebExploreReadDomTool {
    fn name(&self) -> &str {
        "WebExploreReadDom"
    }
    fn description(&self) -> &str {
        "Read the `outerHTML` of the first element matching `selector`, or the whole \
         document's `outerHTML` when `selector` is omitted. Read-only — no approval \
         needed."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<ReadDomInput>()
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
        let input: ReadDomInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let reply = host_round_trip(
            &self.notice_tx,
            BrowserOp::ReadDom {
                id: input.tab_id,
                selector: input.selector.clone(),
            },
            &signal,
        )
        .await?;
        text_reply(reply)
    }
}

#[async_trait::async_trait]
impl AgentTool for WebExploreClickTool {
    fn name(&self) -> &str {
        "WebExploreClick"
    }
    fn description(&self) -> &str {
        "Click the first element matching `selector` in the tab. Gated by the \
         thread's permission mode."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<ClickInput>()
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
        let input: ClickInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let reply = host_round_trip(
            &self.notice_tx,
            BrowserOp::Click {
                id: input.tab_id,
                selector: input.selector.clone(),
            },
            &signal,
        )
        .await?;
        unit_reply(
            reply,
            format!("Clicked `{}` in tab {}", input.selector, input.tab_id),
        )
    }
}

#[async_trait::async_trait]
impl AgentTool for WebExploreTypeTool {
    fn name(&self) -> &str {
        "WebExploreType"
    }
    fn description(&self) -> &str {
        "Focus the first element matching `selector` and type `text` into it (sets the \
         value and dispatches input/change events). Gated by the thread's \
         permission mode."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TypeInput>()
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
        let input: TypeInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let reply = host_round_trip(
            &self.notice_tx,
            BrowserOp::TypeText {
                id: input.tab_id,
                selector: input.selector.clone(),
                text: input.text.clone(),
            },
            &signal,
        )
        .await?;
        unit_reply(
            reply,
            format!("Typed into `{}` in tab {}", input.selector, input.tab_id),
        )
    }
}

#[async_trait::async_trait]
impl AgentTool for WebExploreScrollTool {
    fn name(&self) -> &str {
        "WebExploreScroll"
    }
    fn description(&self) -> &str {
        "Scroll the tab's page by (dx, dy) device pixels (positive = right/down, \
         negative = left/up). Gated by the thread's permission mode."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<ScrollInput>()
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
        let input: ScrollInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let reply = host_round_trip(
            &self.notice_tx,
            BrowserOp::Scroll {
                id: input.tab_id,
                dx: input.dx,
                dy: input.dy,
            },
            &signal,
        )
        .await?;
        unit_reply(
            reply,
            format!(
                "Scrolled tab {} by ({}, {})",
                input.tab_id, input.dx, input.dy
            ),
        )
    }
}

#[async_trait::async_trait]
impl AgentTool for WebExploreScreenshotTool {
    fn name(&self) -> &str {
        "WebExploreScreenshot"
    }
    fn description(&self) -> &str {
        "Return a DOM snapshot of the tab's visible viewport (structure + scroll/viewport \
         metadata, not a pixel image). Read-only — no approval needed."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TabIdInput>()
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
        let input: TabIdInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let reply = host_round_trip(
            &self.notice_tx,
            BrowserOp::Screenshot { id: input.tab_id },
            &signal,
        )
        .await?;
        text_reply(reply)
    }
}

#[async_trait::async_trait]
impl AgentTool for WebExploreYieldTool {
    fn name(&self) -> &str {
        "WebExploreYield"
    }
    fn description(&self) -> &str {
        "Yield control of the tab to the user (e.g. for a login / captcha handshake). \
         Blocks until the user triggers the handback in the browser chrome, then \
         returns. Use this when the page needs human interaction before you can read \
         the authenticated result. Gated by the thread's permission mode."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TabIdInput>()
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
        let input: TabIdInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let reply = host_round_trip(
            &self.notice_tx,
            BrowserOp::YieldToUser { id: input.tab_id },
            &signal,
        )
        .await
        .map_err(|e| match e {
            // Model-facing, never localized: the model must learn that the
            // yield ended by cancel, not by handback.
            ToolError::Aborted => ToolError::ExecutionFailed("yield to user was cancelled".into()),
            other => other,
        })?;
        unit_reply(
            reply,
            format!("User handed tab {} back; control returned", input.tab_id),
        )
    }
}

#[async_trait::async_trait]
impl AgentTool for WebExploreCloseTool {
    fn name(&self) -> &str {
        "WebExploreClose"
    }
    fn description(&self) -> &str {
        "Close and reclaim the browser tab identified by `tab_id`. Gated by the \
         thread's permission mode."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TabIdInput>()
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
        let input: TabIdInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let reply = host_round_trip(
            &self.notice_tx,
            BrowserOp::Close { id: input.tab_id },
            &signal,
        )
        .await?;
        unit_reply(reply, format!("Closed tab {}", input.tab_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::env::TokioExecutionEnv;
    use pi::tool::{LocalToolContext, ToolState};

    fn tool_ctx() -> LocalToolContext {
        LocalToolContext::new(
            std::sync::Arc::new(TokioExecutionEnv::new(std::env::temp_dir())),
            std::env::temp_dir(),
            std::sync::Arc::new(ToolState::new()),
        )
    }

    /// The tool posts a BrowserRequest and resolves from the responder —
    /// the test plays the facade side of the round trip.
    #[tokio::test]
    async fn open_round_trips_tab_id() {
        let (notice_tx, notice_rx) = mpsc::unbounded_channel();
        // No host registered in tests → the tool must fail clean, not hang.
        let tool = WebExploreOpenTool::new(notice_tx.clone());
        let err = tool
            .execute(
                "c1",
                serde_json::json!({"url": "https://example.com"}),
                CancellationToken::new(),
                &tool_ctx(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("browser host"), "{err}");
        drop(notice_rx);
    }

    #[tokio::test]
    async fn read_text_parses_input_and_rejects_bad_json() {
        let (notice_tx, _notice_rx) = mpsc::unbounded_channel();
        let tool = WebExploreReadTextTool::new(notice_tx);
        let err = tool
            .execute(
                "c1",
                serde_json::json!({"wrong_field": 1}),
                CancellationToken::new(),
                &tool_ctx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[test]
    fn approval_flags_match_trust_axes() {
        let (notice_tx, _notice_rx) = mpsc::unbounded_channel();
        // Read axis: approval-free + read-only (plan mode exposes them).
        assert!(
            !WebExploreReadTextTool::new(notice_tx.clone())
                .requires_approval(&serde_json::json!({}))
        );
        assert!(WebExploreReadTextTool::new(notice_tx.clone()).is_read_only());
        assert!(
            !WebExploreReadDomTool::new(notice_tx.clone())
                .requires_approval(&serde_json::json!({}))
        );
        assert!(WebExploreReadDomTool::new(notice_tx.clone()).is_read_only());
        assert!(
            !WebExploreScreenshotTool::new(notice_tx.clone())
                .requires_approval(&serde_json::json!({}))
        );
        // Write axis: approval-gated, not read-only.
        assert!(
            WebExploreOpenTool::new(notice_tx.clone()).requires_approval(&serde_json::json!({}))
        );
        assert!(
            WebExploreClickTool::new(notice_tx.clone()).requires_approval(&serde_json::json!({}))
        );
        assert!(
            WebExploreTypeTool::new(notice_tx.clone()).requires_approval(&serde_json::json!({}))
        );
        assert!(
            WebExploreScrollTool::new(notice_tx.clone()).requires_approval(&serde_json::json!({}))
        );
        assert!(
            WebExploreNavigateTool::new(notice_tx.clone())
                .requires_approval(&serde_json::json!({}))
        );
        assert!(
            WebExploreYieldTool::new(notice_tx.clone()).requires_approval(&serde_json::json!({}))
        );
        assert!(WebExploreCloseTool::new(notice_tx).requires_approval(&serde_json::json!({})));
    }

    /// A cancelled run settles the round trip as `Aborted` before any reply
    /// can arrive.
    #[tokio::test]
    async fn host_round_trip_aborts_when_cancelled_before_any_reply() {
        let (notice_tx, _notice_rx) = mpsc::unbounded_channel();
        let signal = CancellationToken::new();
        signal.cancel();
        let err = host_round_trip(&notice_tx, BrowserOp::ReadText { id: 1 }, &signal)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Aborted), "{err}");
    }
}
