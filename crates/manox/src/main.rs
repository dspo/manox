//! manox — an in-process native agent workbench (thin bin).
//!
//! Only handles window, theme, and tracing init, and mounts `agent_ui::Workspace` in the window.
//! Agent logic lives in the `agent` crate; UI lives in the `agent-ui` crate.
//!
//! With the `debug` feature, `--mcp` keeps the same visible window and
//! additionally serves the debug Harness over stdio JSON-RPC (see
//! `mcp_server.rs`), so an external agent can drive the UI programmatically.

#[cfg(feature = "debug")]
mod mcp_server;

use gpui::{App, AppContext as _, Menu, MenuItem, actions, px, size};
use gpui::{WindowBounds, WindowOptions};
use gpui_component::{Root, Theme, ThemeMode, TitleBar};
use std::borrow::Cow;

actions!(manox, [Quit, ToggleFullscreen]);

/// Minimum window width budget, left to right:
/// sidebar (260) + sidebar divider (6) + a readable conversation column
/// (~594). The context rail is a flex sibling of the conversation column
/// (not an overlay), and folds into a drawer below `RAIL_NARROW_BREAK` in
/// agent-ui, so it never constrains the minimum window width.
const MIN_WINDOW_W: f32 = 860.0;

/// Minimum window height: title bar + several message lines + composer +
/// footer hairline, with breathing room.
const MIN_WINDOW_H: f32 = 520.0;

