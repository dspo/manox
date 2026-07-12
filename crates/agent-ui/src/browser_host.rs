//! `WorkspaceBrowserHost` — the concrete `BrowserHost` driving the built-in
//! browser, plus the routing that connects an untrusted page's notifications
//! and inbound-write requests back to the owning thread.
//!
//! The host is a process-wide singleton (`set_host` at App startup) reached by
//! the `web_explore_*` tools through `agent::webview_host::host()`. It owns a
//! `WeakEntity<Workspace>` for the outbound operations (open/navigate/eval,
//! which touch the live `BrowserView` entities) and a routing table that maps
//! each tab's webview label to its owning `Thread`.
//!
//! Two trust axes meet here:
//! - Outbound (agent → page): `eval_script` / `click` / `type_text` / `scroll`
//!   inject scripts via `WebView::evaluate_script`. Reads (`read_text` /
//!   `read_dom` / `screenshot`) inject an extraction script and await its
//!   `EvalResult` notification, paired by `request_id`.
//! - Inbound (page → agent): `__manox_request_write__` is fire-and-forget on
//!   the page side; the host routes it to a `ThreadEvent::InboundAuthorization`
//!   whose resolution is parked in `Thread::pending_inbound`. This axis ignores
//!   `ApprovalMode` — a page must never gain a write path because the agent
//!   runs in Yolo.
//!
//! The webview crate's notify/inbound bridges fire on the gpui main thread via
//! `PlatformDispatcher::dispatch_on_main_thread`, whose runnable carries no
//! `&mut App`. So those closures do only what needs no cx (resolve a pending
//! eval/yield oneshot, or push a message onto a channel) and ship the
//! cx-requiring work (emitting a `ThreadEvent`) to a drainer `Task` spawned on
//! the Workspace, which does have an `AsyncApp`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use gpui::{App, AppContext as _, AsyncApp, Entity, Task, WeakEntity};
use tokio::sync::oneshot;

use agent::thread::{Thread, ThreadEvent};
use agent::webview_host::{
    BrowserHost, BrowserInboundWrite as AgentInboundWrite,
    BrowserNotification as AgentNotification, BrowserTabId,
};
use manox_webview::{BrowserInboundWrite as WvInboundWrite, BrowserNotification as WvNotification};

use crate::workspace::Workspace;

/// A fully-resolved message the OnceLock notify/inbound handlers ship to the
/// drainer. EvalResult and UserHandback are resolved against pending oneshots
/// directly in the notify handler (no cx needed) and never reach the channel;
/// only page-state notifications and inbound-write requests travel here.
pub(crate) enum HostMessage {
    Notify {
        tab_id: BrowserTabId,
        notification: AgentNotification,
    },
    InboundWrite {
        tab_id: BrowserTabId,
        write: AgentInboundWrite,
    },
}

/// Per-tab routing + pending-oneshot state. Lives in the host's table for the
/// lifetime of the tab; dropped (closing pending senders) when the tab is
/// closed or the owning thread is released.
struct TabState {
    thread: WeakEntity<Thread>,
    label: String,
    /// Pending eval-script oneshots, keyed by `request_id`. A read op injects
    /// a script that calls `__manox_notify__("eval_result", { request_id,
    /// payload })`; the notify handler pairs the arriving payload back to the
    /// parked `Task` awaiting it.
    pending_evals: Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>,
    /// The single pending `yield_to_user` oneshot for this tab. A tab has at
    /// most one outstanding handoff; a second `yield_to_user` supersedes the
    /// first (dropping the prior sender cancels the old await).
    pending_yield: Mutex<Option<oneshot::Sender<()>>>,
}

/// Shared, lock-guarded routing state — the label↔tab map and the per-tab
/// state. Held by both the host (outbound ops register here) and the OnceLock
/// notify/inbound closures (which resolve oneshots / enqueue messages),
/// because the closures run without an `AsyncApp` and must reach this state
/// without going through the host trait.
#[derive(Default)]
pub(crate) struct Routes {
    label_to_tab: Mutex<HashMap<String, BrowserTabId>>,
    tabs: Mutex<HashMap<BrowserTabId, TabState>>,
}

