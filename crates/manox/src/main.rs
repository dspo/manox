//! manox — an in-process native agent workbench (thin bin).
//!
//! Only handles window, theme, and tracing init, and mounts `agent_ui::Workspace` in the window.
//! Agent logic lives in the `agent` crate; UI lives in the `agent-ui` crate.

use gpui::{App, AppContext as _, Menu, MenuItem, QuitMode, WindowHandle, actions, px, size};
use gpui::{WindowBounds, WindowOptions};
use gpui_component::{Root, Theme, ThemeMode, TitleBar};
use std::borrow::Cow;

mod about;
mod tray;

// The harness backend is selected at build time; exactly one must be active.

actions!(manox, [Quit, ToggleFullscreen, OpenAbout]);

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
        // Persist every panic to a durable log: launchd swallows stderr, so a
        // panicking process would otherwise leave no record for diagnosis.
        if let Some(home) = std::env::var_os("HOME") {
            let dir = std::path::Path::new(&home).join("Library/Logs/Manox");
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("panic.log"))
                .and_then(|mut f| {
                    use std::io::Write as _;
                    writeln!(f, "{msg}")
                });
        }
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

    // Re-open the main window when the app is activated with no open window
    // (macOS dock-icon click); the platform only fires this when no window
    // exists, which is exactly the tray-parked state.
    app.on_reopen(open_or_focus_main_window);

    app.run(move |cx| {
        gpui_component::init(cx);
        // This binary is the native-app host; the identity must be pinned
        // before `agent::init` computes host-scoped state.
        agent::host::set_host(agent::host::Host::ManoxApp);
        agent::init(cx);
        agent_ui::slash_command::init(cx);
        terminal::init(cx);
        terminal_ui::init(cx);

        // Embedded OFL typefaces. Lilex ships only Light/Medium in upright and
        // italic cuts: message body inherits Light, markdown bold/headings and
        // tool-call titles resolve to Medium via nearest-weight matching, italic
        // syntax plus reasoning/tool cards hit the italic cuts. JetBrains Mono
        // NL (no-ligature cut) is the UI-chrome family (sidebar, buttons,
        // settings, menus); NL avoids ligatures so operator sequences in
        // labels never collapse into symbols.
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
                    "../assets/fonts/jetbrains-mono/JetBrainsMonoNL-Regular.ttf"
                )),
                Cow::Borrowed(include_bytes!(
                    "../assets/fonts/jetbrains-mono/JetBrainsMonoNL-Bold.ttf"
                )),
                Cow::Borrowed(include_bytes!(
                    "../assets/fonts/jetbrains-mono/JetBrainsMonoNL-Italic.ttf"
                )),
                Cow::Borrowed(include_bytes!(
                    "../assets/fonts/jetbrains-mono/JetBrainsMonoNL-BoldItalic.ttf"
                )),
            ])
            .expect("failed to register embedded fonts");

        // Lilex is the family name embedded in the TTFs (not "Lilex Mono").
        {
            let theme = Theme::global_mut(cx);
            theme.font_family = "JetBrains Mono NL".into();
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
            gpui::KeyBinding::new("cmd-t", agent_ui::NewTerminalTab, None),
            gpui::KeyBinding::new("cmd-shift-t", agent_ui::FocusTerminal, None),
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-shift-c", agent_ui::FocusConversation, None),
            gpui::KeyBinding::new("ctrl-t", agent_ui::NewTerminalTab, None),
            gpui::KeyBinding::new("ctrl-shift-t", agent_ui::FocusTerminal, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("ctrl-shift-c", agent_ui::FocusConversation, None),
            // Built-in browser. cmd-b opens a new browser tab in the right
            // pane, cmd-shift-b closes the active browser tab.
            gpui::KeyBinding::new("cmd-b", agent_ui::OpenBrowserTab, None),
            gpui::KeyBinding::new("cmd-shift-b", agent_ui::CloseBrowserTab, None),
            gpui::KeyBinding::new("ctrl-alt-b", agent_ui::OpenBrowserTab, None),
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
            // the plan-steps section in the "Conversation Info" card. The
            // header is also clickable; this is the keyboard affordance.
            gpui::KeyBinding::new("cmd-shift-m", agent_ui::ToggleCockpitTasks, None),
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
            // Archive the current thread and start a fresh one.
            gpui::KeyBinding::new("cmd-;", agent_ui::ArchiveCurrentThread, None),
            gpui::KeyBinding::new("ctrl-;", agent_ui::ArchiveCurrentThread, None),
            // Open turn navigator (additional binding alongside cmd-m).
            #[cfg(target_os = "macos")]
            gpui::KeyBinding::new("cmd-shift-;", agent_ui::ToggleTurnNavigator, None),
            #[cfg(not(target_os = "macos"))]
            gpui::KeyBinding::new("ctrl-shift-;", agent_ui::ToggleTurnNavigator, None),
        ]);
        cx.bind_keys(agent_ui::turn_navigator_key_bindings());
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.on_action(|_: &ToggleFullscreen, cx: &mut App| {
            if let Some(handle) = cx.active_window() {
                let _ = handle.update(cx, |_, window, _| window.toggle_fullscreen());
            }
        });
        cx.on_action(|_: &OpenAbout, cx: &mut App| {
            about::open_about_window(cx);
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
        // deferred App-level dispatch as the other menu actions; the provider +
        // model payload travels inside the action instance.
        cx.on_action(|action: &agent_ui::LaunchChatGptApp, cx: &mut App| {
            let (workspace, handle) = (
                agent_ui::dispatch::workspace_global(),
                agent_ui::dispatch::window_global(),
            );
            let provider = action.provider.clone();
            let model = action.model.clone();
            cx.defer(move |cx| {
                if let (Some(workspace), Some(handle)) = (workspace, handle) {
                    let _ = handle.update(cx, |_, window, cx| {
                        workspace.update(cx, |ws, cx| {
                            ws.launch_chatgpt_app(provider, model, window, cx)
                        });
                    });
                }
            });
        });

        // VS Code launch from the `工具 → VS Code` entry: injection resolves
        // from the persisted `vscode_app:` settings (no launch-time provider /
        // model choice): same deferred dispatch shape.
        cx.on_action(|_: &agent_ui::LaunchVSCode, cx: &mut App| {
            let (workspace, handle) = (
                agent_ui::dispatch::workspace_global(),
                agent_ui::dispatch::window_global(),
            );
            cx.defer(move |cx| {
                if let (Some(workspace), Some(handle)) = (workspace, handle) {
                    let _ = handle.update(cx, |_, window, cx| {
                        workspace.update(cx, |ws, cx| ws.launch_vscode_app(None, window, cx));
                    });
                }
            });
        });

        // Register the native-menu rebuilder so a live UI-language change can
        // re-resolve every menu label via the new locale (the bin owns
        // `build_app_menus` — `Quit` and `Menu`/`MenuItem` live here, not in
        // `agent` or `agent-ui`, so the rebuild closure stays in this crate).
        agent::i18n::set_menu_rebuilder(|cx| {
            cx.set_menus(build_app_menus());
            tray::rebuild_menus();
        });

        cx.set_menus(build_app_menus());

        // The initial provider registration runs on a background thread
        // (keychain / shell resolution), so the menus built above start
        // empty. Rebuild once registration lands so the 工具 cascades
        // populate without waiting for a settings save / manual reload.
        cx.spawn(async move |cx| {
            agent::pi_providers::wait_ready().await;
            cx.update(agent::i18n::rebuild_menus);
        })
        .detach();

        cx.spawn(async move |cx| {
            cx.update(|cx| {
                open_main_window(cx).expect("failed to open window");

                // System tray: the lifeline for reaching a window-less manox,
                // installed only AFTER the main window opened. The status item
                // creates its own NSStatusBarWindow; on a system whose
                // window-server resources are exhausted (e.g. the IOSurface
                // client cap) AppKit aborts inside window creation, and with
                // the window first that abort can only occur where manox could
                // never have shown UI anyway — the tray adds no new startup
                // death mode. With a tray, closing the main window parks the
                // app instead of quitting it — `QuitMode::Explicit` makes only
                // `cx.quit()` (menu Quit / tray 退出) terminate — and the
                // process-lifetime `Workspace` entity in `agent_ui::dispatch`
                // keeps the foreground thread and any parked background
                // threads running through the close. Install failure keeps the
                // platform default (macOS parks anyway; other platforms quit
                // on last window close) so the app never strands invisibly.
                match tray::install() {
                    Ok(()) => {
                        cx.set_quit_mode(QuitMode::Explicit);
                        tray::spawn_pump(cx);
                    }
                    // The fallback sentence is platform-true: macOS keeps a
                    // window-less app alive by default (dock icon reopens),
                    // other platforms quit on last window close.
                    #[cfg(target_os = "macos")]
                    Err(e) => tracing::warn!(
                        "system tray unavailable: {e:#}; a closed window parks                          manox in the dock with no tray — click the dock icon to reopen"
                    ),
                    #[cfg(not(target_os = "macos"))]
                    Err(e) => tracing::warn!(
                        "system tray unavailable: {e:#}; closing the last window quits"
                    ),
                }
            });

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
        Some(handle) => {
            handle.block_on(async {
                agent::background_task::shutdown_all().await;
                supervisor::global().shutdown_all().await;
            });
        }
        None => {
            supervisor::global().terminate_all();
        }
    }
}

/// Open the main window over the process-lifetime `Workspace`, creating the
/// workspace on first open and reusing it on every re-open (the tray "open"
/// path). Reuse preserves all in-memory state — the foreground thread, parked
/// background threads, drafts, sidebar selection — and keeps running threads
/// alive across a window close: their engine actors live off the `Thread`
/// entities the workspace holds, so a fresh workspace would orphan them.
fn open_main_window(cx: &mut App) -> anyhow::Result<WindowHandle<Root>> {
    let reused = agent_ui::dispatch::workspace_global().is_some();
    let window_options = WindowOptions {
        titlebar: Some(TitleBar::title_bar_options()),
        window_bounds: Some(WindowBounds::centered(size(px(1100.), px(760.)), cx)),
        // Floor below which the window can no longer shrink. Shared with
        // Settings so both views respect one minimum width.
        window_min_size: Some(size(px(MIN_WINDOW_W), px(MIN_WINDOW_H))),
        ..Default::default()
    };
    let handle = cx
        .open_window(window_options, |window, cx| {
            window.activate_window();
            window.set_window_title("Manox Pi");
            Theme::change(ThemeMode::Light, Some(window), cx);

            let view = match agent_ui::dispatch::workspace_global() {
                Some(view) => view,
                None => {
                    let view = cx.new(|cx| agent_ui::Workspace::new(window, cx));
                    agent_ui::dispatch::set_workspace(view.clone());
                    view
                }
            };
            cx.new(|cx| Root::new(view, window, cx))
        })
        .map_err(|e| anyhow::anyhow!("failed to open the main window: {e}"))?;
    agent_ui::dispatch::set_window(handle);

    // A re-opened window starts unfocused; restore keyboard focus to the
    // conversation pane. The first open already focuses the composer inside
    // `Workspace::new`.
    if reused
        && let (Some(workspace), Some(handle)) = (
            agent_ui::dispatch::workspace_global(),
            agent_ui::dispatch::window_global(),
        )
    {
        let _ = handle.update(cx, |_, _window, cx| {
            workspace.update(cx, |ws, cx| ws.focus_conversation(cx));
        });
    }
    Ok(handle)
}

/// Tray / dock "open" entry point: focus the live main window when one
/// exists, otherwise re-open it over the surviving workspace.
pub(crate) fn open_or_focus_main_window(cx: &mut App) {
    if let Some(handle) = agent_ui::dispatch::window_global()
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        cx.activate(true);
        return;
    }
    if let Err(e) = open_main_window(cx) {
        tracing::error!("failed to re-open the main window: {e:#}");
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
                MenuItem::action(agent::i18n::t("menu-about"), OpenAbout),
                MenuItem::separator(),
                MenuItem::action(agent::i18n::t("menu-settings"), agent_ui::OpenSettings),
                MenuItem::separator(),
                MenuItem::action(agent::i18n::t("menu-quit"), Quit),
            ]),
            build_tools_menu(),
        ]
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec![Menu::new(agent::i18n::t("menu-file")).items([
            MenuItem::action(agent::i18n::t("menu-about"), OpenAbout),
            MenuItem::separator(),
            MenuItem::action(agent::i18n::t("menu-settings"), agent_ui::OpenSettings),
            MenuItem::separator(),
            MenuItem::action(agent::i18n::t("menu-quit"), Quit),
        ])]
    }
}

