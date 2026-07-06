//! manox — an in-process native agent workbench (thin bin).
//!
//! Only handles window, theme, and tracing init, and mounts `agent_ui::Workspace` in the window.
//! Agent logic lives in the `agent` crate; UI lives in the `agent-ui` crate.

use gpui::{App, AppContext as _, Menu, MenuItem, actions, px, size};
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
        agent::init(cx);
        terminal::init(cx);
        terminal_ui::init(cx);
        agent_ui::slash_command::init(cx);

        cx.bind_keys([
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-q", Quit, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("alt-f4", Quit, None),
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-ctrl-f", ToggleFullscreen, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("f11", ToggleFullscreen, None),
            // Ctrl-G opens the right-side markdown composer.
            gpui::KeyBinding::new("ctrl-g", agent_ui::ToggleEditor, None),
            // Cmd/Ctrl-W closes the markdown composer and returns the draft to the inline input.
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-w", agent_ui::CloseEditor, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("ctrl-w", agent_ui::CloseEditor, None),
            // Cmd/Ctrl-Shift-P toggles between plain-text edit and markdown preview.
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-shift-p", agent_ui::ToggleEditorPreview, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("ctrl-shift-p", agent_ui::ToggleEditorPreview, None),
            // Cmd/Ctrl-, opens the Settings overlay. The handler lives on the
            // active Workspace (see `Workspace::Render`), so menu items and
            // keybindings both reach the same `cx.listener` once the window
            // is focused.
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-,", agent_ui::OpenSettings, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("ctrl-,", agent_ui::OpenSettings, None),
            // Ask drawer: left/right arrows navigate questions, escape closes.
            gpui::KeyBinding::new("left", agent_ui::AskPrev, Some("AskDrawer")),
            gpui::KeyBinding::new("right", agent_ui::AskNext, Some("AskDrawer")),
            gpui::KeyBinding::new("escape", agent_ui::AskCancel, Some("AskDrawer")),
        ]);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.on_action(|_: &ToggleFullscreen, cx: &mut App| {
            if let Some(handle) = cx.active_window() {
                let _ = handle.update(cx, |_, window, _| window.toggle_fullscreen());
            }
        });
        // macOS system menu items are evaluated against App-level on_action
        // handlers, not the view tree's local on_action listeners. Registering
        // the dispatch here keeps Settings… enabled when the Workspace is the
        // root view of the active window.
        cx.on_action(|_: &agent_ui::OpenSettings, cx: &mut App| {
            // The dispatch is deferred to the next effect cycle. The global
            // action listener fires from inside `update_window_id`, which has
            // already taken the window's slot out of `cx.windows`; calling
            // `handle.update(cx, ...)` synchronously here would re-enter that
            // path on a `None` slot and surface as `Err(window not found)`.
            // `cx.defer` queues the work so it runs once the slot has been
            // put back.
            let workspace = agent_ui::dispatch::workspace_global();
            let handle = agent_ui::dispatch::window_global();
            cx.defer(move |cx| {
                if let (Some(workspace), Some(handle)) = (workspace, handle) {
                    let _ = handle.update(cx, |_, window, cx| {
                        workspace.update(cx, |ws, cx| ws.enter_settings(window, cx));
                    });
                }
            });
        });

        cx.set_menus(build_app_menus());

        let window_options = WindowOptions {
            titlebar: Some(TitleBar::title_bar_options()),
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(760.)), cx)),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            let handle = cx
                .open_window(window_options, |window, cx| {
                    window.activate_window();
                    window.set_window_title("manox");
                    Theme::change(ThemeMode::Light, Some(window), cx);

                    let view = cx.new(|cx| agent_ui::Workspace::new(window, cx));
                    agent_ui::dispatch::set_workspace(view.clone());
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("failed to open window");
            agent_ui::dispatch::set_window(handle);
        })
        .detach();
    });
}

fn build_app_menus() -> Vec<Menu> {
    // macOS native menus render the app-name menu regardless of the label here,
    // so the first Menu's title is ignored on mac; the File/Settings/Quit labels
    // are localized for the user's chosen UI language.
    #[cfg(target_os = "macos")]
    {
        vec![Menu::new("manox").items([
            MenuItem::separator(),
            MenuItem::action(agent::i18n::t("menu-settings"), agent_ui::OpenSettings),
            MenuItem::separator(),
            MenuItem::action(agent::i18n::t("menu-quit"), Quit),
        ])]
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec![Menu::new(agent::i18n::t("menu-file")).items([
            MenuItem::action(agent::i18n::t("menu-settings"), agent_ui::OpenSettings),
            MenuItem::separator(),
            MenuItem::action(agent::i18n::t("menu-quit"), Quit),
        ])]
    }
}
