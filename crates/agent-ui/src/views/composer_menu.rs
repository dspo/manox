//! Composer `+` and `⁄` menus plus pending-attachment support.
//!
//! The `+` menu offers "add / plugins" rows; "Files and Folders" opens a real
//! file picker and "Plan Mode" toggles the thread's plan mode. The remaining
//! rows are static decoration. The `⁄` menu's top section lists registered
//! slash commands dynamically (from the `SlashCommandRegistry`); the rest
//! (memory / skills) remains static decoration. Clicking a registered command
//! inserts `/name ` into the composer for the user to complete and submit.
//!
//! A [`PendingAttachment`] is a file the user picked but has not yet sent. On submit the workspace
//! turns each into message content: images become base64 [`MessageContent::Image`] blocks, text
//! files are inlined into the message text wrapped in `<file>` tags.

use std::rc::Rc;

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

/// Static row for the `+` and `⁄` menus: an icon, a name, and a description.
/// `name`/`desc` are fluent message ids for localized rows, or literal English
/// text for the decorative placeholder rows (resolved identically via
/// identically via [`menu_row_item`]).
struct MenuRow {
    icon: IconName,
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
        name: "composer-add-files",
        desc: "",
    },
    MenuRow {
        icon: IconName::SquareTerminal,
        name: "composer-attach-editor",
        desc: "",
    },
    MenuRow {
        icon: IconName::CircleCheck,
        name: "composer-goal-name",
        desc: "composer-goal-desc",
    },
    MenuRow {
        icon: IconName::LayoutDashboard,
        name: "composer-plan-mode-name",
        desc: "composer-plan-mode-desc",
    },
];

/// `+` menu "Plugins" group — all static decoration. Literal English product
/// names (not localized); `i18n::t` returns them unchanged via the missing-key
/// fallback, which is fine since they are proper nouns.
const PLUS_PLUGIN_ROWS: &[MenuRow] = &[
    MenuRow {
        icon: IconName::File,
        name: "Documents",
        desc: "Create and edit document artifacts",
    },
    MenuRow {
        icon: IconName::File,
        name: "PDF",
        desc: "Read, create, and verify PDF files",
    },
    MenuRow {
        icon: IconName::File,
        name: "Spreadsheets",
        desc: "Create and edit spreadsheet files",
    },
    MenuRow {
        icon: IconName::File,
        name: "Presentations",
        desc: "Create and edit presentations",
    },
    MenuRow {
        icon: IconName::File,
        name: "Template Creator",
        desc: "Create or update personal artifact templates",
    },
];

/// `⁄` menu skills group. The `bool` is whether the skill is "personal" vs "system".
const SLASH_SKILL_ROWS: &[(&str, &str, bool)] = &[
    (
        "Browser",
        "Browser lets manox open and control the in-app browser",
        true,
    ),
    ("CI Debug", "Debug failing GitHub Actions checks", true),
    (
        "Chrome: Control Chrome",
        "Control the user's Chrome browser for tasks",
        true,
    ),
    (
        "Documents",
        "Create and edit Word and Google Docs files",
        true,
    ),
    ("GitHub", "Inspect PRs, issues, CI, and publish bots", true),
    (
        "GitLab Dev",
        "Guide GitLab issue, MR, pipeline, and worktree workflows",
        true,
    ),
    (
        "Image Gen",
        "Generate or edit images for websites, games, and more",
        false,
    ),
    (
        "OpenAI Docs",
        "Reference OpenAI docs, self-knowledge, and model info",
        false,
    ),
    ("PDF", "Read, create, render, and verify PDF files", true),
];

/// Render one menu row (icon + name + muted description) as a popup-menu element
/// item. `name`/`desc` are resolved through `i18n::t`: fluent keys yield the
/// localized text, literal English placeholders fall through unchanged.
fn menu_row_item(row: &'static MenuRow, theme: &Theme) -> PopupMenuItem {
    let fg = theme.foreground;
    let muted = theme.muted_foreground;
    let icon = row.icon.clone();
    let name = i18n::t(row.name);
    let desc = i18n::t(row.desc);
    PopupMenuItem::element(move |_window, _cx| {
        let mut line = h_flex()
            .items_center()
            .gap_2()
            .child(Icon::new(icon.clone()).small().text_color(muted))
            .child(gpui::div().text_sm().text_color(fg).child(name.clone()));
        if !desc.is_empty() {
            line = line.child(gpui::div().text_xs().text_color(muted).child(desc.clone()));
        }
        line
    })
}

