//! About window: centered floating dialog with the app icon, a headline
//! version line, muted-label commit/version rows, and an OK / Copy action
//! row. Copy writes the structured info block (`version::structured_about`)
//! to the clipboard and closes the window; Escape closes as well. Duplicate
//! window detection keeps a single instance.

use std::sync::Arc;

use gpui::{prelude::*, *};
use gpui_component::{
    StyledExt as _, Theme,
    button::{Button, ButtonVariants as _},
};
use manox_agent::{i18n, version};

struct AboutWindow {
    focus_handle: FocusHandle,
    app_icon: Arc<Image>,
    message: SharedString,
    commit: Option<SharedString>,
    full_version: SharedString,
}

impl AboutWindow {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            app_icon: Arc::new(Image::from_bytes(
                ImageFormat::Png,
                include_bytes!("../resources/app-icon.png").to_vec(),
            )),
            message: SharedString::from(format!(
                "Manox {} ({})",
                version::PKG_VERSION,
                version::build_type()
            )),
            commit: version::COMMIT_SHA.map(SharedString::from),
            full_version: SharedString::from(version::full_version_string()),
        }
    }

    fn copy_details(&self, window: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(version::structured_about()));
        window.remove_window();
    }
}

impl Render for AboutWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let muted = theme.muted_foreground;

        div()
            .id("about-window")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|_, ev: &KeyDownEvent, window, _cx| {
                if ev.keystroke.key == "escape" {
                    window.remove_window();
                }
            }))
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .p_4()
            .pt_10()
            .gap_4()
            .text_center()
            .justify_between()
            .child(
                div()
                    .v_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(gpui::img(self.app_icon.clone()).size_16().flex_none())
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.message.clone()),
                    )
                    .when_some(self.commit.clone(), |this, commit| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(i18n::t("about-commit")),
                        )
                        .child(div().text_sm().child(commit))
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(i18n::t("about-version")),
                    )
                    .child(div().text_sm().child(self.full_version.clone())),
            )
            .child(
                div()
                    .h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Button::new("about-ok")
                            .label(i18n::t("about-ok"))
                            .ghost()
                            .flex_1()
                            .on_click(|_ev, window, _cx| {
                                window.remove_window();
                            }),
                    )
                    .child(
                        Button::new("about-copy")
                            .label(i18n::t("about-copy"))
                            .primary()
                            .flex_1()
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.copy_details(window, cx);
                            })),
                    ),
            )
    }
}

pub fn open_about_window(cx: &mut App) {
    // Don't open a second About window.
    if let Some(existing) = cx
        .windows()
        .into_iter()
        .find_map(|w| w.downcast::<AboutWindow>())
    {
        let _ = existing.update(cx, |_, window, _cx| {
            window.activate_window();
        });
        return;
    }

    // Compute bounds before spawning so we can use &App.
    let bounds = WindowBounds::centered(size(px(440.), px(300.)), cx);

    cx.spawn(async move |cx| {
        let options = WindowOptions {
            window_bounds: Some(bounds),
            is_resizable: false,
            is_minimizable: false,
            kind: WindowKind::Floating,
            ..Default::default()
        };
        let _handle = cx
            .open_window(options, |window, cx| {
                window.set_window_title(i18n::t("about-title").as_str());
                let about = cx.new(AboutWindow::new);
                let focus = about.read(cx).focus_handle.clone();
                window.focus(&focus, cx);
                about
            })
            .expect("failed to open about window");
    })
    .detach();
}
