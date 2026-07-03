//! Composer `+` and `⁄` menus plus pending-attachment support.
//!
//! The `+` menu mirrors Codex.app's "add / plugins" popover; only "文件和文件夹" is wired to a
//! real file picker, the rest are static decoration. The `⁄` menu mirrors Codex's slash-command
//! popover and is entirely static — manox has no slash-command or skill infrastructure yet.
//!
//! A [`PendingAttachment`] is a file the user picked but has not yet sent. On submit the workspace
//! turns each into message content: images become base64 [`MessageContent::Image`] blocks, text
//! files are inlined into the message text wrapped in `<file>` tags.

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

/// `⁄` menu command group — all static decoration.
const SLASH_COMMAND_ROWS: &[MenuRow] = &[
    MenuRow {
        icon: IconName::Network,
        name: "MCP",
        desc: "显示 MCP 服务器状态",
    },
    MenuRow {
        icon: IconName::CircleUser,
        name: "个性",
        desc: "选择回应方式",
    },
    MenuRow {
        icon: IconName::Search,
        name: "代码审查",
        desc: "审查未暂存的更改，或与某个分支进行比较",
    },
    MenuRow {
        icon: IconName::PanelLeft,
        name: "侧边",
        desc: "在临时分支中发起侧边对话",
    },
    MenuRow {
        icon: IconName::File,
        name: "初始化",
        desc: "创建包含说明的 AGENTS.md 文件",
    },
    MenuRow {
        icon: IconName::Frame,
        name: "压缩",
        desc: "压缩此会话的上下文",
    },
    MenuRow {
        icon: IconName::Heart,
        name: "反馈",
        desc: "发送有关此聊天的反馈",
    },
    MenuRow {
        icon: IconName::Bot,
        name: "宠物",
        desc: "唤醒或收起桌面宠物",
    },
    MenuRow {
        icon: IconName::Copy,
        name: "派生",
        desc: "为此对话创建本地分支对话",
    },
    MenuRow {
        icon: IconName::Info,
        name: "状态",
        desc: "显示对话 ID、上下文使用情况及额度跟踪",
    },
    MenuRow {
        icon: IconName::CircleCheck,
        name: "目标",
        desc: "设置持续努力实现的目标",
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

/// Build the `+` popup menu. `on_files` runs when the real "文件和文件夹" row is clicked.
pub fn build_plus_menu(
    menu: PopupMenu,
    theme: &Theme,
    on_files: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
) -> PopupMenu {
    let mut menu = menu.max_w(gpui::px(360.)).scrollable(true);
    menu = menu.label("添加");
    // First "添加" row ("文件和文件夹") is the only real action.
    let on_files = std::rc::Rc::new(on_files);
    for (ix, row) in PLUS_ADD_ROWS.iter().enumerate() {
        if ix == 0 {
            let on_files = on_files.clone();
            menu = menu.item(
                menu_row_item(row, theme).on_click(move |_, window, cx| on_files(window, cx)),
            );
        } else {
            menu = menu.item(menu_row_item(row, theme));
        }
    }
    menu = menu.separator().label("插件");
    for row in PLUS_PLUGIN_ROWS {
        menu = menu.item(menu_row_item(row, theme));
    }
    menu
}

/// Build the `⁄` popup menu — entirely static decoration.
pub fn build_slash_menu(menu: PopupMenu, theme: &Theme) -> PopupMenu {
    let mut menu = menu.max_w(gpui::px(420.)).scrollable(true);
    for row in SLASH_COMMAND_ROWS {
        menu = menu.item(menu_row_item(row, theme));
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
