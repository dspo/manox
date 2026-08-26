//! System tray — keeps manox reachable after the main window closes.
//!
//! The tray is the process's lifeline while no window exists: its menu offers
//! "open" (re-open the main window over the surviving `Workspace`) and
//! "quit". Backends: macOS and Windows use `tray-icon` (a native status item
//! whose global event channels the gpui main loop polls), Linux uses `ksni`
//! (StatusNotifierItem over D-Bus — no GTK main loop, so it coexists with
//! gpui's own X11/Wayland loop and delivers commands from its own thread).
//!
//! gpui exposes no cross-thread wake primitive (`AsyncApp` is `!Send` and the
//! platform dispatcher is not reachable from `App`), so tray events are
//! bridged by a single foreground task that wakes every [`POLL`] and drains
//! whatever the active backend produced. Menu-click latency is bounded by one
//! poll period — imperceptible for this interaction.

use std::time::Duration;

use gpui::App;

/// Commands a tray backend can deliver to the app shell.
pub enum TrayCmd {
    /// Open the main window, or focus it when one already exists.
    Open,
    /// Open the WebUI in the browser, starting the server on first use.
    OpenWebUi,
    /// Quit the application.
    Quit,
}

/// Stable ids for the tray-icon menu items (macOS/Windows backends).
#[cfg(any(target_os = "macos", target_os = "windows"))]
const OPEN_ID: &str = "tray-open";
#[cfg(any(target_os = "macos", target_os = "windows"))]
const WEBUI_ID: &str = "tray-webui";
#[cfg(any(target_os = "macos", target_os = "windows"))]
const QUIT_ID: &str = "tray-quit";

/// Foreground poll period for tray event sources.
const POLL: Duration = Duration::from_millis(100);

/// Install the platform tray backend. Must run on the gpui main thread after
/// `agent::init` (labels resolve through `agent::i18n::t`) and after the main
/// window has opened: the status item creates its own window
/// (`NSStatusBarWindow` on macOS), and ordering it after a successful window
/// open keeps tray creation from becoming a startup death mode on systems
/// whose window-server resources are exhausted. Failure means no tray; the
/// caller should keep the default quit-on-close behavior so the app never
/// strands as a window-less background process.
pub fn install() -> anyhow::Result<()> {
    backend::install()
}

/// Rebuild the tray menu so labels re-resolve under a new UI locale. Called
/// from the native-menu rebuilder path (`agent::i18n::set_ui_language` →
/// `rebuild_menus`), which runs on the main thread.
pub fn rebuild_menus() {
    backend::rebuild_menus();
}

/// Spawn the foreground pump that drains tray events into app actions. Call
/// once, after a successful [`install`]; a second call panics (double pumps
/// would double-dispatch every menu click).
pub fn spawn_pump(cx: &mut App) {
    static PUMP_SPAWNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    PUMP_SPAWNED
        .set(())
        .expect("tray::spawn_pump called more than once");
    cx.spawn(async move |cx| {
        let mut cmds: Vec<TrayCmd> = Vec::new();
        loop {
            cx.background_executor().timer(POLL).await;
            cmds.clear();
            backend::drain(&mut cmds);
            let mut quit = false;
            for cmd in cmds.drain(..) {
                quit |= matches!(cmd, TrayCmd::Quit);
                cx.update(|cx| match cmd {
                    TrayCmd::Open => crate::open_or_focus_main_window(cx),
                    TrayCmd::OpenWebUi => manox_webui::open_webui(),
                    TrayCmd::Quit => cx.quit(),
                });
            }
            // The app is tearing down; stop polling.
            if quit {
                break;
            }
        }
    })
    .detach();
}

/// Decode the embedded app icon and resize it to a square tray-sized RGBA
/// bitmap. Shared by every backend; each converts to its wire format.
fn app_icon_rgba(size: u32) -> anyhow::Result<Vec<u8>> {
    let img = image::load_from_memory(include_bytes!("../resources/app-icon.png"))?;
    let img = img.resize_exact(size, size, image::imageops::FilterType::Lanczos3);
    Ok(img.into_rgba8().into_raw())
}

