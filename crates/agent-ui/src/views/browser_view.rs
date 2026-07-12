//! `BrowserView` — a right-pane tab hosting an embedded native webview.
//!
//! Step 1 (go/no-go): a minimal skeleton that builds a wry `WebView` via
//! `manox_webview::Builder::build_as_child` and renders the `WebView` entity.
//! Step 5 fills in the address bar, navigation chrome, trust-mode wiring, and
//! tab lifecycle; this skeleton exists only to prove the embed works against
//! manox's gpui revision at runtime.

use gpui::{
    AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, Window,
    div,
};
use manox_webview::{Builder, webview::WebView};

/// URL the go/no-go skeleton loads when opened from the keybinding. A stable,
/// offline-tolerant page so the embed check doesn't depend on network state.
const GONOGO_URL: &str = "https://example.com";

/// The webview label registered for IPC routing. The go/no-go skeleton performs
/// no IPC, but `build_as_child` rejects an empty id, so a stable non-empty
/// label is mandatory.
const GONOGO_WEBVIEW_ID: &str = "browser-go-nogo";

pub struct BrowserView {
    webview: Entity<WebView>,
}

impl BrowserView {
    /// Build a webview parented to `window` and wrap it in a gpui entity.
    pub fn new(url: Option<&str>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let url = url.unwrap_or(GONOGO_URL);
        let wry = Builder::default()
            .with_webview_id(GONOGO_WEBVIEW_ID)
            .apply(|b| b.with_url(url))
            .build_as_child(window)
            .expect("manox-webview: build_as_child failed");
        let webview = cx.new(|cx| WebView::new(wry, window, cx));
        Self { webview }
    }

    /// Forward a navigation to the underlying webview.
    pub fn load_url(&mut self, url: &str, _window: &mut Window, cx: &mut Context<Self>) {
        self.webview.update(cx, |wv, _| wv.load_url(url));
    }

    pub fn webview(&self) -> &Entity<WebView> {
        &self.webview
    }
}

impl Render for BrowserView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.webview.clone())
    }
}