pub struct WorkspaceBrowserHost {
    weak_ws: WeakEntity<Workspace>,
    routes: Arc<Routes>,
    tx: Sender<HostMessage>,
    next_request_id: AtomicU64,
}

static HOST: OnceLock<Arc<WorkspaceBrowserHost>> = OnceLock::new();

/// Process-wide counter allocating unique inbound-write request ids. Globally
/// unique → unique per-thread (a thread's `pending_inbound` map is keyed by
/// this id).
static NEXT_INBOUND_ID: AtomicU64 = AtomicU64::new(1);

impl WorkspaceBrowserHost {
    /// Construct the host bound to the main `Workspace`. Returns the host and
    /// the channel receiver the drainer consumes — the host keeps only the
    /// sender side (outbound ops push nothing onto this channel; only the
    /// notify/inbound closures do).
    pub(crate) fn new(workspace: Entity<Workspace>) -> (Arc<Self>, Receiver<HostMessage>) {
        let (tx, rx) = async_channel::bounded(256);
        let host = Arc::new(Self {
            weak_ws: workspace.downgrade(),
            routes: Arc::new(Routes::default()),
            tx,
            next_request_id: AtomicU64::new(1),
        });
        (host, rx)
    }

    /// The shared routing state, for the drainer to resolve a tab back to its
    /// owning thread.
    pub(crate) fn routes(&self) -> &Arc<Routes> {
        &self.routes
    }

    /// Register the host in the agent-ui concrete registry. Called once at App
    /// startup, after the Workspace exists. A second registration is a no-op —
    /// the first host wins (single-workspace, single-process model).
    pub(crate) fn set_concrete(host: Arc<WorkspaceBrowserHost>) {
        let _ = HOST.set(host);
    }

    /// The concrete host, or `None` before [`set_concrete`] (e.g. `BrowserView`
    /// built before startup wires the host). `None` makes `BrowserView` skip
    /// attaching the bridges — the page's notifications are then dropped at
    /// the webview layer (logged), never reaching a thread.
    pub(crate) fn concrete() -> Option<Arc<WorkspaceBrowserHost>> {
        HOST.get().cloned()
    }

    /// One-shot App-startup wiring: build the host bound to `workspace`,
    /// register it in both the agent trait registry (`web_explore_*` tools
    /// reach it via `agent::webview_host::host()`) and the agent-ui concrete
    /// registry (`BrowserView` attaches the notify/inbound bridges at build),
    /// then spawn the notify/inbound drainer on the Workspace. The OnceLock
    /// notify/inbound closures run with no `&mut App`; the drainer (owning an
    /// `AsyncApp`) is the cx-bearing sink that emits onto the owning thread.
    pub fn install(workspace: Entity<Workspace>, cx: &mut AsyncApp) {
        let (host, rx) = Self::new(workspace.clone());
        let routes = host.routes().clone();
        Self::set_concrete(host.clone());
        agent::webview_host::set_host(host);
        cx.update(|cx| {
            workspace.update(cx, |_, cx| {
                cx.spawn(async move |_, cx: &mut AsyncApp| {
                    Self::drain(rx, routes, cx).await;
                })
                .detach();
            });
        });
    }

