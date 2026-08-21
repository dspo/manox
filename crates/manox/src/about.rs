//! About Manox window — shows app icon, version, commit SHA, and build type.
//!
//! Modeled after Zed's About dialog: centered floating window with app icon,
//! version details, and an OK button. A ghost icon button in the version row
//! copies a structured info block (`version::structured_about`) to the
//! clipboard. Duplicate window detection ensures only one About window is
//! open at a time.

use std::sync::Arc;
use std::time::Duration;

use agent::{i18n, version};
use gpui::{prelude::*, *};
use gpui_component::{
    IconName, Sizable as _, StyledExt as _, Theme, ThemeMode,
    button::{Button, ButtonVariants as _},
};

/// How long the copy button shows the "Copied" feedback state.
const COPY_FEEDBACK: Duration = Duration::from_millis(1500);

struct AboutWindow {
    app_icon: Arc<Image>,
    full_version: SharedString,
    commit: Option<SharedString>,
    copied: bool,
}

impl AboutWindow {
    fn new(_: &mut Context<Self>) -> Self {
        Self {
            app_icon: Arc::new(Image::from_bytes(
                ImageFormat::Png,
                include_bytes!("../resources/app-icon.png").to_vec(),
            )),
            full_version: SharedString::from(version::full_version_string()),
            commit: version::COMMIT_SHA.map(SharedString::from),
            copied: false,
        }
    }
}

impl Render for AboutWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let muted = theme.muted_foreground;

        let entity = cx.entity();
        let copy_button = if self.copied {
            Button::new("about-copy")
                .ghost()
                .xsmall()
                .icon(IconName::Check)
                .label(i18n::t("about-copied"))
        } else {
            Button::new("about-copy")
                .ghost()
                .xsmall()
                .icon(IconName::Copy)
        }
        .on_click(move |_ev, _window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(version::structured_about()));
            let entity = entity.clone();
            entity.update(cx, |this, cx| {
                this.copied = true;
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(COPY_FEEDBACK).await;
                    let _ = this.update(cx, |this, _| this.copied = false);
                })
                .detach();
            });
        });

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
                            .h_flex()
                            .gap_1()
                            .items_center()
                            .text_sm()
                            .text_color(muted)
                            .child(self.full_version.clone())
                            .child(copy_button),
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
                // Bottom section: OK button.
                div().h_flex().w_full().gap_2().child(
                    Button::new("about-ok")
                        .label(i18n::t("about-ok"))
                        .ghost()
                        .w_full()
                        .on_click(|_ev, window, _cx| {
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
