//! `BrowserView` — a right-pane tab hosting an untrusted embedded webview.
//!
//! Built with `TrustMode::Untrusted`: only the closed-enum notify bridge and
//! the inbound-write request bridge are injected; the page has no Tauri
//! command surface. The chrome (back / forward / address bar) is pure GPUI;
//! the content area is the native `WebViewElement`. The address bar doubles
//! as reload — re-submitting the current URL re-navigates.

use std::sync::atomic::{AtomicU64, Ordering};

use agent::webview_host::BrowserTabId;
use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, Styled as _, Subscription, Window, div,
};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
};

/// URL a browser tab loads when opened without one (e.g. via the keybinding).
/// A stable, offline-tolerant page so the tab is immediately useful without
/// depending on network state.
pub const DEFAULT_URL: &str = "https://example.com";

/// Process-wide counter allocating unique `BrowserTabId`s. The id is woven
/// into the webview label (`browser-tab-{id}`) so the host can route inbound
/// notifications back to the originating tab.
static NEXT_TAB_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a process-unique `BrowserTabId`.
pub fn allocate_tab_id() -> BrowserTabId {
    NEXT_TAB_ID.fetch_add(1, Ordering::Relaxed)
}

/// The webview label registered for IPC routing. Untrusted webviews share a
/// single process-wide notify/inbound handler (set by the host in a later
/// step); the label lets the host dispatch a notification back to its tab.
pub fn webview_label_for(tab_id: BrowserTabId) -> String {
    format!("browser-tab-{tab_id}")
}

pub struct BrowserView {
    tab_id: BrowserTabId,
    webview: Entity<manox_webview::webview::WebView>,
    address: Entity<InputState>,
    url: String,
    _input_sub: Subscription,
}

impl BrowserView {
    /// Build an untrusted webview parented to `window` and wrap it + its
    /// address bar in a gpui entity. `tab_id` must be process-unique; obtain
    /// it from [`allocate_tab_id`].
    pub fn new(
        tab_id: BrowserTabId,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let label = webview_label_for(tab_id);
        let builder = manox_webview::Builder::default()
            .with_webview_id(label.as_str())
            .trust_mode(manox_webview::TrustMode::Untrusted);
        // Attach the process-wide notify/inbound bridges. The host is the
        // single owner of routing; the webview crate's OnceLock keeps the
        // first-attached closures, so every BrowserView attaches the same
        // host-owned closures (a later open never finds a stale handler).
        let builder = crate::browser_host::WorkspaceBrowserHost::attach_to_builder(builder);
        let wry = builder
            .apply(|b| b.with_url(url))
            .build_as_child(window)
            .expect("manox-webview: build_as_child failed");
        let webview = cx.new(|cx| manox_webview::webview::WebView::new(wry, window, cx));

        let address = cx.new(|cx| {
            InputState::new(window, cx).placeholder(agent::i18n::t("browser-address-placeholder"))
        });
        address.update(cx, |s, cx| s.set_value(url, window, cx));

        // Enter in the address bar navigates — re-submitting the current
        // value reloads the page. Single-line input: shift is ignored.
        let _input_sub = cx.subscribe_in(
            &address,
            window,
            move |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = ev {
                    let url = this.address.read(cx).value().to_string();
                    if !url.is_empty() {
                        this.load_url(&url, window, cx);
                    }
                }
            },
        );

        Self {
            tab_id,
            webview,
            address,
            url: url.to_string(),
            _input_sub,
        }
    }

    pub fn tab_id(&self) -> BrowserTabId {
        self.tab_id
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The underlying webview entity, for host-driven eval/navigation.
    pub fn webview(&self) -> &Entity<manox_webview::webview::WebView> {
        &self.webview
    }

    /// Navigate to `url` and sync the address bar.
    pub fn load_url(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.url = url.to_string();
        self.address
            .update(cx, |s, cx| s.set_value(url, window, cx));
        self.webview.update(cx, |wv, _| wv.load_url(url));
    }

    /// Go back in the webview history.
    pub fn back(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.webview.update(cx, |wv, _| wv.back());
    }

    /// Go forward in the webview history.
    pub fn forward(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.webview.update(cx, |wv, _| wv.forward());
    }
}

impl Render for BrowserView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .child(
                div()
                    .id("browser-chrome")
                    .w_full()
                    .h_8()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("browser-back")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ArrowLeft)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.back(window, cx);
                            })),
                    )
                    .child(
                        Button::new("browser-forward")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ArrowRight)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.forward(window, cx);
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.address).appearance(false)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.webview.clone()),
            )
    }
}
