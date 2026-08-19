//! ChromeUse tool set — the agent's outbound surface for driving a real
//! Chrome through the in-process rustwright CDP engine.
//!
//! Interaction model: snapshot + element refs. Every write action replies
//! with a fresh snapshot so the loop continues without an extra round trip;
//! refs are valid only for the snapshot that issued them. Reads (`Snapshot`
//! / `WaitFor` / `Screenshot`) are approval-free and `is_read_only`; writes
//! declare `requires_approval` and ride the owning thread's `ApprovalMode`
//! (the engine wraps them in `ApprovalGatedTool`), the same trust axes as
//! the built-in browser tools. Chrome's network egress bypasses the bash
//! sandbox proxy — treat it as an unsandboxed outbound surface.

use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use pi::types::ContentBlock;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::runtime::{self, ChromeTabId, WaitTarget};
use super::snapshot;

/// Screenshots above this size fall back to a temp-file path instead of an
/// inline image block.
const MAX_INLINE_SCREENSHOT_BYTES: usize = 4 * 1024 * 1024;

fn schema<T: JsonSchema>() -> serde_json::Value {
    let mut value = serde_json::to_value(schemars::schema_for!(T)).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("$schema");
        obj.remove("$defs");
    }
    value
}

fn parse<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Result<T, ToolError> {
    serde_json::from_value(params).map_err(|e| ToolError::InvalidArguments(e.to_string()))
}