/// macOS / Windows backend: `tray-icon` status item + native menu. Both
/// platforms pump the tray's messages on the gpui main thread (macOS: the
/// NSApplication run loop; Windows: gpui's `GetMessageW` loop dispatches for
/// all windows on the thread, including tray-icon's hidden one), so the icon
/// is created here on the main thread and its global channels are drained by
/// [`super::spawn_pump`].
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod backend {
    use super::{OPEN_ID, QUIT_ID, TrayCmd, WEBUI_ID, app_icon_rgba};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    /// Tray icon edge in px: the macOS menu bar slot is 22pt (44 @2x); the
    /// Windows notification-area slot is 16dp (32 @2x).
    #[cfg(target_os = "macos")]
    const ICON_PX: u32 = 44;
    #[cfg(target_os = "windows")]
    const ICON_PX: u32 = 32;

    // Held for the process lifetime; dropping it removes the status item.
    // `TrayIcon` is `!Sync` (the macOS handle is `Rc<RefCell<..>>`), so it
    // lives in a main-thread-local — every access site (install, menu
    // rebuild) runs on the gpui main thread anyway.
    thread_local! {
        static TRAY: std::cell::RefCell<Option<TrayIcon>> = const { std::cell::RefCell::new(None) };
    }

    pub fn install() -> anyhow::Result<()> {
        let rgba = app_icon_rgba(ICON_PX)?;
        let icon = Icon::from_rgba(rgba, ICON_PX, ICON_PX)
            .map_err(|e| anyhow::anyhow!("invalid tray icon: {e}"))?;
        let builder = TrayIconBuilder::new()
            .with_tooltip("Manox")
            .with_icon(icon)
            .with_menu(Box::new(build_menu()?));
        // Windows convention: left click opens the app (handled in `drain`),
        // right click opens the menu. macOS keeps the default — icon click
        // pops the menu.
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_menu_on_left_click(false);
        let tray = builder.build()?;
        TRAY.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_some() {
                return Err(anyhow::anyhow!("tray already installed"));
            }
            *slot = Some(tray);
            Ok(())
        })
    }

    fn build_menu() -> anyhow::Result<Menu> {
        let open = MenuItem::with_id(OPEN_ID, agent::i18n::t("menu-open-manox"), true, None);
        let webui = MenuItem::with_id(WEBUI_ID, agent::i18n::t("menu-webui"), true, None);
        let quit = MenuItem::with_id(QUIT_ID, agent::i18n::t("menu-quit"), true, None);
        Menu::with_items(&[&open, &webui, &PredefinedMenuItem::separator(), &quit])
            .map_err(|e| anyhow::anyhow!("tray menu build failed: {e}"))
    }

    pub fn rebuild_menus() {
        TRAY.with(|slot| {
            let Some(tray) = &*slot.borrow() else {
                return;
            };
            match build_menu() {
                Ok(menu) => tray.set_menu(Some(Box::new(menu))),
                Err(e) => tracing::warn!("tray menu rebuild failed: {e:#}"),
            }
        });
    }

    pub fn drain(cmds: &mut Vec<TrayCmd>) {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match event.id().0.as_str() {
                OPEN_ID => cmds.push(TrayCmd::Open),
                WEBUI_ID => cmds.push(TrayCmd::OpenWebUi),
                QUIT_ID => cmds.push(TrayCmd::Quit),
                _ => {}
            }
        }
        // Keep the icon-click channel drained everywhere; only non-macOS acts
        // on it (macOS pops the menu on icon click). Windows delivers a
        // Click for both button-down and button-up — open on left-up only.
        while let Ok(event) = tray_icon::TrayIconEvent::receiver().try_recv() {
            #[cfg(not(target_os = "macos"))]
            if let tray_icon::TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            } = event
            {
                cmds.push(TrayCmd::Open);
            }
            #[cfg(target_os = "macos")]
            let _ = event;
        }
    }
}