    /// Attach the process-wide notify/inbound bridges to an untrusted
    /// webview's `Builder`. Idempotent across `BrowserView`s: the webview
    /// crate's `NOTIFY_HANDLER`/`INBOUND_HANDLER` are `OnceLock`s, so only the
    /// first attached webview actually publishes them — but every `BrowserView`
    /// attaches the same closures, so a later open never finds a stale
    /// different handler.
    pub fn attach_to_builder(builder: manox_webview::Builder<'_>) -> manox_webview::Builder<'_> {
        match Self::concrete() {
            Some(host) => {
                let routes_n = host.routes.clone();
                let tx_n = host.tx.clone();
                let routes_i = host.routes.clone();
                let tx_i = host.tx.clone();
                builder
                    .on_notify(move |label, n| handle_notify(&routes_n, &tx_n, label, n))
                    .on_inbound_write(move |label, w| handle_inbound(&routes_i, &tx_i, label, w))
            }
            None => builder,
        }
    }

    /// Drain the notify/inbound channel, dispatching each message to its owning
    /// thread. Spawned once on the Workspace at App startup; runs for the
    /// process lifetime. Page-state notifications become
    /// `ThreadEvent::BrowserNotification`; inbound-write requests register a
    /// pending decision oneshot on the owning thread and emit
    /// `ThreadEvent::InboundAuthorization`. The decision await is parked in a
    /// detached `Task` per request so a never-answered confirmation overlay
    /// cannot block subsequent notifications (which would deadlock an
    /// in-flight read eval).
    pub(crate) async fn drain(rx: Receiver<HostMessage>, routes: Arc<Routes>, cx: &mut AsyncApp) {
        while let Ok(msg) = rx.recv().await {
            match msg {
                HostMessage::Notify {
                    tab_id,
                    notification,
                } => {
                    let thread = routes
                        .tabs
                        .lock()
                        .expect("routes lock poisoned")
                        .get(&tab_id)
                        .and_then(|t| t.thread.upgrade());
                    let Some(thread) = thread else {
                        continue;
                    };
                    thread.update(cx, |_, cx| {
                        cx.emit(ThreadEvent::BrowserNotification {
                            tab_id,
                            notification,
                        });
                    });
                }
                HostMessage::InboundWrite { tab_id, write } => {
                    let thread = routes
                        .tabs
                        .lock()
                        .expect("routes lock poisoned")
                        .get(&tab_id)
                        .and_then(|t| t.thread.upgrade());
                    let Some(thread) = thread else {
                        continue;
                    };
                    let id = format!(
                        "inbound-{}",
                        NEXT_INBOUND_ID.fetch_add(1, Ordering::Relaxed)
                    );
                    let (tx_inbound, rx_inbound) = oneshot::channel::<bool>();
                    let intent = write.intent.clone();
                    let payload = write.payload.clone();
                    thread.update(cx, |t, cx| {
                        t.register_inbound(id.clone(), intent, payload, tx_inbound, cx);
                    });
                    // Park the decision await off the drainer loop. No inbound
                    // intent is registered yet, so the decision is observed but
                    // not acted on — the parked `Task` is the forward-compat
                    // hook for a future write surface, and resolves cleanly
                    // when the overlay (or a tab close / thread release)
                    // drops the sender.
                    cx.background_spawn(async move {
                        let _ = rx_inbound.await;
                    })
                    .detach();
                }
            }
        }
    }

    /// Inject `js` into the tab's webview and return immediately
    /// (fire-and-forget). Reaches the `wry::WebView` via `Entity::read_with`
    /// (eval is a read-only `&self` op on wry) — no window required.
    fn inject_script(&self, id: BrowserTabId, js: &str, cx: &mut App) -> Result<(), String> {
        let ws = self
            .weak_ws
            .upgrade()
            .ok_or_else(|| "browser host: workspace dropped".to_string())?;
        let view = ws
            .read_with(cx, |ws, _| ws.browser_views.get(&id).cloned())
            .ok_or_else(|| format!("browser host: no browser tab with id {id}"))?;
        let wv = view.read_with(cx, |v, _| v.webview().clone());
        wv.read_with(cx, |w, _| w.evaluate_script(js))
            .map_err(|e| e.to_string())
    }

    /// Allocate a `request_id`, park a pending oneshot for its `EvalResult`,
    /// inject the caller-built script (which must call
    /// `__manox_notify__("eval_result", { request_id, payload })`), and return
    /// a `Task` awaiting the paired payload. A 60s timeout bounds a
    /// non-responding page so a hung eval never blocks the turn; the stale
    /// sender is best-effort removed on the timeout/drop path.
    fn eval_awaiting(
        &self,
        id: BrowserTabId,
        make_script: impl FnOnce(u64) -> String,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<serde_json::Value>();
        let registered = self
            .routes
            .tabs
            .lock()
            .expect("routes lock poisoned")
            .get(&id)
            .map(|tab| {
                tab.pending_evals
                    .lock()
                    .expect("pending_evals lock poisoned")
                    .insert(request_id, tx);
            })
            .is_some();
        if !registered {
            return cx.background_spawn(async move {
                Err(format!("browser host: no browser tab with id {id}"))
            });
        }
        let script = make_script(request_id);
        if let Err(e) = self.inject_script(id, &script, cx) {
            if let Some(tab) = self
                .routes
                .tabs
                .lock()
                .expect("routes lock poisoned")
                .get(&id)
            {
                tab.pending_evals
                    .lock()
                    .expect("pending_evals lock poisoned")
                    .remove(&request_id);
            }
            return cx.background_spawn(async move { Err(e) });
        }
        let routes = self.routes.clone();
        cx.background_spawn(async move {
            let result = match tokio::time::timeout(Duration::from_secs(60), rx).await {
                Ok(Ok(payload)) => {
                    if let Some(msg) = payload.get("__error").and_then(|v| v.as_str()) {
                        Err(format!("browser host: page script error: {msg}"))
                    } else {
                        Ok(stringify_value(&payload))
                    }
                }
                Ok(Err(_)) => {
                    Err("browser host: eval was cancelled before the page responded".to_string())
                }
                Err(_) => {
                    Err("browser host: eval timed out (60s) — the page did not respond".to_string())
                }
            };
            // The notify handler already removed the sender on success; on the
            // timeout / cancellation path, reclaim it so a late response
            // cannot resolve a future request that reused the id (it won't —
            // ids are monotonic — but dropping the sender closes the channel).
            if result.is_err()
                && let Some(tab) = routes.tabs.lock().expect("routes lock poisoned").get(&id)
            {
                tab.pending_evals
                    .lock()
                    .expect("pending_evals lock poisoned")
                    .remove(&request_id);
            }
            result
        })
    }
}

/// Stringify an `EvalResult` payload as the agent-facing string. A string
/// payload is returned raw (the common case — extracted text/HTML); a
/// non-string payload is JSON-encoded so the model still sees structure
/// (objects, numbers) rather than a lossy `to_string`.
fn stringify_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// The notify-handler side: resolve label → tab, then either resolve a parked
/// eval/yield oneshot directly (no cx needed) or enqueue a page-state message
/// for the drainer to emit as a `ThreadEvent`. Runs on the gpui main thread
/// without an `AsyncApp`, so it touches only the shared `Routes` (lock-guarded)
/// and the channel sender.
fn handle_notify(routes: &Arc<Routes>, tx: &Sender<HostMessage>, label: String, n: WvNotification) {
    let tab_id = match routes
        .label_to_tab
        .lock()
        .expect("routes lock poisoned")
        .get(&label)
        .copied()
    {
        Some(id) => id,
        None => return, // stale label — the tab was closed before the notify landed
    };
    match &n {
        WvNotification::EvalResult { request_id, .. } => {
            let req_id = *request_id;
            let payload = if let WvNotification::EvalResult { payload, .. } = &n {
                payload.clone()
            } else {
                unreachable!()
            };
            if let Some(tab) = routes
                .tabs
                .lock()
                .expect("routes lock poisoned")
                .get(&tab_id)
                && let Some(sender) = tab
                    .pending_evals
                    .lock()
                    .expect("pending_evals lock poisoned")
                    .remove(&req_id)
            {
                let _ = sender.send(payload);
            }
            return;
        }
        WvNotification::UserHandback => {
            if let Some(tab) = routes
                .tabs
                .lock()
                .expect("routes lock poisoned")
                .get(&tab_id)
                && let Some(sender) = tab
                    .pending_yield
                    .lock()
                    .expect("pending_yield lock poisoned")
                    .take()
            {
                let _ = sender.send(());
            }
            return;
        }
        _ => {}
    }
    let agent_n = match n {
        WvNotification::PageLoaded => AgentNotification::PageLoaded,
        WvNotification::DomChanged => AgentNotification::DomChanged,
        WvNotification::Navigation(url) => AgentNotification::Navigation { url },
        WvNotification::UserHandback | WvNotification::EvalResult { .. } => unreachable!(),
    };
    let _ = tx.try_send(HostMessage::Notify {
        tab_id,
        notification: agent_n,
    });
}

/// The inbound-handler side: resolve label → tab and enqueue the write for
/// the drainer to surface as a `ThreadEvent::InboundAuthorization`. Runs on the
/// gpui main thread without an `AsyncApp`; the actual confirmation overlay and
/// the parked decision oneshot are wired by the drainer (which has an
/// `AsyncApp`).
fn handle_inbound(
    routes: &Arc<Routes>,
    tx: &Sender<HostMessage>,
    label: String,
    w: WvInboundWrite,
) {
    let tab_id = match routes
        .label_to_tab
        .lock()
        .expect("routes lock poisoned")
        .get(&label)
        .copied()
    {
        Some(id) => id,
        None => return,
    };
    let write = AgentInboundWrite {
        intent: w.intent,
        payload: w.payload,
    };
    let _ = tx.try_send(HostMessage::InboundWrite { tab_id, write });
}

impl BrowserHost for WorkspaceBrowserHost {
    fn open_tab(&self, url: &str, cx: &mut App) -> Result<BrowserTabId, String> {
        let handle = crate::dispatch::window_global()
            .ok_or_else(|| "browser host: main window not available".to_string())?;
        let ws = self
            .weak_ws
            .upgrade()
            .ok_or_else(|| "browser host: workspace dropped".to_string())?;
        let (tab_id, thread) = handle
            .update(cx, |_, window, cx| {
                ws.update(cx, |ws, cx| {
                    let tab_id = ws.open_browser_tab(url, window, cx);
                    let thread = ws.thread.clone();
                    (tab_id, thread)
                })
            })
            .map_err(|e| format!("browser host: window update failed: {e}"))?;
        let label = crate::views::browser_view::webview_label_for(tab_id);
        {
            let mut labels = self
                .routes
                .label_to_tab
                .lock()
                .expect("routes lock poisoned");
            labels.insert(label.clone(), tab_id);
        }
        {
            let mut tabs = self.routes.tabs.lock().expect("routes lock poisoned");
            tabs.insert(
                tab_id,
                TabState {
                    thread: thread.downgrade(),
                    label,
                    pending_evals: Mutex::new(HashMap::new()),
                    pending_yield: Mutex::new(None),
                },
            );
        }
        Ok(tab_id)
    }

