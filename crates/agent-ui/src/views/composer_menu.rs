//! Composer `+` and `⁄` menus plus pending-attachment support.
//!
//! The `+` menu mirrors Codex.app's "add / plugins" popover; only "文件和文件夹" is wired to a
//! real file picker, the rest are static decoration. The `⁄` menu's top section lists
//! registered slash commands dynamically (from the `SlashCommandRegistry`); the rest
//! (memory / skills) remains static decoration. Clicking a registered command inserts
//! `/name ` into the composer for the user to complete and submit.
//!
//! A [`PendingAttachment`] is a file the user picked but has not yet sent. On submit the workspace
//! turns each into message content: images become base64 [`MessageContent::Image`] blocks, text
//! files are inlined into the message text wrapped in `<file>` tags.

use std::rc::Rc;

use std::path::{Path, PathBuf};

use agent::language_model::MessageContent;
use base64::Engine as _;
use gpui::{SharedString, prelude::*};
use gpui_component::{
    Icon, IconName, Sizable as _, Theme, h_flex,
    menu::{PopupMenu, PopupMenuItem},
    v_flex,
};

/// Static row for the `+` and `⁄` menus: an icon, a name, and a description.
struct MenuRow {
    icon: IconName,
    name: &'static str,
    desc: &'static str,
}

/// `+` menu "添加" group. Only `文件和文件夹` carries behavior (handled by the caller); the rest
/// are static decoration mirroring Codex.app.
const PLUS_ADD_ROWS: &[MenuRow] = &[
    MenuRow {
        icon: IconName::Folder,
        name: "文件和文件夹",
        desc: "",
    },
    MenuRow {
        icon: IconName::SquareTerminal,
        name: "附加 Zed",
        desc: "",
    },
    MenuRow {
        icon: IconName::CircleCheck,
        name: "目标",
        desc: "设置持续努力实现的目标",
    },
    MenuRow {
        icon: IconName::LayoutDashboard,
        name: "计划模式",
        desc: "开启计划模式",
    },
];

/// `+` menu "插件" group — all static decoration.
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

/// `⁄` menu skills group. The `bool` is whether the skill is "个人" (personal) vs "系统" (system).
const SLASH_SKILL_ROWS: &[(&str, &str, bool)] = &[
    (
        "Browser",
        "Browser lets Codex open and control the in-app browser",
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

/// Render one menu row (icon + name + muted description) as a popup-menu element item.
fn menu_row_item(row: &'static MenuRow, theme: &Theme) -> PopupMenuItem {
    let fg = theme.foreground;
    let muted = theme.muted_foreground;
    let (icon, name, desc) = (row.icon.clone(), row.name, row.desc);
    PopupMenuItem::element(move |_window, _cx| {
        let mut line = h_flex()
            .items_center()
            .gap_2()
            .child(Icon::new(icon.clone()).small().text_color(muted))
            .child(gpui::div().text_sm().text_color(fg).child(name));
        if !desc.is_empty() {
            line = line.child(gpui::div().text_xs().text_color(muted).child(desc));
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

/// Build the `+` popup menu. `on_files` runs when the "文件和文件夹" row is
/// clicked (index 0); `on_plan` runs when the "计划模式" row is clicked (index 3).
pub fn build_plus_menu(
    menu: PopupMenu,
    theme: &Theme,
    on_files: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
    on_plan: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
) -> PopupMenu {
    let mut menu = menu.max_w(gpui::px(360.)).scrollable(true);
    menu = menu.label("添加");
    // Index 0 ("文件和文件夹") and index 3 ("计划模式") are real actions.
    let on_files = std::rc::Rc::new(on_files);
    let on_plan = std::rc::Rc::new(on_plan);
    for (ix, row) in PLUS_ADD_ROWS.iter().enumerate() {
        match ix {
            0 => {
                let on_files = on_files.clone();
                menu = menu.item(
                    menu_row_item(row, theme).on_click(move |_, window, cx| on_files(window, cx)),
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
    menu = menu.separator().label("插件");
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
    menu = menu.label("命令");
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
    menu = menu.separator().label("记忆");
    menu = menu.item(PopupMenuItem::Label("生成开".into()));
    menu = menu.separator().label("技能");
    let fg = theme.foreground;
    let muted = theme.muted_foreground;
    for (name, desc, personal) in SLASH_SKILL_ROWS {
        let tag = if *personal { "个人" } else { "系统" };
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
                .child(gpui::div().text_xs().text_color(muted).child(tag))
        }));
    }
    menu
}

/// A file the user picked in the `+` menu but has not yet submitted.
#[derive(Debug, Clone)]
pub struct PendingAttachment {
    pub path: PathBuf,
    pub is_image: bool,
}

impl PendingAttachment {
    pub fn new(path: PathBuf) -> Self {
        let is_image = is_image_path(&path);
        Self { path, is_image }
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string()
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

/// Read one attachment from disk into a [`MessageContent`]. Images become base64 image blocks;
/// text files are inlined into `text_out` wrapped in `<file>` tags. Blocking file IO — call from a
/// background executor.
pub fn load_attachment(att: &PendingAttachment, text_out: &mut String) -> Option<MessageContent> {
    if att.is_image {
        let bytes = std::fs::read(&att.path).ok()?;
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Some(MessageContent::Image {
            data,
            mime_type: mime_for(&att.path).to_string(),
        })
    } else {
        let content = std::fs::read_to_string(&att.path).ok()?;
        text_out.push_str(&format!(
            "\n<file path=\"{}\">\n{content}\n</file>",
            att.path.display()
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
        let icon = if att.is_image {
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