/// Linux backend: `ksni` StatusNotifierItem over D-Bus. The tray service runs
/// on its own thread (blocking API — registration is synchronous, then the
/// service loop moves to the background); menu/activate callbacks fire there
/// and forward [`TrayCmd`]s over an mpsc channel that [`super::spawn_pump`]
/// drains on the gpui main loop. No GTK involvement, so nothing fights
/// gpui's event loop.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod backend {
    use super::{TrayCmd, app_icon_rgba};
    use ksni::blocking::TrayMethods;
    use ksni::menu::{MenuItem, StandardItem};
    use std::sync::{Mutex, OnceLock, mpsc};

    /// StatusNotifier icon edge in px (freedesktop trays render ~22dp).
    const ICON_PX: u32 = 48;

    static HANDLE: OnceLock<ksni::blocking::Handle<ManoxTray>> = OnceLock::new();
    static CMDS: OnceLock<Mutex<mpsc::Receiver<TrayCmd>>> = OnceLock::new();

    struct ManoxTray {
        icon: ksni::Icon,
        sender: mpsc::Sender<TrayCmd>,
    }

    impl ManoxTray {
        fn send(&self, cmd: TrayCmd) {
            // The pump drains continuously; a full channel is impossible in
            // practice (unbounded) and a dead receiver just means the app is
            // already quitting.
            let _ = self.sender.send(cmd);
        }
    }

    impl ksni::Tray for ManoxTray {
        fn id(&self) -> String {
            "manox".into()
        }

        fn title(&self) -> String {
            "Manox".into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            vec![self.icon.clone()]
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            self.send(TrayCmd::Open);
        }

        // Re-read on every menu popup and after `Handle::update`, so labels
        // always resolve through the current UI locale. `t()` runs on the
        // ksni service thread here; i18n keeps a thread-local bundle per
        // thread, rebuilt lazily against the process-global locale.
        fn menu(&self) -> Vec<MenuItem<Self>> {
            vec![
                StandardItem {
                    label: agent::i18n::t("menu-open-manox").to_string(),
                    activate: Box::new(|this: &mut Self| this.send(TrayCmd::Open)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: agent::i18n::t("menu-webui").to_string(),
                    activate: Box::new(|this: &mut Self| this.send(TrayCmd::OpenWebUi)),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: agent::i18n::t("menu-quit").to_string(),
                    activate: Box::new(|this: &mut Self| this.send(TrayCmd::Quit)),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    pub fn install() -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel();
        let handle = ManoxTray {
            icon: make_icon()?,
            sender: tx,
        }
        .spawn()?;
        let _ = CMDS.set(Mutex::new(rx));
        HANDLE
            .set(handle)
            .map_err(|_| anyhow::anyhow!("tray already installed"))
    }

    fn make_icon() -> anyhow::Result<ksni::Icon> {
        let rgba = app_icon_rgba(ICON_PX)?;
        // RGBA -> ARGB32, network byte order (A R G B per pixel).
        let mut data = Vec::with_capacity(rgba.len());
        for chunk in rgba.chunks_exact(4) {
            data.extend_from_slice(&[chunk[3], chunk[0], chunk[1], chunk[2]]);
        }
        Ok(ksni::Icon {
            width: ICON_PX as i32,
            height: ICON_PX as i32,
            data,
        })
    }

    pub fn rebuild_menus() {
        let Some(handle) = HANDLE.get() else { return };
        // A no-op mutation still makes the service re-emit the tray state,
        // re-reading `menu()` whose labels resolve against the new locale.
        handle.update(|_| {});
    }

    pub fn drain(cmds: &mut Vec<TrayCmd>) {
        static POISON_WARNED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        let Some(slot) = CMDS.get() else { return };
        let rx = match slot.lock() {
            Ok(rx) => rx,
            Err(poisoned) => {
                // Warn once (this runs every poll) but keep draining — the
                // channel itself is intact, a poisoned lock must not silently
                // kill the tray's only input path.
                if !POISON_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!("tray command channel lock poisoned, recovering");
                }
                poisoned.into_inner()
            }
        };
        while let Ok(cmd) = rx.try_recv() {
            cmds.push(cmd);
        }
    }
}

/// Fallback for targets with no tray backend (e.g. wasm): installation fails
/// cleanly so the app keeps the platform-default quit behavior instead of
/// stranding, and the pump drains nothing.
#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd"
)))]
mod backend {
    use super::TrayCmd;

    pub fn install() -> anyhow::Result<()> {
        Err(anyhow::anyhow!("no system tray backend for this platform"))
    }

    pub fn rebuild_menus() {}

    pub fn drain(_cmds: &mut Vec<TrayCmd>) {}
}
