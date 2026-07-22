//! About Manox window — shows version, commit SHA, and build type.

use agent::{i18n, version};
use gpui::{prelude::*, *};
use gpui_component::{
    StyledExt as _, Theme, ThemeMode,
    button::{Button, ButtonVariants as _},
};

struct AboutWindow {
    version: SharedString,
    commit: Option<SharedString>,
}

impl AboutWindow {
    fn new(_: &mut Context<Self>) -> Self {
        Self {
            version: SharedString::from(version::full_version_string()),
            commit: version::COMMIT_SHA.map(SharedString::from),
        }
    }
}

impl Render for AboutWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let muted = theme.muted_foreground;

        div()
            .v_flex()
            .size_full()
            .p_6()
            .gap_4()
            .child(
                // App name.
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .child("Manox"),
            )
            .child(
                // Version string.
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(self.version.clone()),
            )
            .when_some(self.commit.as_ref(), |el, commit| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(muted.opacity(0.6))
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
            )
            .child(div().flex_1())
            .child(
                Button::new("about-ok")
                    .label(i18n::t("about-ok"))
                    .primary()
                    .w_full()
                    .on_click(|_ev, window, _cx| {
                        window.remove_window();
                    }),
            )
    }
}

pub fn open_about_window(cx: &mut App) {
    cx.spawn(async move |cx| {
        let options = WindowOptions {
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
