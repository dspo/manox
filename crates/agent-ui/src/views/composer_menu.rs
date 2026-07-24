//! Composer `+` menu plus pending-attachment support.
//!
//! The `+` menu offers "add / plugins" rows; "Files and Folders" opens a real
//! file picker and "Plan Mode" toggles the thread's plan mode. The remaining
//! rows are static decoration. Slash-command typeahead (`/`) and `@` mentions
//! live in [`crate::views::completion`], not here.
//!
//! A [`PendingAttachment`] is a file the user picked but has not yet sent. On submit the workspace
//! turns each into message content: images become base64 [`MessageContent::Image`] blocks, text
//! files are inlined into the message text wrapped in `<file>` tags.

use std::path::{Path, PathBuf};

use agent::i18n;
use agent::language_model::MessageContent;
use base64::Engine as _;
use gpui::{SharedString, prelude::*};
use gpui_component::{
    Icon, IconName, Sizable as _, Theme, h_flex,
    menu::{PopupMenu, PopupMenuItem},
    v_flex,
};

/// Static row for the `+` menu: an icon, a name, and a description.
/// `name`/`desc` are fluent message ids for localized rows, or literal English
/// text for the decorative placeholder rows (resolved identically via
/// [`menu_row_item`]).
struct MenuRow {
    icon: IconName,
    /// Custom SVG path when the icon is not in the `IconName` enum
    /// (layered via `ExtrasAssetSource`). When set, this overrides `icon`.
    icon_path: Option<&'static str>,
    name: &'static str,
    desc: &'static str,
}

/// `+` menu "Add" group. "Files and folders" (index 0) opens the file picker,
/// and "Plan mode" (index 3) toggles plan mode, both wired by the caller; the
/// rest are static decoration. Names/descs are fluent keys
/// resolved at render time.
const PLUS_ADD_ROWS: &[MenuRow] = &[
    MenuRow {
        icon: IconName::Folder,
        icon_path: None,
        name: "composer-add-files",
        desc: "",
    },
    MenuRow {
        icon: IconName::SquareTerminal,
        icon_path: None,
        name: "composer-attach-editor",
        desc: "",
    },
    MenuRow {
        icon: IconName::CircleCheck,
        icon_path: Some("circle-check-big.svg"),
        name: "composer-goal-name",
        desc: "composer-goal-desc",
    },
];

/// `+` menu "Plugins" group — all static decoration. Literal English product
const PLUS_PLUGIN_ROWS: &[MenuRow] = &[
    MenuRow {
        icon: IconName::File,
        icon_path: None,
        name: "Documents",
        desc: "Create and edit document artifacts",
    },
    MenuRow {
        icon: IconName::File,
        icon_path: None,
        name: "PDF",
        desc: "Read, create, and verify PDF files",
    },
    MenuRow {
        icon: IconName::File,
        icon_path: None,
        name: "Spreadsheets",
        desc: "Create and edit spreadsheet files",
    },
    MenuRow {
        icon: IconName::File,
        icon_path: None,
        name: "Presentations",
        desc: "Create and edit presentations",
    },
    MenuRow {
        icon: IconName::File,
        icon_path: None,
        name: "Template Creator",
        desc: "Create or update personal artifact templates",
    },
];
/// Render one menu row (icon + name + muted description) as a popup-menu element
fn menu_row_item(row: &'static MenuRow, theme: &Theme) -> PopupMenuItem {
    let fg = theme.foreground;
    let muted = theme.muted_foreground;
    let icon = row.icon.clone();
    let icon_path = row.icon_path;
    let name = i18n::t(row.name);
    let desc = i18n::t(row.desc);
    PopupMenuItem::element(move |_window, _cx| {
        let icon_el: Icon = if let Some(path) = icon_path {
            Icon::default().path(format!("icons/{path}"))
        } else {
            Icon::new(icon.clone())
        };
        let mut line = h_flex()
            .items_center()
            .gap_2()
            .child(icon_el.small().text_color(muted))
            .child(gpui::div().text_sm().text_color(fg).child(name.clone()));
        if !desc.is_empty() {
            line = line.child(gpui::div().text_xs().text_color(muted).child(desc.clone()));
        }
        line
    })
}

