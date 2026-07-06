//! Slash-command parsing and dispatch infrastructure.
//!
//! A slash command is a line-initial `/name [args]` token in the composer.
//! On submit, [`parse`] checks the input against the [`SlashCommandRegistry`];
//! a hit dispatches to the command's [`SlashCommand::execute`] instead of
//! sending a normal user turn. Unrecognized `/foo` text falls through as a
//! plain user message (the model may interpret it freely).
//!
//! The registry is a process-global `OnceLock` populated once at startup
//! ([`init`]). Each command is an erased `&'static dyn SlashCommand`. The
//! `⁄` popover in the composer lists registered commands dynamically.
//!
//! Two commands ship today: the mock `/yolo` (placeholder, real YOLO lands in
//! a follow-up PR) and the live `/plan` (enter/exit plan mode, see
//! [`PlanCommand`]).

use std::sync::{Arc, OnceLock};

use gpui::{App, Context, Window};

use agent::command::CommandDefinition;

use crate::workspace::Workspace;

/// Result of dispatching a slash command.
#[derive(Debug, Default)]
pub enum SlashResult {
    /// The command handled the input fully; the composer should clear and not
    /// send a user turn (e.g. a toggle command).
    #[default]
    Handled,
    /// The command wants the remaining text sent as a normal user turn after
    /// performing any side effects (e.g. `/yolo fix it` enables YOLO then runs
    /// the prompt). The `String` is the text to send (may differ from input).
    InjectUserTurn(String),
    /// The command did nothing; the input should be treated as a normal
    /// message. Distinct from `Handled` so the caller can fall back to
    /// `send_user_turn` instead of clearing the box.
    NoOp,
}

/// A parsed slash command invocation: the command name and the trailing args
/// (text after the first space, trimmed; empty string when no args).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSlash {
    pub name: String,
    pub args: String,
}

/// A single slash command. Commands operate on the active [`Workspace`] via a
/// typed `Context<Workspace>` so they can toggle thread state, push messages,
/// etc., exactly like inline workspace methods.
pub trait SlashCommand: Send + Sync {
    /// Canonical name without the leading `/` (e.g. `yolo`).
    fn name(&self) -> &str;
    /// One-line description shown in the `⁄` popover.
    fn description(&self) -> &str;
    /// Execute the command. `args` is the trailing text after the command name.
    fn execute(
        &self,
        args: &str,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> SlashResult;
}

/// Process-global registry of slash commands.
static REGISTRY: OnceLock<SlashCommandRegistry> = OnceLock::new();

/// Holds the registered commands; constructed once via [`init`].
pub struct SlashCommandRegistry {
    commands: Vec<Box<dyn SlashCommand>>,
}

impl SlashCommandRegistry {
    fn new(commands: Vec<Box<dyn SlashCommand>>) -> Self {
        Self { commands }
    }

    /// The global registry; `None` before [`init`] is called.
    pub fn global() -> Option<&'static SlashCommandRegistry> {
        REGISTRY.get()
    }

    /// Look up a command by name.
    pub fn get(&self, name: &str) -> Option<&dyn SlashCommand> {
        self.commands
            .iter()
            .find(|c| c.name() == name)
            .map(|c| c.as_ref())
    }

    /// Iterate all registered commands (for building the `⁄` popover).
    pub fn commands(&self) -> impl Iterator<Item = &dyn SlashCommand> {
        self.commands.iter().map(|c| c.as_ref())
    }
}

/// Register the built-in slash commands. Call once during app startup, before
/// any workspace is created — and after `agent::init`, which populates the
/// markdown command registry the macro adapters mirror. Idempotent via
/// `OnceLock::set`.
pub fn init(_cx: &mut App) {
    let mut commands: Vec<Box<dyn SlashCommand>> = vec![
        // Mock /yolo: pushes a placeholder notice. The real YOLO
        // implementation replaces this command with a live toggle + execution
        // in a follow-up PR.
        Box::new(MockYoloCommand),
        Box::new(PlanCommand),
    ];
    // Mirror every loaded markdown prompt-macro (`/gitwork:deliver`, etc.) into
    // the registry so `parse` recognizes them and the `⁄` popover lists them.
    // The adapter delegates to `Workspace::run_command_turn`, which substitutes
    // `$ARGUMENTS` and applies `allowed-tools` via `Thread::submit_command`.
    // `agent::command::try_global` is `None` only before `agent::init` (which
    // `main` calls before us); fall back to no macros rather than panicking.
    commands.extend(
        agent::command::try_global()
            .map(|r| r.entries())
            .unwrap_or_default()
            .into_iter()
            .map(|(key, def)| {
                Box::new(MarkdownSlashCommand::new(key.clone(), def.clone()))
                    as Box<dyn SlashCommand>
            }),
    );
    let _ = REGISTRY.set(SlashCommandRegistry::new(commands));
}

/// Parse a raw composer input into a slash command invocation.
///
/// Rules (matching codex/zed conventions):
/// - The command must be at the very start of the (trimmed) input, preceded
///   only by whitespace.
/// - The name is the first whitespace-delimited token, with the leading `/`
///   stripped. Everything after the first space is `args` (trimmed).
/// - Returns `None` when the input does not start with `/`, the token is only
///   `/`, or the name is not a registered command. Unrecognized `/foo` thus
///   falls through as a normal user message rather than erroring.
pub fn parse(input: &str) -> Option<ParsedSlash> {
    let trimmed = input.trim_start();
    let rest = trimmed.strip_prefix('/')?;
    if rest.is_empty() {
        return None;
    }
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (rest, ""),
    };
    // Only treat as a command if the name is registered; otherwise the input
    // is a plain user message the model may interpret freely.
    if REGISTRY.get().and_then(|r| r.get(name)).is_some() {
        Some(ParsedSlash {
            name: name.to_string(),
            args: args.to_string(),
        })
    } else {
        None
    }
}