    fn navigate(&self, id: BrowserTabId, url: &str, cx: &mut App) -> Result<(), String> {
        let handle = crate::dispatch::window_global()
            .ok_or_else(|| "browser host: main window not available".to_string())?;
        let ws = self
            .weak_ws
            .upgrade()
            .ok_or_else(|| "browser host: workspace dropped".to_string())?;
        handle
            .update(cx, |_, window, cx| {
                ws.update(cx, |ws, cx| {
                    if let Some(view) = ws.browser_views.get(&id).cloned() {
                        view.update(cx, |v, cx| v.load_url(url, window, cx));
                    }
                })
            })
            .map_err(|e| format!("browser host: window update failed: {e}"))?;
        Ok(())
    }

    fn eval_script(
        &self,
        id: BrowserTabId,
        js: &str,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        // Embed the caller's JS as a JSON-escaped string literal (a valid JS
        // string) and indirect-eval it so it runs in global scope, returning
        // its value as the eval_result payload.
        let body = serde_json::to_string(js).unwrap_or_else(|_| "\"\"".to_string());
        self.eval_awaiting(
            id,
            move |rid| {
                format!(
                    "(function(){{try{{var payload=(0,eval)({body});window.__manox_notify__('eval_result',{{request_id:{rid},payload:payload===undefined?null:payload}});}}catch(e){{window.__manox_notify__('eval_result',{{request_id:{rid},payload:{{__error:String(e&&e.message||e)}}}});}}}})();",
                    body = body,
                    rid = rid,
                )
            },
            cx,
        )
    }