// ─── inputs ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct OpenInput {
    /// URL for the first tab; omit to open a blank tab.
    #[serde(default)]
    url: Option<String>,
    /// Attach to an already-running Chrome over its DevTools WebSocket
    /// endpoint (`ws://127.0.0.1:9222/...`) instead of launching; keeps the
    /// user's existing logins and tabs. Overrides `[chrome].cdp_endpoint`.
    #[serde(default)]
    cdp_endpoint: Option<String>,
    /// Headless override; only applied when the session starts.
    #[serde(default)]
    headless: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TabInput {
    /// Tab id returned by `ChromeUseOpen` or `ChromeUseTabs`.
    tab_id: ChromeTabId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NavigateInput {
    /// Tab id returned by `ChromeUseOpen` or `ChromeUseTabs`.
    tab_id: ChromeTabId,
    /// Absolute URL to navigate the tab to.
    url: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RefInput {
    /// Tab id returned by `ChromeUseOpen` or `ChromeUseTabs`.
    tab_id: ChromeTabId,
    /// Element ref `[eN]` from the tab's latest snapshot.
    #[serde(rename = "ref")]
    ref_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TypeInput {
    /// Tab id returned by `ChromeUseOpen` or `ChromeUseTabs`.
    tab_id: ChromeTabId,
    /// Element ref `[eN]` from the tab's latest snapshot.
    #[serde(rename = "ref")]
    ref_id: String,
    /// Text to type into the element.
    text: String,
    /// Press Enter after typing (form submission).
    #[serde(default)]
    submit: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PressKeyInput {
    /// Tab id returned by `ChromeUseOpen` or `ChromeUseTabs`.
    tab_id: ChromeTabId,
    /// Key to press (`Enter`, `Tab`, `ArrowDown`, `Control+A`, …).
    key: String,
    /// Element ref `[eN]` to focus first; omit to press on the focused element.
    #[serde(default, rename = "ref")]
    ref_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SelectOptionInput {
    /// Tab id returned by `ChromeUseOpen` or `ChromeUseTabs`.
    tab_id: ChromeTabId,
    /// Element ref `[eN]` of the `<select>` from the tab's latest snapshot.
    #[serde(rename = "ref")]
    ref_id: String,
    /// Option values to select; exact values or visible labels both match.
    values: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ScrollDirection {
    Up,
    Down,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScrollInput {
    /// Tab id returned by `ChromeUseOpen` or `ChromeUseTabs`.
    tab_id: ChromeTabId,
    /// Element ref `[eN]` to reveal; omit to scroll the page in place.
    #[serde(default, rename = "ref")]
    ref_id: Option<String>,
    /// Scroll direction (ignored when `ref` is given).
    direction: ScrollDirection,
    /// Scroll distance in pixels (default 600; ignored when `ref` is given).
    #[serde(default)]
    pixels: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WaitForInput {
    /// Tab id returned by `ChromeUseOpen` or `ChromeUseTabs`.
    tab_id: ChromeTabId,
    /// Block until this text appears on the page (10s budget).
    #[serde(default)]
    text: Option<String>,
    /// Block until this text disappears from the page (10s budget).
    #[serde(default)]
    text_gone: Option<String>,
    /// Block for this many seconds.
    #[serde(default)]
    time: Option<f64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScreenshotInput {
    /// Tab id returned by `ChromeUseOpen` or `ChromeUseTabs`.
    tab_id: ChromeTabId,
    /// Capture the entire scrollable page instead of the viewport.
    #[serde(default)]
    full_page: bool,
}

#[derive(Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TabAction {
    List,
    New,
    Select,
    Close,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TabsInput {
    /// `list` (adopts tabs opened outside ChromeUse), `new`, `select`, or
    /// `close`.
    action: TabAction,
    /// Target tab for `select` / `close`.
    #[serde(default)]
    tab_id: Option<ChromeTabId>,
    /// URL for the `new` action; omit for a blank tab.
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EvaluateInput {
    /// Tab id returned by `ChromeUseOpen` or `ChromeUseTabs`.
    tab_id: ChromeTabId,
    /// JavaScript to run in the page with Playwright semantics: a function
    /// literal like `() => document.title` is invoked; any other expression
    /// returns its completion value.
    function: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CloseInput {
    /// Tab to close; omit to close the whole Chrome session.
    #[serde(default)]
    tab_id: Option<ChromeTabId>,
}

// ─── tools ──────────────────────────────────────────────────────────────────

/// Launch/attach the shared Chrome session and open the first tab.
pub struct ChromeUseOpenTool;
/// Navigate a tab to a new url.
pub struct ChromeUseNavigateTool;
/// Compact snapshot tree with element refs.
pub struct ChromeUseSnapshotTool;
/// Click an element by snapshot ref.
pub struct ChromeUseClickTool;
/// Type text into an element by snapshot ref.
pub struct ChromeUseTypeTool;
/// Press a native key, optionally focusing a ref first.
pub struct ChromeUsePressKeyTool;
/// Select `<select>` options by snapshot ref.
pub struct ChromeUseSelectOptionTool;
/// Scroll the page or reveal an element.
pub struct ChromeUseScrollTool;
/// Block until text appears/disappears or time elapses.
pub struct ChromeUseWaitForTool;
/// PNG screenshot as an image block.
pub struct ChromeUseScreenshotTool;
/// Tab management (list/new/select/close).
pub struct ChromeUseTabsTool;
/// Evaluate JavaScript in a tab's page.
pub struct ChromeUseEvaluateTool;
/// Close one tab or the whole session.
pub struct ChromeUseCloseTool;

#[async_trait::async_trait]
impl AgentTool for ChromeUseOpenTool {
    fn name(&self) -> &str {
        "ChromeUseOpen"
    }
    fn description(&self) -> &str {
        "Launch the shared Chrome session (or attach to a running Chrome via `cdp_endpoint`) \
         and open a tab, optionally navigated to `url`. Session options only apply when the \
         session starts; an already-running session is reused. Returns `{\"tab_id\": N}` plus \
         a snapshot when a url was given. Chrome's network egress bypasses the bash sandbox. \
         Requires approval (subject to the thread's approval mode)."
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
        let input: OpenInput = parse(params)?;
        let (tab_id, report) = super::bridge::run(signal, move |cancel| {
            let rt = runtime::runtime();
            rt.ensure_session(input.cdp_endpoint.as_deref(), input.headless, cancel)?;
            let tab_id = rt.open_tab(input.url.as_deref(), cancel)?;
            let report = match input.url {
                Some(_) => Some(rt.snapshot(tab_id, cancel)?),
                None => None,
            };
            Ok((tab_id, report))
        })
        .await?;
        let mut text = format!("{{\"tab_id\":{tab_id}}}");
        if let Some(report) = report {
            text.push_str("\n\n");
            text.push_str(&report);
        }
        Ok(AgentToolResult::text(text))
    }
}

#[async_trait::async_trait]
impl AgentTool for ChromeUseNavigateTool {
    fn name(&self) -> &str {
        "ChromeUseNavigate"
    }
    fn description(&self) -> &str {
        "Navigate a ChromeUse tab to a new `url` and return the fresh snapshot. Requires \
         approval (subject to the thread's approval mode)."
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
        let input: NavigateInput = parse(params)?;
        let url = input.url.clone();
        let report = super::bridge::run(signal, move |cancel| {
            runtime::runtime().navigate(input.tab_id, &input.url, cancel)
        })
        .await?;
        Ok(AgentToolResult::text(format!(
            "Navigated tab {} to {url}.\n\n{report}",
            input.tab_id
        )))
    }
}

#[async_trait::async_trait]
impl AgentTool for ChromeUseSnapshotTool {
    fn name(&self) -> &str {
        "ChromeUseSnapshot"
    }
    fn description(&self) -> &str {
        "Return the tab's page snapshot: a compact accessibility-style tree with element refs \
         `[eN]`. Act on the page by passing those refs to the other ChromeUse tools. Refs are \
         valid only for the snapshot that issued them. Read-only — no approval needed."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TabInput>()
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
        let input: TabInput = parse(params)?;
        let report = super::bridge::run(signal, move |cancel| {
            runtime::runtime().snapshot(input.tab_id, cancel)
        })
        .await?;
        Ok(AgentToolResult::text(report))
    }
}

#[async_trait::async_trait]
impl AgentTool for ChromeUseClickTool {
    fn name(&self) -> &str {
        "ChromeUseClick"
    }
    fn description(&self) -> &str {
        "Click the element `[ref]` from the tab's latest snapshot and return the fresh \
         snapshot. Requires approval (subject to the thread's approval mode)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<RefInput>()
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
        let input: RefInput = parse(params)?;
        let ref_id = input.ref_id.clone();
        let report = super::bridge::run(signal, move |cancel| {
            let rt = runtime::runtime();
            let selector = rt.ref_selector(input.tab_id, &input.ref_id)?;
            rt.with_tab(input.tab_id, |entry| {
                entry
                    .page
                    .click_with_cancel(&selector, Some(runtime::ACTION_TIMEOUT_MS), cancel)
                    .map_err(|e| format!("click failed: {e}"))?;
                entry.refs.clear();
                let (text, refs) = snapshot::take(&entry.page, cancel)?;
                entry.refs = refs;
                Ok(text)
            })
        })
        .await?;
        Ok(AgentToolResult::text(format!(
            "Clicked [{ref_id}].\n\n{report}"
        )))
    }
}

#[async_trait::async_trait]
impl AgentTool for ChromeUseTypeTool {
    fn name(&self) -> &str {
        "ChromeUseType"
    }
    fn description(&self) -> &str {
        "Set the element `[ref]`'s value to `text` (replacing existing content); set \
         `submit` to press Enter afterwards. Returns the fresh snapshot. Requires approval \
         (subject to the thread's approval mode)."
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
        let input: TypeInput = parse(params)?;
        let ref_id = input.ref_id.clone();
        let submitted = input.submit;
        let report = super::bridge::run(signal, move |cancel| {
            let rt = runtime::runtime();
            let selector = rt.ref_selector(input.tab_id, &input.ref_id)?;
            rt.with_tab(input.tab_id, |entry| {
                entry
                    .page
                    .fill_with_cancel(
                        &selector,
                        &input.text,
                        Some(runtime::ACTION_TIMEOUT_MS),
                        cancel,
                    )
                    .map_err(|e| format!("type failed: {e}"))?;
                if input.submit {
                    entry
                        .page
                        .press_key_with_timeout_and_cancel(
                            Some(&selector),
                            "Enter",
                            Some(runtime::ACTION_TIMEOUT_MS),
                            cancel,
                        )
                        .map_err(|e| format!("submit (Enter) failed: {e}"))?;
                }
                entry.refs.clear();
                let (text, refs) = snapshot::take(&entry.page, cancel)?;
                entry.refs = refs;
                Ok(text)
            })
        })
        .await?;
        let confirmation = match submitted {
            true => format!("Typed into [{ref_id}] and submitted."),
            false => format!("Typed into [{ref_id}]."),
        };
        Ok(AgentToolResult::text(format!("{confirmation}\n\n{report}")))
    }
}

#[async_trait::async_trait]
impl AgentTool for ChromeUsePressKeyTool {
    fn name(&self) -> &str {
        "ChromeUsePressKey"
    }
    fn description(&self) -> &str {
        "Press a native key (`Enter`, `Tab`, `ArrowDown`, `Control+A`, …) on the focused \
         element, optionally focusing `[ref]` first. Returns the fresh snapshot. Requires \
         approval (subject to the thread's approval mode)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<PressKeyInput>()
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
        let input: PressKeyInput = parse(params)?;
        let key = input.key.clone();
        let report = super::bridge::run(signal, move |cancel| {
            let rt = runtime::runtime();
            let selector = match &input.ref_id {
                Some(ref_id) => Some(rt.ref_selector(input.tab_id, ref_id)?),
                None => None,
            };
            rt.with_tab(input.tab_id, |entry| {
                entry
                    .page
                    .press_key_with_timeout_and_cancel(
                        selector.as_deref(),
                        &input.key,
                        Some(runtime::ACTION_TIMEOUT_MS),
                        cancel,
                    )
                    .map_err(|e| format!("press failed: {e}"))?;
                entry.refs.clear();
                let (text, refs) = snapshot::take(&entry.page, cancel)?;
                entry.refs = refs;
                Ok(text)
            })
        })
        .await?;
        Ok(AgentToolResult::text(format!("Pressed {key}.\n\n{report}")))
    }
}

#[async_trait::async_trait]
impl AgentTool for ChromeUseSelectOptionTool {
    fn name(&self) -> &str {
        "ChromeUseSelectOption"
    }
    fn description(&self) -> &str {
        "Select option `values` in the `<select>` element `[ref]`; exact option values or \
         visible labels both match. Returns the fresh snapshot. Requires approval (subject to \
         the thread's approval mode)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<SelectOptionInput>()
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
        let input: SelectOptionInput = parse(params)?;
        let ref_id = input.ref_id.clone();
        let report = super::bridge::run(signal, move |cancel| {
            let rt = runtime::runtime();
            let selector = rt.ref_selector(input.tab_id, &input.ref_id)?;
            rt.with_tab(input.tab_id, |entry| {
                let selected = entry
                    .page
                    .select_options_with_cancel(
                        &selector,
                        &input.values,
                        Some(runtime::ACTION_TIMEOUT_MS),
                        cancel,
                    )
                    .map_err(|e| format!("select failed: {e}"))?;
                entry.refs.clear();
                let (text, refs) = snapshot::take(&entry.page, cancel)?;
                entry.refs = refs;
                Ok((selected, text))
            })
        })
        .await?;
        Ok(AgentToolResult::text(format!(
            "Selected {:?} in [{}].\n\n{}",
            report.0, ref_id, report.1
        )))
    }
}

#[async_trait::async_trait]
impl AgentTool for ChromeUseScrollTool {
    fn name(&self) -> &str {
        "ChromeUseScroll"
    }
    fn description(&self) -> &str {
        "Scroll the tab's page `direction` by `pixels` (default 600), or reveal the element \
         `[ref]` when given. Returns the fresh snapshot. Requires approval (subject to the \
         thread's approval mode)."
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
        let input: ScrollInput = parse(params)?;
        let pixels = input.pixels.unwrap_or(600);
        let direction_word = match input.direction {
            ScrollDirection::Down => "down",
            ScrollDirection::Up => "up",
        };
        let ref_id = input.ref_id.clone();
        let report = super::bridge::run(signal, move |cancel| {
            let rt = runtime::runtime();
            let selector = match &input.ref_id {
                Some(ref_id) => Some(rt.ref_selector(input.tab_id, ref_id)?),
                None => None,
            };
            rt.with_tab(input.tab_id, |entry| {
                match selector {
                    Some(selector) => entry
                        .page
                        .scroll_into_view_with_cancel(
                            &selector,
                            Some(runtime::ACTION_TIMEOUT_MS),
                            cancel,
                        )
                        .map_err(|e| format!("scroll failed: {e}"))?,
                    None => {
                        let delta = match input.direction {
                            ScrollDirection::Down => pixels as f64,
                            ScrollDirection::Up => -(pixels as f64),
                        };
                        entry
                            .page
                            .scroll_viewport_with_cancel(
                                delta,
                                Some(runtime::ACTION_TIMEOUT_MS),
                                cancel,
                            )
                            .map_err(|e| format!("scroll failed: {e}"))?
                    }
                }
                entry.refs.clear();
                let (text, refs) = snapshot::take(&entry.page, cancel)?;
                entry.refs = refs;
                Ok(text)
            })
        })
        .await?;
        let confirmation = match &ref_id {
            Some(ref_id) => format!("Scrolled [{ref_id}] into view."),
            None => format!("Scrolled {direction_word} {pixels}px."),
        };
        Ok(AgentToolResult::text(format!("{confirmation}\n\n{report}")))
    }
}

#[async_trait::async_trait]
impl AgentTool for ChromeUseWaitForTool {
    fn name(&self) -> &str {
        "ChromeUseWaitFor"
    }
    fn description(&self) -> &str {
        "Block until page `text` appears, `text_gone` disappears (10s budget each), or \
         `time` seconds elapse — exactly one of the three must be given. Read-only — no \
         approval needed."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<WaitForInput>()
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
        let input: WaitForInput = parse(params)?;
        let target = match (input.text, input.text_gone, input.time) {
            (Some(text), None, None) => WaitTarget::Appears(text),
            (None, Some(text), None) => WaitTarget::Disappears(text),
            (None, None, Some(seconds)) if seconds > 0.0 => {
                WaitTarget::Sleep(std::time::Duration::from_secs_f64(seconds))
            }
            _ => {
                return Err(ToolError::InvalidArguments(
                    "give exactly one of `text`, `text_gone`, or a positive `time`".into(),
                ));
            }
        };
        let outcome = super::bridge::run(signal, move |cancel| {
            runtime::runtime().wait_for(input.tab_id, target, cancel)
        })
        .await?;
        Ok(AgentToolResult::text(outcome))
    }
}

#[async_trait::async_trait]
impl AgentTool for ChromeUseScreenshotTool {
    fn name(&self) -> &str {
        "ChromeUseScreenshot"
    }
    fn description(&self) -> &str {
        "Capture a PNG screenshot of the tab's page (the viewport, or the entire page with \
         `full_page`) and return it as an image. Read-only — no approval needed."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<ScreenshotInput>()
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
        let input: ScreenshotInput = parse(params)?;
        let (bytes, url) = super::bridge::run(signal, move |cancel| {
            runtime::runtime().screenshot(input.tab_id, input.full_page, cancel)
        })
        .await?;
        if bytes.len() > MAX_INLINE_SCREENSHOT_BYTES {
            let path = std::env::temp_dir().join(format!(
                "manox-chrome-screenshot-{}.png",
                uuid::Uuid::new_v4()
            ));
            std::fs::write(&path, &bytes).map_err(|e| {
                ToolError::ExecutionFailed(format!("saving screenshot failed: {e}"))
            })?;
            return Ok(AgentToolResult::text(format!(
                "screenshot of {url} too large to inline ({} bytes); saved to {}",
                bytes.len(),
                path.display()
            )));
        }
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(AgentToolResult {
            content: vec![
                ContentBlock::Text {
                    text: format!("Screenshot of tab {} ({url})", input.tab_id),
                    signature: None,
                },
                ContentBlock::Image {
                    data,
                    mime_type: "image/png".into(),
                },
            ],
            details: None,
            is_error: false,
            usage: None,
            added_tool_names: None,
            terminate: false,
        })
    }
}

#[async_trait::async_trait]
impl AgentTool for ChromeUseTabsTool {
    fn name(&self) -> &str {
        "ChromeUseTabs"
    }
    fn description(&self) -> &str {
        "Manage the Chrome session's tabs: `list` (also adopts tabs opened outside \
         ChromeUse), `new` (optionally navigated via `url`), `select` (report a tab — \
         ChromeUse tools address tabs by explicit tab_id), or `close`. `list` is read-only; \
         the other actions require approval (subject to the thread's approval mode)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<TabsInput>()
    }
    fn requires_approval(&self, params: &serde_json::Value) -> bool {
        let input: Result<TabsInput, _> = serde_json::from_value(params.clone());
        !matches!(
            input,
            Ok(TabsInput {
                action: TabAction::List,
                ..
            })
        )
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: TabsInput = parse(params)?;
        match input.action {
            TabAction::List => {
                let tabs =
                    super::bridge::run(signal, move |_| runtime::runtime().list_tabs(true)).await?;
                let rows: Vec<serde_json::Value> = tabs
                    .into_iter()
                    .map(|(tab_id, url)| serde_json::json!({ "tab_id": tab_id, "url": url }))
                    .collect();
                Ok(AgentToolResult::text(
                    serde_json::json!({ "tabs": rows }).to_string(),
                ))
            }
            TabAction::New => {
                let url = input.url.clone();
                let tab_id = super::bridge::run(signal, move |cancel| {
                    let rt = runtime::runtime();
                    rt.ensure_session(None, None, cancel)?;
                    rt.open_tab(url.as_deref(), cancel)
                })
                .await?;
                Ok(AgentToolResult::text(format!("{{\"tab_id\":{tab_id}}}")))
            }
            TabAction::Select => {
                let tab_id = input.tab_id.ok_or_else(|| {
                    ToolError::InvalidArguments("`select` requires `tab_id`".into())
                })?;
                let tabs = super::bridge::run(signal, move |_| runtime::runtime().list_tabs(false))
                    .await?;
                let found = tabs.into_iter().find(|(id, _)| *id == tab_id);
                match found {
                    Some((_, url)) => Ok(AgentToolResult::text(format!(
                        "Tab {tab_id} is open at {url}; address it by tab_id in ChromeUse tools."
                    ))),
                    None => Err(ToolError::ExecutionFailed(format!(
                        "unknown tab_id {tab_id}; list open tabs with ChromeUseTabs"
                    ))),
                }
            }
            TabAction::Close => {
                let tab_id = input.tab_id.ok_or_else(|| {
                    ToolError::InvalidArguments(
                        "`close` requires `tab_id`; use ChromeUseClose to end the session".into(),
                    )
                })?;
                super::bridge::run(signal, move |_| runtime::runtime().close_tab(tab_id)).await?;
                Ok(AgentToolResult::text(format!("Closed tab {tab_id}.")))
            }
        }
    }
}

#[async_trait::async_trait]
impl AgentTool for ChromeUseEvaluateTool {
    fn name(&self) -> &str {
        "ChromeUseEvaluate"
    }
    fn description(&self) -> &str {
        "Evaluate JavaScript in the tab's page with Playwright semantics: a function literal \
         like `() => document.title` is invoked; any other expression returns its completion \
         value. Returns the JSON-encoded result. Requires approval (subject to the thread's \
         approval mode)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<EvaluateInput>()
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
        let input: EvaluateInput = parse(params)?;
        let result = super::bridge::run(signal, move |cancel| {
            runtime::runtime().evaluate(input.tab_id, &input.function, cancel)
        })
        .await?;
        Ok(AgentToolResult::text(result))
    }
}

#[async_trait::async_trait]
impl AgentTool for ChromeUseCloseTool {
    fn name(&self) -> &str {
        "ChromeUseClose"
    }
    fn description(&self) -> &str {
        "Close one tab (`tab_id`) or the whole Chrome session when `tab_id` is omitted — a \
         launched Chrome process is terminated, an attached one is left running. Requires \
         approval (subject to the thread's approval mode)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<CloseInput>()
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
        let input: CloseInput = parse(params)?;
        match input.tab_id {
            Some(tab_id) => {
                super::bridge::run(signal, move |_| runtime::runtime().close_tab(tab_id)).await?;
                Ok(AgentToolResult::text(format!("Closed tab {tab_id}.")))
            }
            None => {
                super::bridge::run(signal, move |_| runtime::runtime().close_session()).await?;
                Ok(AgentToolResult::text("Closed the Chrome session."))
            }
        }
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

    #[test]
    fn approval_flags_match_trust_axes() {
        // Read axis: approval-free + read-only (plan mode exposes them).
        assert!(!ChromeUseSnapshotTool.requires_approval(&serde_json::json!({})));
        assert!(ChromeUseSnapshotTool.is_read_only());
        assert!(!ChromeUseWaitForTool.requires_approval(&serde_json::json!({})));
        assert!(ChromeUseWaitForTool.is_read_only());
        assert!(!ChromeUseScreenshotTool.requires_approval(&serde_json::json!({})));
        assert!(ChromeUseScreenshotTool.is_read_only());
        // Write axis: approval-gated, not read-only.
        for gated in [
            ChromeUseOpenTool.requires_approval(&serde_json::json!({})),
            ChromeUseNavigateTool.requires_approval(&serde_json::json!({})),
            ChromeUseClickTool.requires_approval(&serde_json::json!({})),
            ChromeUseTypeTool.requires_approval(&serde_json::json!({})),
            ChromeUsePressKeyTool.requires_approval(&serde_json::json!({})),
            ChromeUseSelectOptionTool.requires_approval(&serde_json::json!({})),
            ChromeUseScrollTool.requires_approval(&serde_json::json!({})),
            ChromeUseEvaluateTool.requires_approval(&serde_json::json!({})),
            ChromeUseCloseTool.requires_approval(&serde_json::json!({})),
        ] {
            assert!(gated);
        }
        assert!(!ChromeUseClickTool.is_read_only());
    }

    #[test]
    fn tabs_approval_depends_on_action() {
        let tool = ChromeUseTabsTool;
        assert!(!tool.requires_approval(&serde_json::json!({"action": "list"})));
        assert!(tool.requires_approval(&serde_json::json!({"action": "new"})));
        assert!(tool.requires_approval(&serde_json::json!({"action": "close", "tab_id": 1})));
        // Unparseable params stay gated (fail closed).
        assert!(tool.requires_approval(&serde_json::json!({"action": "bogus"})));
    }

    #[test]
    fn ref_input_accepts_the_ref_field_name() {
        let input: RefInput =
            serde_json::from_value(serde_json::json!({"tab_id": 3, "ref": "e12"})).unwrap();
        assert_eq!(input.tab_id, 3);
        assert_eq!(input.ref_id, "e12");
        // `ref_id` is the Rust field name only; the wire name is `ref`.
        assert!(
            serde_json::from_value::<RefInput>(serde_json::json!({"tab_id": 3, "ref_id": "e12"}))
                .is_err()
        );
    }

    #[tokio::test]
    async fn wait_for_rejects_ambiguous_or_empty_conditions() {
        let tool = ChromeUseWaitForTool;
        for params in [
            serde_json::json!({"tab_id": 1}),
            serde_json::json!({"tab_id": 1, "text": "a", "time": 1.0}),
            serde_json::json!({"tab_id": 1, "time": -2.0}),
        ] {
            let err = tool
                .execute("c1", params, CancellationToken::new(), &tool_ctx())
                .await
                .unwrap_err();
            assert!(matches!(err, ToolError::InvalidArguments(_)), "{err}");
        }
    }

    #[tokio::test]
    async fn tabs_list_fails_closed_without_a_session() {
        let err = ChromeUseTabsTool
            .execute(
                "c1",
                serde_json::json!({"action": "list"}),
                CancellationToken::new(),
                &tool_ctx(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no Chrome session"), "{err}");
    }
}
