//! About Manox window — shows app icon, version, commit SHA, and build type.
//!
//! Modeled after Zed's About dialog: centered floating window with app icon,
//! version details, and two action buttons (OK / Copy). Duplicate window
//! detection ensures only one About window is open at a time.

use std::sync::Arc;

use agent::{i18n, version};
use gpui::{prelude::*, *};
use gpui_component::{
    StyledExt as _, Theme, ThemeMode,
    button::{Button, ButtonVariants as _},
};

struct AboutWindow {
    app_icon: Arc<Image>,
    message: SharedString,
    full_version: SharedString,
    commit: Option<SharedString>,
}

impl AboutWindow {
    fn new(_: &mut Context<Self>) -> Self {
        Self {
            app_icon: Arc::new(Image::from_bytes(
                ImageFormat::Png,
                include_bytes!("../resources/app-icon.png").to_vec(),
            )),
            message: i18n::t("about-title"),
            full_version: SharedString::from(version::full_version_string()),
            commit: version::COMMIT_SHA.map(SharedString::from),
        }
    }
}

impl Render for AboutWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let muted = theme.muted_foreground;

        // Pre-compute the clipboard text so the Copy button closure can
        // capture it without needing &self.
        let copy_content: SharedString = match self.commit.as_ref() {
            Some(commit) => {
                format!(
                    "{}\nVersion: {}\nCommit: {}",
                    self.message, self.full_version, commit
                )
            }
            None => format!("{}\nVersion: {}", self.message, self.full_version),
        }
        .into();

        div()
            .v_flex()
            .size_full()
            .p_6()
            .gap_4()
            .justify_between()
            .child(
                // Top section: icon + version info.
                div()
                    .v_flex()
                    .gap_3()
                    .items_center()
                    .child(gpui::img(self.app_icon.clone()).size(px(48.)).flex_none())
                    .child(div().text_lg().font_weight(FontWeight::BOLD).child("Manox"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(self.full_version.clone()),
                    )
                    .when_some(self.commit.as_ref(), |el, commit| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(muted.opacity(0.7))
                                .child(format!("commit: {commit}")),
                        )
                    })
                    .child(
                        // Build type badge.
                        div().flex().gap_2().child(
                            div()
                                .px_2()
                                .py_0p5()
                                .rounded_md()
                                .bg(muted.opacity(0.12))
                                .text_xs()
                                .text_color(muted)
                                .child(if cfg!(debug_assertions) {
                                    "debug"
                                } else {
                                    "release"
                                }),
                        ),
                    ),
            )
            .child(
                // Bottom section: action buttons.
                div()
                    .h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Button::new("about-ok")
                            .label(i18n::t("about-ok"))
                            .ghost()
                            .w_full()
                            .on_click(|_ev, window, _cx| {
                                window.remove_window();
                            }),
                    )
                    .child(
                        Button::new("about-copy")
                            .label(i18n::t("about-copy"))
                            .primary()
                            .w_full()
                            .on_click(move |_ev, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    copy_content.to_string(),
                                ));
                                window.remove_window();
                            }),
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
    let bounds = WindowBounds::centered(size(px(380.), px(300.)), cx);

    cx.spawn(async move |cx| {
        let options = WindowOptions {
            window_bounds: Some(bounds),
            is_resizable: false,
            ..Default::default()
        };
        let _handle = cx
            .open_window(options, |window, cx| {
                window.set_window_title(i18n::t("about-title").as_ref());
                Theme::change(ThemeMode::Light, Some(window), cx);
                cx.new(AboutWindow::new)
            })
            .expect("failed to open about window");
    })
    .detach();
}