/// A `⁄`-popover row for a registered slash command: a `/name` label in mono
/// style plus its description. Clicking inserts the command into the composer.
fn slash_command_item(name: &str, desc: &str, theme: &Theme) -> PopupMenuItem {
    let fg = theme.foreground;
    let muted = theme.muted_foreground;
    let name = format!("/{name}");
    let desc = desc.to_string();
    PopupMenuItem::element(move |_window, _cx| {
        let mut line = h_flex().w_full().items_center().gap_2().child(
            gpui::div()
                .text_sm()
                .text_color(fg)
                .child(gpui::StyledText::new(name.clone())),
        );
        if !desc.is_empty() {
            line = line.child(
                gpui::div()
                    .flex_1()
                    .text_xs()
                    .text_color(muted)
                    .child(desc.clone()),
            );
        }
        line
    })
}

/// Build the `+` popup menu. `on_files` runs when the "Files and folders" row is
/// clicked (index 0); `on_plan` runs when the "Plan mode" row is clicked (index 3).
pub fn build_plus_menu(
    menu: PopupMenu,
    theme: &Theme,
    on_files: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
    on_plan: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
    on_goal: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
) -> PopupMenu {
    let mut menu = menu.max_w(gpui::px(360.)).scrollable(true);
    menu = menu.label(i18n::t("composer-add-label"));
    let on_files = std::rc::Rc::new(on_files);
    let on_plan = std::rc::Rc::new(on_plan);
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
            3 => {
                let on_plan = on_plan.clone();
                menu = menu.item(
                    menu_row_item(row, theme).on_click(move |_, window, cx| on_plan(window, cx)),
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

/// Build the `⁄` popover. The top section lists registered slash commands
/// dynamically; clicking one inserts `/name ` into the composer via
/// `on_select(name)`. The memory and skills sections remain static decoration.
pub fn build_slash_menu(
    menu: PopupMenu,
    theme: &Theme,
    on_select: impl Fn(&str, &mut gpui::Window, &mut gpui::App) + 'static,
) -> PopupMenu {
    let mut menu = menu.max_w(gpui::px(420.)).scrollable(true);
    menu = menu.label(i18n::t("composer-commands-label"));
    // Registered slash commands (dynamic). Falls back to an empty section when
    // the registry is not yet initialized.
    let on_select = Rc::new(on_select);
    if let Some(reg) = crate::slash_command::SlashCommandRegistry::global() {
        for cmd in reg.commands() {
            let name = cmd.name().to_string();
            let desc = cmd.description().to_string();
            let on_select = on_select.clone();
            menu = menu.item(slash_command_item(&name, &desc, theme).on_click(
                move |_, window, cx| {
                    on_select(&name, window, cx);
                },
            ));
        }
    }
    menu = menu.separator().label(i18n::t("composer-memory-label"));
    menu = menu.item(PopupMenuItem::Label(i18n::t("composer-generate-memory")));
    menu = menu.separator().label(i18n::t("composer-skills-label"));
    let fg = theme.foreground;
    let muted = theme.muted_foreground;
    for (name, desc, personal) in SLASH_SKILL_ROWS {
        let tag = if *personal {
            i18n::t("composer-tag-personal")
        } else {
            i18n::t("composer-tag-system")
        };
        menu = menu.item(PopupMenuItem::element(move |_window, _cx| {
            h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .child(Icon::new(IconName::Frame).small().text_color(muted))
                .child(gpui::div().text_sm().text_color(fg).child(*name))
                .child(
                    gpui::div()
                        .flex_1()
                        .text_xs()
                        .text_color(muted)
                        .child(*desc),
                )
                .child(gpui::div().text_xs().text_color(muted).child(tag.clone()))
        }));
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
