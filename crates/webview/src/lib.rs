//! manox-webview — wry WebView embedded as a native child of a gpui window.
//!
//! Step 1 (go/no-go): only the embed mechanics are present. A native WKWebView /
//! WebView2 / WebKitGTK view is parented to the gpui window via wry's
//! `build_as_child`, and a gpui `Element` (`WebViewElement`) positions it by
//! forwarding computed layout bounds to `WebView::set_bounds`. The Tauri-style
//! IPC layer, plugins, and init scripts are ported in step 2.

pub mod webview;

pub use webview::{WebView, WebViewElement};

use std::rc::Rc;
use wry::{WebView as WryWebView, WebViewBuilder, WebViewId};

/// Minimal builder for the go/no-go embed check. Step 2 replaces this with the
/// full manos `Builder` (init scripts, IPC handlers, custom protocols, plugins).
pub struct Builder<'a> {
    builder: WebViewBuilder<'a>,
    webview_id: WebViewId<'a>,
    url: Option<String>,
}

impl<'a> Builder<'a> {
    pub fn new() -> Self {
        Self {
            builder: WebViewBuilder::new(),
            webview_id: WebViewId::default(),
            url: None,
        }
    }

    pub fn with_webview_id(mut self, webview_id: WebViewId<'a>) -> Self {
        self.webview_id = webview_id;
        self
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Parent a native webview to the gpui window's raw window handle.
    pub fn build_as_child(mut self, window: &mut gpui::Window) -> anyhow::Result<WryWebView> {
        use raw_window_handle::HasWindowHandle;

        if self.webview_id.is_empty() {
            self.webview_id = WebViewId::from("manox-webview");
        }
        let window_handle = window.window_handle()?;
        let builder = self.builder.with_id(self.webview_id);
        let builder = match self.url.take() {
            Some(url) => builder.with_url(url),
            None => builder,
        };
        Ok(builder.build_as_child(&window_handle)?)
    }
}

impl<'a> Default for Builder<'a> {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrap a freshly-built wry `WebView` into a gpui `WebView` entity.
///
/// Convenience kept as a free function so callers (e.g. a `BrowserView`) can
/// construct the entity without reaching into `Builder` internals.
pub fn into_entity(
    webview: WryWebView,
    window: &mut gpui::Window,
    cx: &mut gpui::App,
) -> Rc<WryWebView> {
    let _ = window; // window handle was consumed at build time; kept for API symmetry
    let _ = cx;
    Rc::new(webview)
}
