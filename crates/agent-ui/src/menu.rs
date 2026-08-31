//! Menu rebuilder — the gpui-dependent twin of `agent::i18n`.
//!
//! The bin owns the native-menu construction (`Quit` action and `Menu`/`MenuItem`
//! live there). When the UI locale changes the native menus must be re-`set_menus`'d
//! with fresh `t()`-resolved labels; this module holds the indirection that keeps
//! the rebuild closure out of the `agent` crate (it cannot depend on `gpui::App`)
//! without scattering menu-rebuild calls across the UI layer.

use std::sync::OnceLock;

use gpui::App;

type MenuRebuild = Box<dyn Fn(&mut App) + Send + Sync>;
static MENU_REBUILDER: OnceLock<MenuRebuild> = OnceLock::new();

/// Register the native-menu rebuilder. Called once from the bin at startup,
/// after `agent::init`. Subsequent `rebuild_menus` calls invoke it so menu
/// labels re-localize live.
pub fn set_menu_rebuilder(rebuild: impl Fn(&mut App) + Send + Sync + 'static) {
    let _ = MENU_REBUILDER.set(Box::new(rebuild));
}

/// Re-run the registered native-menu rebuilder. The menu tree embeds dynamic
/// content beyond localized labels (the provider/model cascades mirror the
/// provider registry), so callers that swap the registry — or otherwise change
/// what the menus should show — rebuild through this.
/// No-op before `set_menu_rebuilder` registers the closure.
pub fn rebuild_menus(cx: &mut App) {
    if let Some(rebuild) = MENU_REBUILDER.get() {
        rebuild(cx);
    }
}