fn main() {
    // Install a panic hook before anything else so a panic anywhere — main
    // thread, gpui foreground task, or a detached tokio worker — surfaces with
    // location and a forced backtrace. Without this a panic in a spawned task
    // prints a terse one-liner and the process disappears, leaving no clue for
    // crash diagnosis.
    // Edition 2024 marks `set_var` unsafe because it can race with reads on
    // other threads; safe here because this runs before any other thread exists.
    unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("<non-string panic payload>");
        let bt = std::backtrace::Backtrace::force_capture();
        let msg = format!(
            "panic: {payload}\n  location: {location}\n  thread: {:?}\n{bt}",
            std::thread::current().name().unwrap_or("<unnamed>")
        );
        eprintln!("{msg}");
        tracing::error!("{msg}");
    }));

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_thread_ids(true)
        .with_thread_names(true)
        .init();

    let app = gpui_platform::application().with_assets(agent_ui::assets::ExtrasAssetSource::new());

    app.run(move |cx| {
        gpui_component::init(cx);
        agent::init(cx);
        terminal::init(cx);
        terminal_ui::init(cx);
        agent_ui::slash_command::init(cx);

        // Embedded OFL typefaces. Lilex ships only Light/Medium in upright and
        // italic cuts: message body inherits Light, markdown bold/headings and
        // tool-call titles resolve to Medium via nearest-weight matching, italic
        // syntax plus reasoning/tool cards hit the italic cuts. IBM Plex Mono
        // stays the UI-chrome family (sidebar, buttons, settings, menus) at its
        // full weight range.
        // Both are registered before any view renders so the first frame already
        // resolves to the embedded faces rather than a system fallback.
        cx.text_system()
            .add_fonts(vec![
                Cow::Borrowed(include_bytes!("../assets/fonts/lilex/Lilex-Light.ttf")),
                Cow::Borrowed(include_bytes!("../assets/fonts/lilex/Lilex-Medium.ttf")),
                Cow::Borrowed(include_bytes!(
                    "../assets/fonts/lilex/Lilex-LightItalic.ttf"
                )),
                Cow::Borrowed(include_bytes!(
                    "../assets/fonts/lilex/Lilex-MediumItalic.ttf"
                )),
                Cow::Borrowed(include_bytes!(
                    "../assets/fonts/ibm-plex-mono/IBMPlexMono-Regular.ttf"
                )),
                Cow::Borrowed(include_bytes!(
                    "../assets/fonts/ibm-plex-mono/IBMPlexMono-Bold.ttf"
                )),
                Cow::Borrowed(include_bytes!(
                    "../assets/fonts/ibm-plex-mono/IBMPlexMono-Italic.ttf"
                )),
                Cow::Borrowed(include_bytes!(
                    "../assets/fonts/ibm-plex-mono/IBMPlexMono-BoldItalic.ttf"
                )),
            ])
            .expect("failed to register embedded fonts");

        // Lilex is the family name embedded in the TTFs (not "Lilex Mono").
        {
            let theme = Theme::global_mut(cx);
            theme.font_family = "IBM Plex Mono".into();
            theme.mono_font_family = "Lilex".into();
            theme.font_size = px(14.);
            theme.mono_font_size = px(14.);
        }

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
            // Terminal tab: cmd-t opens, cmd-shift-t focuses, cmd-shift-c
            // returns to the conversation pane. Handlers live on the active
            // Workspace (see `Workspace::Render`).
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-t", agent_ui::NewTerminalTab, None),
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-shift-t", agent_ui::FocusTerminal, None),
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-shift-c", agent_ui::FocusConversation, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("ctrl-t", agent_ui::NewTerminalTab, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("ctrl-shift-t", agent_ui::FocusTerminal, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("ctrl-shift-c", agent_ui::FocusConversation, None),
            // Built-in browser. cmd-b opens a new browser tab in the right
            // pane, cmd-shift-b closes the active browser tab.
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-b", agent_ui::OpenBrowserTab, None),
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-shift-b", agent_ui::CloseBrowserTab, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("ctrl-alt-b", agent_ui::OpenBrowserTab, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("ctrl-shift-b", agent_ui::CloseBrowserTab, None),
            // Park the active running thread into the background and open a
            // fresh empty thread in the same project — the explicit "background
            // this task" gesture. No-op when idle. cmd-b stays the browser key
            // on macOS, so ctrl-b is free there; on other platforms the browser
            // tab moved to ctrl-alt-b to free ctrl-b for this action.
            gpui::KeyBinding::new("ctrl-b", agent_ui::BackgroundCurrentThread, None),
            // Pop the last follow-up parked above the composer while a turn is
            // running (mirrors the per-item Remove affordance for the tail).
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-alt-/", agent_ui::UndoLastQueued, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("ctrl-alt-/", agent_ui::UndoLastQueued, None),
            // Cockpit milestone panel: cmd/ctrl-shift-m collapses or expands
            // the plan-steps section in the "Conversation info" card. The
            // header is also clickable; this is the keyboard affordance.
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-shift-m", agent_ui::ToggleCockpitTasks, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("ctrl-shift-m", agent_ui::ToggleCockpitTasks, None),
            // Completion popover (driven while the composer Input is focused and
            // a `/` or `@` trigger token is active). The Descendant predicate
            // `completion == open > Input` matches at the same depth as the
            // Input's own bindings; since these are registered after
            // `gpui_component::init` they win the tie and shadow up/down/enter/
            // tab/escape to navigate the popover instead. When the popover is
            // closed the ancestor sets no `completion = open` context, so the
            // predicate fails and the Input's bindings apply normally.
            gpui::KeyBinding::new(
                "up",
                agent_ui::CompletionUp,
                Some("completion == open > Input"),
            ),
            gpui::KeyBinding::new(
                "down",
                agent_ui::CompletionDown,
                Some("completion == open > Input"),
            ),
            gpui::KeyBinding::new(
                "enter",
                agent_ui::CompletionConfirm,
                Some("completion == open > Input"),
            ),
            gpui::KeyBinding::new(
                "tab",
                agent_ui::CompletionConfirm,
                Some("completion == open > Input"),
            ),
            gpui::KeyBinding::new(
                "escape",
                agent_ui::CompletionDismiss,
                Some("completion == open > Input"),
            ),
            // Cycle the collaboration mode (Default ↔ Plan). Mirrors `/plan`,
            // the `+` menu row, and the composer mode chip. The handler lives
            // on the active Workspace (see `Workspace::Render`).
            gpui::KeyBinding::new("shift-tab", agent_ui::CycleCollaborationMode, None),
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
        // Terminal actions share the same deferred-dispatch path as Settings:
        // menu items fire App-level handlers, which reach the active window's
        // Workspace via the stashed handles.
        cx.on_action(|_: &agent_ui::NewTerminalTab, cx: &mut App| {
            let (workspace, handle) = (
                agent_ui::dispatch::workspace_global(),
                agent_ui::dispatch::window_global(),
            );
            cx.defer(move |cx| {
                if let (Some(workspace), Some(handle)) = (workspace, handle) {
                    let _ = handle.update(cx, |_, _window, cx| {
                        workspace.update(cx, |ws, cx| ws.open_terminal_tab(cx));
                    });
                }
            });
        });
        cx.on_action(|_: &agent_ui::FocusTerminal, cx: &mut App| {
            let (workspace, handle) = (
                agent_ui::dispatch::workspace_global(),
                agent_ui::dispatch::window_global(),
            );
            cx.defer(move |cx| {
                if let (Some(workspace), Some(handle)) = (workspace, handle) {
                    let _ = handle.update(cx, |_, _window, cx| {
                        workspace.update(cx, |ws, cx| ws.focus_terminal(cx));
                    });
                }
            });
        });
        cx.on_action(|_: &agent_ui::FocusConversation, cx: &mut App| {
            let (workspace, handle) = (
                agent_ui::dispatch::workspace_global(),
                agent_ui::dispatch::window_global(),
            );
            cx.defer(move |cx| {
                if let (Some(workspace), Some(handle)) = (workspace, handle) {
                    let _ = handle.update(cx, |_, _window, cx| {
                        workspace.update(cx, |ws, cx| ws.focus_conversation(cx));
                    });
                }
            });
        });
        cx.on_action(|_: &agent_ui::CloseTerminalTab, cx: &mut App| {
            let (workspace, handle) = (
                agent_ui::dispatch::workspace_global(),
                agent_ui::dispatch::window_global(),
            );
            cx.defer(move |cx| {
                if let (Some(workspace), Some(handle)) = (workspace, handle) {
                    let _ = handle.update(cx, |_, _window, cx| {
                        workspace.update(cx, |ws, cx| ws.close_terminal_tab(cx));
                    });
                }
            });
        });
        cx.on_action(|_: &agent_ui::OpenBrowserTab, cx: &mut App| {
            let (workspace, handle) = (
                agent_ui::dispatch::workspace_global(),
                agent_ui::dispatch::window_global(),
            );
            cx.defer(move |cx| {
                if let (Some(workspace), Some(handle)) = (workspace, handle) {
                    let _ = handle.update(cx, |_, window, cx| {
                        workspace.update(cx, |ws, cx| {
                            ws.open_browser_tab(
                                agent_ui::views::browser_view::DEFAULT_URL,
                                window,
                                cx,
                            );
                        });
                    });
                }
            });
        });
        cx.on_action(|_: &agent_ui::CloseBrowserTab, cx: &mut App| {
            let (workspace, handle) = (
                agent_ui::dispatch::workspace_global(),
                agent_ui::dispatch::window_global(),
            );
            cx.defer(move |cx| {
                if let (Some(workspace), Some(handle)) = (workspace, handle) {
                    let _ = handle.update(cx, |_, _window, cx| {
                        workspace.update(cx, |ws, cx| ws.close_active_browser_tab(cx));
                    });
                }
            });
        });

        cx.set_menus(build_app_menus());

        let window_options = WindowOptions {
            titlebar: Some(TitleBar::title_bar_options()),
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(760.)), cx)),
            // Floor below which the window can no longer shrink. Shared with
            // Settings so both views respect one minimum width.
            window_min_size: Some(size(px(MIN_WINDOW_W), px(MIN_WINDOW_H))),
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

            // Wire the process-wide browser host: bind it to the main
            // Workspace, register it in both the agent trait registry (so the
            // `web_explore_*` tools reach it via `agent::webview_host::host()`)
            // and the agent-ui concrete registry (so `BrowserView` attaches the
            // notify/inbound bridges at build), then spawn the notify/inbound
            // drainer on the Workspace — a notify ships through the OnceLock
            // closure with no `&mut App`, so the drainer (which owns an
            // `AsyncApp`) is the cx-bearing sink that emits onto the owning
            // thread.
            if let Some(workspace) = agent_ui::dispatch::workspace_global() {
                agent_ui::browser_host::WorkspaceBrowserHost::install(workspace.clone(), cx);
            }

            // MCP mode: serve the debug Harness over stdio on the tokio runtime,
            // bridged to the gpui-side dispatcher via an async_channel. The
            // window stays visible. When stdio closes (the agent disconnected),
            // `serve_server` returns, the sender drops, the dispatcher's
            // `rx.recv()` errors out, and the dispatcher calls `cx.quit()`.
            // Compiled only under `--features debug`.
            #[cfg(feature = "debug")]
            if std::env::args().any(|a| a == "--mcp") {
                let Some(workspace) = agent_ui::dispatch::workspace_global() else {
                    return;
                };
                let (tx, rx) = async_channel::bounded::<agent_ui::harness::bridge::McpRequest>(64);
                cx.update(|cx| agent_ui::harness::bridge::spawn_dispatcher(cx, rx, workspace));
                let server = mcp_server::ManoxMcpServer::new(tx);
                agent::runtime::handle().spawn(async move {
                    let (stdin, stdout) = rmcp::transport::stdio();
                    let _ = rmcp::serve_server(server, (stdin, stdout)).await;
                });
            }
        })
        .detach();
    });

    // The app has quit (gpui torn down) but the process has not exited yet —
    // the forgotten tokio runtime (`runtime::init` `mem::forget`s it) still owns
    // its worker threads. Reap every third-party process manox spawned (LSP/MCP
    // servers) so they don't outlive manox and get reparented to init as
    // orphans. Prefer the graceful path — LSP servers get their `shutdown`/
    // `exit` handshake, then SIGTERM, then SIGKILL — each bounded by the
    // supervisor's per-process timeouts. The main thread is not a tokio worker
    // (gpui's `run` returned here), so `Handle::block_on` is safe. Only manox's
    // own children are signaled — a server the user ran elsewhere is untouched.
    match agent::runtime::try_handle() {
        Some(handle) => handle.block_on(supervisor::global().shutdown_all()),
        None => supervisor::global().terminate_all(),
    }
}

