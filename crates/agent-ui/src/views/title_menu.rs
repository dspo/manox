//! Title bar "..." popup menu (Codex.app parity).
//!
//! Built lazily by the workspace; each row wires either to a real
//! thread-store op (pin / rename / archive) or to an info-message stub for
//! not-yet-implemented features. Submenus (Copy / Branch) are built via
//! `PopupMenu::submenu` with their own `PopupMenu` entities.

use std::rc::Rc;

use agent::i18n;
use gpui::{App, ClickEvent, Window};
use gpui_component::menu::{PopupMenu, PopupMenuItem};

/// Row click handler shared by all title menu entries. Cloned into each
/// `on_click` so the same callback can be reused across the parent menu and
/// its submenu rows.
type RowHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// All callbacks the workspace hands in when constructing the menu. Cloned
/// into each row's `on_click` (and into the submenus, where they need to be
/// shared across parent and child builders).
pub struct TitleMenuCallbacks {
    pub on_pin: RowHandler,
    pub on_rename: RowHandler,
    pub on_archive: RowHandler,
    pub on_copy_id: RowHandler,
    pub on_copy_markdown: RowHandler,
    pub on_copy_cwd: RowHandler,
    pub on_copy_deeplink: RowHandler,
    pub on_schedule: RowHandler,
    pub on_new_window: RowHandler,
    /// True when the active thread is already pinned — toggles the row label
    /// between "Pin" and "Unpin".
    pub is_pinned: bool,
    /// True when the active thread is already archived — toggles the row label
    /// between "Archive" and "Unarchive".
    pub is_archived: bool,
}

/// Append the title bar menu rows to `menu` and return it. Designed to be
/// called from the `PopupMenu::build` closure in `Workspace`.
pub fn build_title_menu(
    menu: PopupMenu,
    window: &mut Window,
    cx: &mut gpui::Context<PopupMenu>,
    cb: TitleMenuCallbacks,
) -> PopupMenu {
    let mut menu = menu.max_w(gpui::px(280.));

    menu = menu.item(
        PopupMenuItem::new(if cb.is_pinned {
            i18n::t("titlebar-unpin")
        } else {
            i18n::t("titlebar-pin")
        })
        .on_click({
            let on_pin = cb.on_pin.clone();
            move |ev, window, cx| on_pin(ev, window, cx)
        }),
    );

    menu = menu.item(PopupMenuItem::new(i18n::t("titlebar-rename")).on_click({
        let on_rename = cb.on_rename.clone();
        move |ev, window, cx| on_rename(ev, window, cx)
    }));

    menu = menu.item(
        PopupMenuItem::new(if cb.is_archived {
            i18n::t("titlebar-unarchive")
        } else {
            i18n::t("titlebar-archive")
        })
        .on_click({
            let on_archive = cb.on_archive.clone();
            move |ev, window, cx| on_archive(ev, window, cx)
        }),
    );

    menu = menu.separator();

    // The sidebar is always visible; the "Open side chat" row stays as a
    // disabled placeholder so the menu shape matches Codex's layout without
    // implying the feature is implemented.
    menu = menu.item(PopupMenuItem::new(i18n::t("titlebar-sidebar-toggle")).disabled(true));

    menu = menu.submenu(
        i18n::t("titlebar-copy-label"),
        window,
        cx,
        move |sub, _window, _cx| {
            sub.item(PopupMenuItem::new(i18n::t("titlebar-copy-id")).on_click({
                let on_copy_id = cb.on_copy_id.clone();
                move |ev, window, cx| on_copy_id(ev, window, cx)
            }))
            .item(
                PopupMenuItem::new(i18n::t("titlebar-copy-markdown")).on_click({
                    let on_copy_markdown = cb.on_copy_markdown.clone();
                    move |ev, window, cx| on_copy_markdown(ev, window, cx)
                }),
            )
            .item(PopupMenuItem::new(i18n::t("titlebar-copy-cwd")).on_click({
                let on_copy_cwd = cb.on_copy_cwd.clone();
                move |ev, window, cx| on_copy_cwd(ev, window, cx)
            }))
            .item(
                PopupMenuItem::new(i18n::t("titlebar-copy-deeplink")).on_click({
                    let on_copy_deeplink = cb.on_copy_deeplink.clone();
                    move |ev, window, cx| on_copy_deeplink(ev, window, cx)
                }),
            )
        },
    );

    menu = menu.submenu(
        i18n::t("titlebar-branch-label"),
        window,
        cx,
        move |sub, _window, _cx| {
            // Branching a conversation is not implemented. Rows stay visible
            // (matching Codex's menu shape) but disabled so the user can see
            // the entry without invoking an action.
            sub.item(PopupMenuItem::new(i18n::t("titlebar-branch-from-here")).disabled(true))
                .item(PopupMenuItem::new(i18n::t("titlebar-branch-from-start")).disabled(true))
        },
    );

    menu = menu.item(PopupMenuItem::new(i18n::t("titlebar-schedule")).on_click({
        let on_schedule = cb.on_schedule.clone();
        move |ev, window, cx| on_schedule(ev, window, cx)
    }));

    menu = menu.separator();

    menu = menu.item(
        PopupMenuItem::new(i18n::t("titlebar-new-window")).on_click({
            let on_new_window = cb.on_new_window.clone();
            move |ev, window, cx| on_new_window(ev, window, cx)
        }),
    );

    menu
}