/// The `工具` (Tools) top-level menu: external-app entries that launch with
/// provider/model injection from the cx registry. `ChatGPT.app` cascades
/// provider → model (Responses-capable models); `VS Code` cascades provider →
/// model (Anthropic-wire models) and injects Claude Code BYOK env. Picking a
/// model dispatches the matching launch action, routed through the bin's
/// App-level action handlers to the Workspace. Native menu items carry no
/// images in gpui, so entries are text-only.
#[cfg(target_os = "macos")]
fn build_tools_menu() -> Menu {
    let chatgpt = Menu::new("ChatGPT.app").items(build_chatgpt_menu_items());
    let chatgpt_empty = chatgpt.items.is_empty();
    let chatgpt = chatgpt.disabled(chatgpt_empty);
    // VS Code 单一入口：注入目标由持久化 `vscode_app:` 设置决定
    //（Settings → 外部工具 → Visual Studio Code.app），启动时无选择级联。
    // VS Code 未安装时禁用——此时没有任何 VS Code 实例可打开/注入。
    let vscode = MenuItem::action(agent::i18n::t("menu-vscode-open"), agent_ui::LaunchVSCode)
        .disabled(!cx::vscode_app_installed());
    Menu::new(agent::i18n::t("menu-tools")).items([MenuItem::submenu(chatgpt), vscode])
}

