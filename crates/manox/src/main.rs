//! manox — an in-process native agent workbench (thin bin).
//!
//! Only handles window, theme, and tracing init, and mounts `agent_ui::Workspace` in the window.
//! Agent logic lives in the `agent` crate; UI lives in the `agent-ui` crate.

use gpui::{App, AppContext as _, actions, px, size};
use gpui::{WindowBounds, WindowOptions};
use gpui_component::{Root, Theme, ThemeMode, TitleBar};

actions!(manox, [Quit, ToggleFullscreen]);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    app.run(move |cx| {
        gpui_component::init(cx);
        gpui_rich_text::init(cx);
        agent::init(cx);

        cx.bind_keys([
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-q", Quit, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("alt-f4", Quit, None),
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-ctrl-f", ToggleFullscreen, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("f11", ToggleFullscreen, None),
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-shift-e", agent_ui::ToggleEditor, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("ctrl-shift-e", agent_ui::ToggleEditor, None),
        ]);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.on_action(|_: &ToggleFullscreen, cx: &mut App| {
            if let Some(handle) = cx.active_window() {
                let _ = handle.update(cx, |_, window, _| window.toggle_fullscreen());
            }
        });

        let window_options = WindowOptions {
            titlebar: Some(TitleBar::title_bar_options()),
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(760.)), cx)),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(window_options, |window, cx| {
                window.activate_window();
                window.set_window_title("manox");
                Theme::change(ThemeMode::Light, Some(window), cx);

                let view = cx.new(|cx| agent_ui::Workspace::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}
