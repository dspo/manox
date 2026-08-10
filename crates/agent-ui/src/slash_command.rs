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
//! Built-in commands (registered in [`init`]): `/compact` (manual context
//! compaction, optional focus instructions, see [`CompactCommand`]), `/exit`
//! (alias `/quit`; archive the current thread and start a fresh one, see
//! [`ExitCommand`]), and `/new` (aliases `/clear`, `/archive`; archive the
//! current thread and start a fresh one that keeps the project, approval
//! mode, and model, see [`NewCommand`]). The retired manox harness
//! additionally had `/danger`, `/plan`, `/goal`, markdown prompt-macro, and
//! skill adapters — see git history (`origin/Manox` backup branch) for those
//! flows.

use std::sync::OnceLock;

use gpui::{App, Context, SharedString, Window};

use agent::i18n;

use crate::views::completion::CompletionKind;
use crate::workspace::Workspace;

/// Result of dispatching a slash command.
#[derive(Debug, Default)]
pub enum SlashResult {
    /// The command handled the input fully; the composer should clear and not
    /// send a user turn (e.g. a toggle command).
    #[default]
    Handled,
    /// The command wants the remaining text sent as a normal user turn after
    /// performing any side effects (e.g. `/danger fix it` enables Danger then runs
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
    /// Canonical name without the leading `/` (e.g. `danger`).
    fn name(&self) -> &str;
    /// One-line description shown in the `⁄` popover. Localized via `i18n` for
    /// built-in commands; markdown-defined commands return their frontmatter
    /// description verbatim (author-chosen language).
    fn description(&self) -> SharedString;
    /// Kind shown as the row icon + tag in the `⁄` popover. Defaults to
    /// `Command`; `SkillSlashCommand` overrides to `Skill` so plugin skills
    /// mirrored into the registry render with the skill icon.
    fn kind(&self) -> CompletionKind {
        CompletionKind::Command
    }
    /// Alternate invocation names (`/quit` for `/exit`). Lookup matches the
    /// canonical name or any alias; the `⁄` popover lists only the canonical
    /// name.
    fn aliases(&self) -> &[&str] {
        &[]
    }
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

    /// Look up a command by canonical name or alias.
    pub fn get(&self, name: &str) -> Option<&dyn SlashCommand> {
        self.commands
            .iter()
            .find(|c| c.name() == name || c.aliases().contains(&name))
            .map(|c| c.as_ref())
    }

    /// Iterate all registered commands (for building the `⁄` popover).
    pub fn commands(&self) -> impl Iterator<Item = &dyn SlashCommand> {
        self.commands.iter().map(|c| c.as_ref())
    }
}

/// Register the built-in slash commands. Call once during app startup, before
/// any workspace is created. Idempotent via `OnceLock::set`.
pub fn init(_cx: &mut App) {
    let commands: Vec<Box<dyn SlashCommand>> = vec![
        Box::new(CompactCommand),
        Box::new(ExitCommand),
        Box::new(NewCommand),
    ];
    let _ = REGISTRY.set(SlashCommandRegistry::new(commands));
}

/// Parse a raw composer input into a slash command invocation.
///
/// Rules:
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

// ─── built-in commands ─────────────────────────────────────────────────────

/// `/compact` — manually trigger a context-compaction pass on the current
/// thread. Summarizes older history into a handoff message, keeping a recent
/// user-message tail verbatim. No-op when a turn is in flight or there is
/// nothing to summarize; the side LLM call runs in a spawned task and the
/// result lands as a Recap card.
struct CompactCommand;

