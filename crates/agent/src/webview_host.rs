//! Browser host abstraction — the agent-side boundary for the built-in browser.
//!
//! `agent` does NOT depend on `manox-webview`. This module defines only the
//! data types and the `BrowserHost` trait; the concrete implementation lives in
//! `agent-ui` (which holds a `WeakEntity<Workspace>` and owns the real
//! `manox_webview::WebView`s), registered process-wide via `set_host` at App
//! startup. Tools in `tools/web_explore/` reach the host through `host()`.
//!
//! The split keeps `agent` pure (no GPUI-webview coupling) while letting the
//! outbound trust axis (agent → page, governed by `ApprovalMode`) and the
//! inbound trust axis (page → agent, always-confirmed, `ApprovalMode`-blind)
//! meet at a single host surface.

use std::sync::{Arc, OnceLock};

use gpui::{App, Task};
use serde::{Deserialize, Serialize};

/// Process-unique handle for an open browser tab. Allocated by the host; tools
/// pass it back verbatim to address the tab they opened. Opaque to the agent
/// crate — the host maps it to its real webview identity.
pub type BrowserTabId = u64;

/// A closed-enum notification an untrusted browser page sends back to manox.
/// Mirrors `manox_webview::BrowserNotification` without introducing a
/// `webview` dependency on this crate; `agent-ui` converts at the boundary.
///
/// `EvalResult` correlates an injected read script's return value by
/// `request_id` — the host allocates the id, injects a script that calls
/// `__manox_notify__("eval_result", { request_id, payload })`, and pairs the
/// arriving notification with a pending oneshot to resolve the read `Task`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserNotification {
    PageLoaded,
    DomChanged,
    Navigation {
        url: String,
    },
    UserHandback,
    EvalResult {
        request_id: u64,
        payload: serde_json::Value,
    },
}

/// An inbound write request an untrusted page makes via
/// `__manox_request_write__(intent, payload)`. This is never executed directly
/// — the host routes it to a confirmation overlay that ignores `ApprovalMode`
/// (the inbound axis is orthogonal to outbound approval). `intent` is a closed
/// command name; no intents are registered yet, so every request is rejected
/// until a write surface is added.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BrowserInboundWrite {
    pub intent: String,
    pub payload: serde_json::Value,
}

/// The browser surface an agent drives. Implemented by `agent-ui`; reached by
/// the `web_explore_*` tools through [`host`].
///
/// Outbound read operations (`read_text` / `read_dom` / `screenshot` /
/// `eval_script`) inject a script into the page and await its `EvalResult`
/// notification — there is no persistent bridge the page can hold, so an
/// untrusted page never gains a lasting handle into manox. Outbound write
/// operations (`click` / `type_text` / `scroll` / `navigate` / `open_tab` /
/// `close_tab` / `yield_to_user`) are subject to the owning thread's
/// `ApprovalMode`; `yield_to_user` blocks the tool's `Task` until the user
/// triggers a `UserHandback` notification.
pub trait BrowserHost: Send + Sync {
    /// Open a new browser tab navigated to `url`; return its id.
    fn open_tab(&self, url: &str, cx: &mut App) -> Result<BrowserTabId, String>;
    /// Navigate an existing tab to a new url.
    fn navigate(&self, id: BrowserTabId, url: &str, cx: &mut App) -> Result<(), String>;
    /// Inject `js` and await its return value (JSON-serialized by the page).
    fn eval_script(&self, id: BrowserTabId, js: &str, cx: &mut App)
    -> Task<Result<String, String>>;
    /// Read the page's main text content (readability-extracted).
    fn read_text(&self, id: BrowserTabId, cx: &mut App) -> Task<Result<String, String>>;
    /// Read `outerHTML` of the first element matching `selector`, or the whole
    /// document when `selector` is `None`.
    fn read_dom(
        &self,
        id: BrowserTabId,
        selector: Option<String>,
        cx: &mut App,
    ) -> Task<Result<String, String>>;
    /// Click the first element matching `selector`.
    fn click(&self, id: BrowserTabId, selector: &str, cx: &mut App) -> Task<Result<(), String>>;
    /// Focus the first element matching `selector` and type `text` into it.
    fn type_text(
        &self,
        id: BrowserTabId,
        selector: &str,
        text: &str,
        cx: &mut App,
    ) -> Task<Result<(), String>>;
    /// Scroll the page by `(dx, dy)` device pixels.
    fn scroll(&self, id: BrowserTabId, dx: i32, dy: i32, cx: &mut App) -> Task<Result<(), String>>;
    /// Return a DOM snapshot of the visible viewport (structure + metadata,
    /// not a pixel image).
    fn screenshot(&self, id: BrowserTabId, cx: &mut App) -> Task<Result<String, String>>;
    /// Yield control of the tab to the user (e.g. for a login handshake). The
    /// returned `Task` resolves once the user triggers `UserHandback`.
    fn yield_to_user(&self, id: BrowserTabId, cx: &mut App) -> Task<Result<(), String>>;
    /// Close and reclaim the tab. Returns `Err` if `id` does not identify an
    /// open tab, so a stale id surfaces as a tool error rather than a silent
    /// no-op confirmation.
    fn close_tab(&self, id: BrowserTabId, cx: &mut App) -> Result<(), String>;
}

static HOST: OnceLock<Arc<dyn BrowserHost>> = OnceLock::new();

/// Register the process-wide browser host. Call once at App startup, after the
/// workspace exists. A second registration is a no-op — the first host wins,
/// matching the single-workspace, single-process delivery model.
pub fn set_host(host: Arc<dyn BrowserHost>) {
    let _ = HOST.set(host);
}

/// The registered browser host, or `None` before `set_host` (e.g. in non-UI
/// contexts). Tools call this; `None` makes them return a clean error rather
/// than panic.
pub fn host() -> Option<&'static Arc<dyn BrowserHost>> {
    HOST.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The internally-tagged `BrowserNotification` must round-trip every variant
    // — a newtype-of-String variant would fail serde's tag-merge at runtime, so
    // `Navigation` is a struct variant. This test pins that invariant against
    // any future "simplify back to `Navigation(String)`" refactor.
    #[test]
    fn notification_round_trips() {
        let cases: Vec<BrowserNotification> = vec![
            BrowserNotification::PageLoaded,
            BrowserNotification::DomChanged,
            BrowserNotification::Navigation {
                url: "https://example.com".into(),
            },
            BrowserNotification::UserHandback,
            BrowserNotification::EvalResult {
                request_id: 42,
                payload: serde_json::json!({"ok": true}),
            },
        ];
        for notification in cases {
            let json = serde_json::to_value(&notification).expect("serialize");
            let back: BrowserNotification = serde_json::from_value(json).expect("deserialize");
            assert_eq!(
                serde_json::to_value(&back).unwrap(),
                serde_json::to_value(&notification).unwrap(),
                "round-trip not idempotent"
            );
        }
    }

    #[test]
    fn notification_navigation_serializes_with_tag_and_url() {
        let n = BrowserNotification::Navigation {
            url: "https://example.com".into(),
        };
        let json = serde_json::to_value(&n).expect("serialize navigation");
        assert_eq!(json["kind"], "navigation");
        assert_eq!(json["url"], "https://example.com");
    }

    #[test]
    fn inbound_write_round_trips() {
        let w = BrowserInboundWrite {
            intent: "save_file".into(),
            payload: serde_json::json!({"path": "/tmp/x"}),
        };
        let json = serde_json::to_value(&w).expect("serialize");
        let back: BrowserInboundWrite = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.intent, "save_file");
        assert_eq!(back.payload["path"], "/tmp/x");
    }
}