fn build_app_menus() -> Vec<Menu> {
    // macOS native menus render the app-name menu regardless of the label here,
    // so the first Menu's title is ignored on mac; the File/Settings/Quit labels
    // are localized for the user's chosen UI language.
    #[cfg(target_os = "macos")]
    {
        vec![
            Menu::new("manox").items([
                MenuItem::separator(),
                MenuItem::action(agent::i18n::t("menu-settings"), agent_ui::OpenSettings),
                MenuItem::separator(),
                MenuItem::action(agent::i18n::t("menu-quit"), Quit),
            ]),
            Menu::new(agent::i18n::t("menu-terminal")).items([
                MenuItem::action(
                    agent::i18n::t("menu-new-terminal"),
                    agent_ui::NewTerminalTab,
                ),
                MenuItem::action(
                    agent::i18n::t("menu-close-terminal"),
                    agent_ui::CloseTerminalTab,
                ),
            ]),
        ]
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec![Menu::new(agent::i18n::t("menu-file")).items([
            MenuItem::action(agent::i18n::t("menu-settings"), agent_ui::OpenSettings),
            MenuItem::separator(),
            MenuItem::action(
                agent::i18n::t("menu-new-terminal"),
                agent_ui::NewTerminalTab,
            ),
            MenuItem::action(
                agent::i18n::t("menu-close-terminal"),
                agent_ui::CloseTerminalTab,
            ),
            MenuItem::separator(),
            MenuItem::action(agent::i18n::t("menu-quit"), Quit),
        ])]
    }
}