/// Items inside the `ChatGPT.app` submenu: one nested submenu per provider
/// that exposes models visible to the `ChatGPT.app` agent (i.e. Responses-capable
/// models), one action item per model. Mirrors the provider registry snapshot
/// at build time — `agent::i18n::rebuild_menus` re-runs menu construction after
/// a registry reload. Provider submenus keep the registry's first-appearance
/// order; models keep registry order within their provider.
#[cfg(target_os = "macos")]
fn build_chatgpt_menu_items() -> Vec<MenuItem> {
    let mut providers: Vec<(String, Vec<String>)> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for model in agent::pi_providers::global().models() {
        let visible = model
            .metadata
            .get("agents")
            .and_then(|v| v.as_array())
            .map(|list| list.iter().any(|a| a.as_str() == Some("ChatGPT.app")))
            .unwrap_or(true);
        if !visible {
            continue;
        }
        let provider = agent::pi_providers::display_provider_name(&model);
        let model_id = model
            .metadata
            .get("config_id")
            .and_then(|v| v.as_str())
            .unwrap_or(model.id.as_str())
            .to_string();
        if !seen.insert((provider.clone(), model_id.clone())) {
            continue; // same model registered on several wire apis
        }
        match providers.iter_mut().find(|(name, _)| *name == provider) {
            Some((_, models)) => models.push(model_id),
            None => providers.push((provider, vec![model_id])),
        }
    }
    providers
        .into_iter()
        .map(|(provider, models)| {
            MenuItem::submenu(
                Menu::new(provider.clone()).items(models.into_iter().map(|model| {
                    MenuItem::action(
                        model.clone(),
                        agent_ui::LaunchChatGptApp {
                            provider: provider.clone(),
                            model,
                        },
                    )
                })),
            )
        })
        .collect()
}