impl SlashCommand for CompactCommand {
    fn name(&self) -> &str {
        "compact"
    }
    fn description(&self) -> SharedString {
        i18n::t("slash-compact-desc")
    }
    fn execute(
        &self,
        args: &str,
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> SlashResult {
        let trimmed = args.trim();
        let instructions = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        let thread = workspace.thread.clone();
        thread.update(cx, |t, cx| t.compact(instructions, cx));
        cx.notify();
        SlashResult::Handled
    }
}

/// `/exit` (alias `/quit`) — archive the current thread and start a fresh
/// one.
struct ExitCommand;

impl SlashCommand for ExitCommand {
    fn name(&self) -> &str {
        "exit"
    }
    fn description(&self) -> SharedString {
        i18n::t("slash-exit-desc")
    }
    fn aliases(&self) -> &[&str] {
        &["quit"]
    }
    fn execute(
        &self,
        _args: &str,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> SlashResult {
        workspace.archive_current_thread(window, cx);
        SlashResult::Handled
    }
}

/// `/new` — archive the current thread and open a fresh one that inherits
/// the outgoing thread's project, approval mode, and model: the conversation
/// starts empty but keeps the working context.
struct NewCommand;

impl SlashCommand for NewCommand {
    fn name(&self) -> &str {
        "new"
    }
    fn description(&self) -> SharedString {
        i18n::t("slash-new-desc")
    }
    fn aliases(&self) -> &[&str] {
        &["clear", "archive"]
    }
    fn execute(
        &self,
        _args: &str,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> SlashResult {
        workspace.archive_current_thread_inheriting(window, cx);
        SlashResult::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_command() {
        register_for_tests();
        let p = parse("/compact").unwrap();
        assert_eq!(p.name, "compact");
        assert_eq!(p.args, "");
    }

    #[test]
    fn parse_command_with_args() {
        register_for_tests();
        let p = parse("/compact focus on the auth flow").unwrap();
        assert_eq!(p.name, "compact");
        assert_eq!(p.args, "focus on the auth flow");
    }

    #[test]
    fn parse_leading_whitespace_ok() {
        register_for_tests();
        let p = parse("   /compact hi").unwrap();
        assert_eq!(p.name, "compact");
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
        assert!(parse("hello /compact").is_none());
    }

    #[test]
    fn registry_lookup() {
        register_for_tests();
        let r = REGISTRY.get().unwrap();
        assert!(r.get("compact").is_some());
        assert!(r.get("exit").is_some());
        assert!(r.get("new").is_some());
        assert!(r.get("nope").is_none());
        {
            // The pi registry only carries the commands its engine supports.
            assert!(r.get("danger").is_none());
            assert!(r.get("plan").is_none());
            assert!(r.get("goal").is_none());
        }
    }

    #[test]
    fn registry_lookup_resolves_aliases() {
        register_for_tests();
        let r = REGISTRY.get().unwrap();
        assert_eq!(r.get("quit").expect("/quit alias").name(), "exit");
        assert_eq!(r.get("clear").expect("/clear alias").name(), "new");
        assert_eq!(r.get("archive").expect("/archive alias").name(), "new");
    }

    #[test]
    fn parse_alias_invocations() {
        register_for_tests();
        for alias in ["/quit", "/clear", "/archive"] {
            assert!(parse(alias).is_some(), "{alias} must parse");
        }
    }

    #[test]
    fn parse_compact_command() {
        // `/compact` is a bare toggle (no args).
        register_for_tests();
        let p = parse("/compact").unwrap();
        assert_eq!(p.name, "compact");
        assert_eq!(p.args, "");
    }
    #[test]
    fn parse_exit_command() {
        register_for_tests();
        let p = parse("/exit").unwrap();
        assert_eq!(p.name, "exit");
        assert_eq!(p.args, "");
    }

    #[test]
    fn parse_new_command() {
        // `/new` is a bare command; trailing args are tolerated but ignored.
        register_for_tests();
        let p = parse("/new").unwrap();
        assert_eq!(p.name, "new");
        assert_eq!(p.args, "");
        let p = parse("/new fresh start").unwrap();
        assert_eq!(p.name, "new");
        assert_eq!(p.args, "fresh start");
    }

    /// Ensure the registry is populated for tests (idempotent). Mirrors
    /// [`init`]'s per-variant command set.
    fn register_for_tests() {
        if REGISTRY.get().is_some() {
            return;
        }
        let commands: Vec<Box<dyn SlashCommand>> = vec![
            Box::new(CompactCommand),
            Box::new(ExitCommand),
            Box::new(NewCommand),
        ];
        let _ = REGISTRY.set(SlashCommandRegistry::new(commands));
    }
}