    fn read_text(&self, id: BrowserTabId, cx: &mut App) -> Task<Result<String, String>> {
        self.eval_awaiting(
            id,
            |rid| {
                format!(
                    "(function(){{try{{var t=(document.body&&document.body.innerText)||'';window.__manox_notify__('eval_result',{{request_id:{rid},payload:t}});}}catch(e){{window.__manox_notify__('eval_result',{{request_id:{rid},payload:{{__error:String(e&&e.message||e)}}}});}}}})();",
                    rid = rid,
                )
            },
            cx,
        )
    }

    fn read_dom(
        &self,
        id: BrowserTabId,
        selector: Option<String>,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        self.eval_awaiting(
            id,
            move |rid| match &selector {
                Some(sel) => {
                    let s = serde_json::to_string(sel).unwrap_or_else(|_| "\"\"".to_string());
                    format!(
                        "(function(){{try{{var el=document.querySelector({s});var html=el?el.outerHTML:'';window.__manox_notify__('eval_result',{{request_id:{rid},payload:html}});}}catch(e){{window.__manox_notify__('eval_result',{{request_id:{rid},payload:{{__error:String(e&&e.message||e)}}}});}}}})();",
                        s = s,
                        rid = rid,
                    )
                }
                None => {
                    format!(
                        "(function(){{try{{var html=document.documentElement.outerHTML;window.__manox_notify__('eval_result',{{request_id:{rid},payload:html}});}}catch(e){{window.__manox_notify__('eval_result',{{request_id:{rid},payload:{{__error:String(e&&e.message||e)}}}});}}}})();",
                        rid = rid,
                    )
                }
            },
            cx,
        )
    }