/// Build the `+` popup menu. `on_files` runs when the "Files and folders" row
/// is clicked (index 0); `on_goal` runs when the "Goal" row is clicked
/// (index 2).
pub fn build_plus_menu(
    menu: PopupMenu,
    theme: &Theme,
    on_files: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
    on_goal: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
) -> PopupMenu {
    let mut menu = menu.max_w(gpui::px(360.)).scrollable(true);
    menu = menu.label(i18n::t("composer-add-label"));
    let on_files = std::rc::Rc::new(on_files);
    let on_goal = std::rc::Rc::new(on_goal);
    for (ix, row) in PLUS_ADD_ROWS.iter().enumerate() {
        match ix {
            0 => {
                let on_files = on_files.clone();
                menu = menu.item(
                    menu_row_item(row, theme).on_click(move |_, window, cx| on_files(window, cx)),
                );
            }
            2 => {
                let on_goal = on_goal.clone();
                menu = menu.item(
                    menu_row_item(row, theme).on_click(move |_, window, cx| on_goal(window, cx)),
                );
            }
            _ => {
                menu = menu.item(menu_row_item(row, theme));
            }
        }
    }
    menu = menu.separator().label(i18n::t("composer-plugins-label"));
    for row in PLUS_PLUGIN_ROWS {
        menu = menu.item(menu_row_item(row, theme));
    }
    menu
}

/// An attachment staged in the composer but not yet submitted. Either a file
/// picked from the `+` menu, or an image pasted straight from the clipboard
/// (resized off-thread on submit).
#[derive(Debug, Clone)]
pub enum PendingAttachment {
    File { path: PathBuf, is_image: bool },
    ClipboardImage(gpui::Image),
}

impl PendingAttachment {
    pub fn new(path: PathBuf) -> Self {
        Self::File {
            is_image: is_image_path(&path),
            path,
        }
    }

    pub fn is_image(&self) -> bool {
        matches!(
            self,
            Self::ClipboardImage(_) | Self::File { is_image: true, .. }
        )
    }

    pub fn file_name(&self) -> String {
        match self {
            Self::File { path, .. } => path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("file")
                .to_string(),
            // No filename on the clipboard; surface a localized label instead.
            Self::ClipboardImage(_) => i18n::t("composer-pasted-image").to_string(),
        }
    }
}

fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp")
    )
}

fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        _ => "application/octet-stream",
    }
}

/// Read one on-disk attachment into a [`MessageContent`]. Images become base64
/// image blocks; text files are inlined into `text_out` wrapped in `<file>`
/// tags. Clipboard images are handled separately by the caller (resize) and
/// yield `None` here. Blocking file IO — call from a background executor.
pub fn load_attachment(att: &PendingAttachment, text_out: &mut String) -> Option<MessageContent> {
    let PendingAttachment::File { path, is_image } = att else {
        return None;
    };
    if *is_image {
        let bytes = std::fs::read(path).ok()?;
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Some(MessageContent::Image {
            data,
            mime_type: mime_for(path).to_string(),
        })
    } else {
        let content = std::fs::read_to_string(path).ok()?;
        text_out.push_str(&format!(
            "\n<file path=\"{}\">\n{content}\n</file>",
            path.display()
        ));
        None
    }
}

/// Render the pending-attachment chips shown above the composer. `on_remove(ix)` removes chip `ix`.
pub fn render_attachment_chips(
    attachments: &[PendingAttachment],
    theme: &Theme,
    on_remove: impl Fn(usize, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::AnyElement {
    let on_remove = std::rc::Rc::new(on_remove);
    let mut row = h_flex().w_full().flex_wrap().gap_1();
    for (ix, att) in attachments.iter().enumerate() {
        let on_remove = on_remove.clone();
        let icon = if att.is_image() {
            IconName::Palette
        } else {
            IconName::File
        };
        let name: SharedString = att.file_name().into();
        row = row.child(
            h_flex()
                .id(("attachment-chip", ix))
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .bg(theme.secondary)
                .border_1()
                .border_color(theme.border)
                .child(
                    Icon::new(icon.clone())
                        .xsmall()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(name),
                )
                .child(
                    gpui::div()
                        .id(("attachment-remove", ix))
                        .cursor_pointer()
                        .child(
                            Icon::new(IconName::Close)
                                .xsmall()
                                .text_color(theme.muted_foreground),
                        )
                        .on_click(move |_, window, cx| on_remove(ix, window, cx)),
                ),
        );
    }
    v_flex().w_full().child(row).into_any_element()
}