/// Dispatch a parsed slash command against the given workspace.
pub fn dispatch(
    parsed: &ParsedSlash,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> SlashResult {
    let Some(registry) = REGISTRY.get() else {
        return SlashResult::NoOp;
    };
    let Some(cmd) = registry.get(&parsed.name) else {
        return SlashResult::NoOp;
    };
    cmd.execute(&parsed.args, workspace, window, cx)
}

// ─── built-in commands (mock) ─────────────────────────────────────────────

/// Placeholder `/yolo` command. Pushes an info notice; no real effect.
/// Replaced by the live YOLO toggle in the follow-up PR.
struct MockYoloCommand;

impl SlashCommand for MockYoloCommand {
    fn name(&self) -> &str {
        "yolo"
    }
    fn description(&self) -> &str {
        "切换 YOLO 模式（mock：暂无实际效果）"
    }
    fn execute(
        &self,
        _args: &str,
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> SlashResult {
        workspace.add_info_message(
            "/yolo 命令已识别（mock）。真实 YOLO 模式将在后续 PR 实现。".to_string(),
            cx,
        );
        SlashResult::Handled
    }
}

/// Adapter wrapping a markdown prompt-macro `CommandDefinition` as a
/// `SlashCommand`. The `key` is the full registry key (`gitwork:deliver`), not
/// the bare filename stem, so `parse` matches what the user actually types.
/// `execute` delegates to `Workspace::run_command_turn`, which pushes the
/// display bubble, substitutes `$ARGUMENTS` into the body, and applies the
/// command's `allowed-tools` whitelist for the turn.
struct MarkdownSlashCommand {
    key: String,
    def: Arc<CommandDefinition>,
}

impl MarkdownSlashCommand {
    fn new(key: String, def: Arc<CommandDefinition>) -> Self {
        Self { key, def }
    }
}

impl SlashCommand for MarkdownSlashCommand {
    fn name(&self) -> &str {
        &self.key
    }
    fn description(&self) -> &str {
        &self.def.description
    }
    fn execute(
        &self,
        args: &str,
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> SlashResult {
        workspace.run_command_turn(&self.key, args, cx);
        SlashResult::Handled
    }
}

/// `/plan` — enter or exit plan mode.
///
/// - No args: toggle plan mode and consume the input (nothing sent to the
///   model). The state change is reflected in the access chip.
/// - With args: ensure plan mode is on, then send `args` as a normal user
///   message so the agent plans against that prompt. `set_plan_mode(true)`
///   runs before `InjectUserTurn` returns, so the turn `submit_input` then
///   launches builds its request with the read-only tool filter already active.
struct PlanCommand;

impl SlashCommand for PlanCommand {
    fn name(&self) -> &str {
        "plan"
    }
    fn description(&self) -> &str {
        "进入计划模式：仅允许只读工具，研究后提交计划待批准（裸 `/plan` 切换；`/plan <提示>` 带提示进入）"
    }
    fn execute(
        &self,
        args: &str,
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> SlashResult {
        let thread = workspace.thread.clone();
        let in_plan = thread.read(cx).plan_mode();
        if args.is_empty() {
            thread.update(cx, |t, cx| t.set_plan_mode(!in_plan, cx));
            cx.notify();
            SlashResult::Handled
        } else {
            if !in_plan {
                thread.update(cx, |t, cx| t.set_plan_mode(true, cx));
            }
            cx.notify();
            SlashResult::InjectUserTurn(args.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_command() {
        register_for_tests();
        let p = parse("/yolo").unwrap();
        assert_eq!(p.name, "yolo");
        assert_eq!(p.args, "");
    }

    #[test]
    fn parse_command_with_args() {
        register_for_tests();
        let p = parse("/yolo fix the bug").unwrap();
        assert_eq!(p.name, "yolo");
        assert_eq!(p.args, "fix the bug");
    }

    #[test]
    fn parse_leading_whitespace_ok() {
        register_for_tests();
        let p = parse("   /yolo hi").unwrap();
        assert_eq!(p.name, "yolo");
        assert_eq!(p.args, "hi");
    }

    #[test]
    fn parse_bare_slash_is_none() {
        register_for_tests();
        assert!(parse("/").is_none());
        assert!(parse("   /   ").is_none());
    }

    #[test]
    fn parse_non_command_text_is_none() {
        register_for_tests();
        assert!(parse("hello world").is_none());
        assert!(parse("/unknowncmd hi").is_none());
    }

    #[test]
    fn parse_inline_slash_is_none() {
        // Slash not at line start must not be treated as a command.
        register_for_tests();
        assert!(parse("hello /yolo").is_none());
    }

    #[test]
    fn registry_lookup() {
        register_for_tests();
        let r = REGISTRY.get().unwrap();
        assert!(r.get("yolo").is_some());
        assert!(r.get("plan").is_some());
        assert!(r.get("nope").is_none());
    }

    #[test]
    fn parse_plan_command() {
        // `/plan` bare and `/plan <prompt>` both parse once /plan is registered.
        register_for_tests();
        let p = parse("/plan").unwrap();
        assert_eq!(p.name, "plan");
        assert_eq!(p.args, "");
        let p = parse("/plan fix the auth flow").unwrap();
        assert_eq!(p.name, "plan");
        assert_eq!(p.args, "fix the auth flow");
    }

    /// Ensure the registry is populated for tests (idempotent).
    fn register_for_tests() {
        if REGISTRY.get().is_some() {
            return;
        }
        let _ = REGISTRY.set(SlashCommandRegistry::new(vec![
            Box::new(MockYoloCommand),
            Box::new(PlanCommand),
        ]));
    }
}