    fn click(&self, id: BrowserTabId, selector: &str, cx: &mut App) -> Task<Result<(), String>> {
        let sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string());
        let js = format!(
            "(function(){{var el=document.querySelector({sel});if(el){{el.click();}}}})();",
            sel = sel,
        );
        let res = self.inject_script(id, &js, cx);
        cx.background_spawn(async move { res })
    }

    fn type_text(
        &self,
        id: BrowserTabId,
        selector: &str,
        text: &str,
        cx: &mut App,
    ) -> Task<Result<(), String>> {
        let sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string());
        let txt = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
        let js = format!(
            "(function(){{var el=document.querySelector({sel});if(el){{el.focus();el.value={txt};el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));}}}})();",
            sel = sel,
            txt = txt,
        );
        let res = self.inject_script(id, &js, cx);
        cx.background_spawn(async move { res })
    }

    fn scroll(&self, id: BrowserTabId, dx: i32, dy: i32, cx: &mut App) -> Task<Result<(), String>> {
        let js = format!("window.scrollBy({dx},{dy});", dx = dx, dy = dy);
        let res = self.inject_script(id, &js, cx);
        cx.background_spawn(async move { res })
    }

    fn screenshot(&self, id: BrowserTabId, cx: &mut App) -> Task<Result<String, String>> {
        // A DOM snapshot of the visible state (structure + metadata), not a
        // pixel image — the agent needs page structure, and a true pixel
        // snapshot needs a platform-specific wry extension not in scope here.
        self.eval_awaiting(
            id,
            |rid| {
                format!(
                    "(function(){{try{{var snap={{viewport:{{w:window.innerWidth,h:window.innerHeight}},scroll:{{x:window.scrollX,y:window.scrollY,url:location.href}},html:document.documentElement.outerHTML}};window.__manox_notify__('eval_result',{{request_id:{rid},payload:snap}});}}catch(e){{window.__manox_notify__('eval_result',{{request_id:{rid},payload:{{__error:String(e&&e.message||e)}}}});}}}})();",
                    rid = rid,
                )
            },
            cx,
        )
    }

    fn yield_to_user(&self, id: BrowserTabId, cx: &mut App) -> Task<Result<(), String>> {
        let (tx, rx) = oneshot::channel::<()>();
        let registered = self
            .routes
            .tabs
            .lock()
            .expect("routes lock poisoned")
            .get(&id)
            .map(|tab| {
                *tab.pending_yield
                    .lock()
                    .expect("pending_yield lock poisoned") = Some(tx);
            })
            .is_some();
        if !registered {
            return cx.background_spawn(async move {
                Err(format!("browser host: no browser tab with id {id}"))
            });
        }
        cx.background_spawn(async move {
            match rx.await {
                Ok(()) => Ok(()),
                Err(_) => {
                    Err("browser host: yield was cancelled before the user handed back".to_string())
                }
            }
        })
    }

    fn close_tab(&self, id: BrowserTabId, cx: &mut App) {
        // Reclaim the routing state first so any in-flight notify for this tab
        // finds no entry and is dropped (no orphaned oneshot resolution).
        let label = self
            .routes
            .tabs
            .lock()
            .expect("routes lock poisoned")
            .remove(&id)
            .map(|t| t.label);
        if let Some(label) = label {
            self.routes
                .label_to_tab
                .lock()
                .expect("routes lock poisoned")
                .remove(&label);
        }
        if let Some(ws) = self.weak_ws.upgrade() {
            ws.update(cx, |ws, cx| ws.close_browser_tab(id, cx));
        }
    }
}
