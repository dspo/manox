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
//! current thread and start a fresh one that keeps the project, permission
//! mode, and model, see [`NewCommand`]), and `/mode` (cycle or set the
//! thread's permission mode, optionally with a prompt that starts working
//! immediately, see [`ModeCommand`]); markdown prompt-macros and skills are
//! mirrored into the registry at startup from the shared `agent::command` /
//! `agent::skill` registries ([`MarkdownSlashCommand`] /
//! [`SkillSlashCommand`]).

use std::sync::{Arc, OnceLock};

use gpui::{App, Context, SharedString, Window};

use agent::command::CommandDefinition;
use agent::i18n;
use agent::skill::SkillDefinition;

use crate::conversation::NoticeAnchor;
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
    /// performing any side effects (e.g. `/plan fix it` enables plan mode then
    /// runs the prompt). The `String` is the text to send (may differ from input).
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
    /// Canonical name without the leading `/` (e.g. `mode`).
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
    let mut commands: Vec<Box<dyn SlashCommand>> = vec![
        Box::new(ModeCommand),
        Box::new(PlanCommand),
        Box::new(CompactCommand),
        Box::new(ExitCommand),
        Box::new(NewCommand),
        Box::new(GoalCommand),
    ];
    // Names already claimed by built-ins and (below) markdown macros, so a
    // skill sharing one is skipped — keeps one popover row per name and routes
    // dispatch to the higher-priority command/built-in. The built-in set is
    // shared with the headless actor's surface via
    // `agent::slash_builtins`, so the two hosts enumerate the same commands.
    let mut command_keys: std::collections::HashSet<String> = std::collections::HashSet::from_iter(
        agent::slash_builtins::BUILTIN_SLASH_COMMANDS
            .iter()
            .map(|meta| meta.name.to_string()),
    );
    // Mirror every loaded markdown prompt-macro (`/gitwork:deliver`, etc.) into
    // the registry so `parse` recognizes them and the `⁄` popover lists them.
    // The adapter delegates to `Workspace::run_command_turn` →
    // `Thread::submit_command`, which substitutes `$ARGUMENTS` into the body
    // (the retired manox harness additionally applied the macro's
    // `allowed-tools` turn filter).
    // `agent::command::try_global` is `None` only before `agent::init` (which
    // `main` calls before us); fall back to no macros rather than panicking.
    for (key, def) in agent::command::try_global()
        .map(|r| r.entries())
        .unwrap_or_default()
    {
        // A macro sharing a built-in name (e.g. `commands/mode.md`) is skipped —
        // the built-in wins, mirroring the skill-skip rule below, so the popover
        // never shows two rows for the same name.
        if command_keys.contains(key.as_str()) {
            continue;
        }
        command_keys.insert(key.clone());
        commands.push(
            Box::new(MarkdownSlashCommand::new(key.clone(), def.clone())) as Box<dyn SlashCommand>,
        );
    }
    // Mirror every loaded skill (`/gitwork:deliver`, bare `/skill`, etc.) the
    // same way. Skills dispatch to `Workspace::run_skill_turn` →
    // `Thread::submit_skill`, which injects the skill body as the turn's user
    // message. A command and a skill may share a key (`gitwork:deliver`); the
    // command wins — skip a skill whose key an already-registered command owns,
    // so the popover shows one row and `parse`/`dispatch` hit the command path.
    for (key, def) in agent::skill::try_global()
        .map(|r| r.entries())
        .unwrap_or_default()
    {
        if command_keys.contains(key.as_str()) {
            continue;
        }
        commands.push(
            Box::new(SkillSlashCommand::new(key.clone(), def.clone())) as Box<dyn SlashCommand>
        );
    }
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
    fn description(&self) -> SharedString {
        SharedString::from(self.def.description.clone())
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

/// Adapter wrapping a `SkillDefinition` as a `SlashCommand`, so a plugin skill
/// (`/gitwork:deliver`) or user-authored skill (`/myskill`) is slash-invocable
/// the way it is in Claude Code. The `key` is the full registry lookup name
/// (`plugin:skill` or bare `skill`), matching what the user types and what
/// `parse` looks up. `execute` delegates to `Workspace::run_skill_turn`, which
/// pushes the display bubble and injects the skill body as the turn's user
/// message via `Thread::submit_skill`.
struct SkillSlashCommand {
    key: String,
    def: Arc<SkillDefinition>,
}

impl SkillSlashCommand {
    fn new(key: String, def: Arc<SkillDefinition>) -> Self {
        Self { key, def }
    }
}

impl SlashCommand for SkillSlashCommand {
    fn name(&self) -> &str {
        &self.key
    }
    fn description(&self) -> SharedString {
        SharedString::from(self.def.description.clone())
    }
    fn kind(&self) -> CompletionKind {
        CompletionKind::Skill
    }
    fn execute(
        &self,
        args: &str,
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> SlashResult {
        workspace.run_skill_turn(&self.key, args, cx);
        SlashResult::Handled
    }
}

/// The shared built-in metadata for a command by canonical name. Panics on a
/// miss so a typo in a command impl is caught at startup, never at runtime.
fn builtin_meta(name: &str) -> &'static agent::slash_builtins::BuiltinSlashMeta {
    agent::slash_builtins::canonical_builtin(name).expect("registered built-in command")
}

/// `/plan` — toggle plan mode. Entering wires the read-only gate and the
/// plan-mode research instructions; `/plan <prompt>` also starts planning
/// the prompt immediately. Running `/plan` again exits plan mode (full
/// write access restored). Plans are submitted for approval through the
/// `ProposePlan` tool, never as prose.
/// `/mode` — cycle or set the permission mode on the current thread.
///
/// `/mode` (no args) cycles ReadOnly → WorkspaceWrite → DangerFullAccess and
/// pushes a notice. `/mode <name>` sets the named mode (`read-only`,
/// `workspace-write`, `danger-full-access`); an optional prompt after the mode
/// name immediately starts a turn under the new mode.
struct ModeCommand;

impl SlashCommand for ModeCommand {
    fn name(&self) -> &str {
        builtin_meta("mode").name
    }
    fn description(&self) -> SharedString {
        i18n::t(builtin_meta("mode").description_key)
    }
    fn execute(
        &self,
        args: &str,
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> SlashResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            workspace.cycle_mode(cx);
            return SlashResult::Handled;
        }
        let (name, rest) = match trimmed.split_once(char::is_whitespace) {
            Some((head, tail)) => (head, tail.trim()),
            None => (trimmed, ""),
        };
        let parsed: Result<agent::thread::PermissionMode, _> =
            serde_json::from_value(serde_json::Value::String(name.to_string()));
        match parsed {
            Ok(mode) => {
                if rest.is_empty() {
                    workspace.apply_permission_mode(mode, cx);
                } else {
                    workspace.start_mode_turn(mode, rest.to_string(), cx);
                }
            }
            Err(_) => {
                workspace.add_info_message(
                    i18n::t_str("slash-mode-unknown", &[("mode", name)]).to_string(),
                    NoticeAnchor::TurnEnd,
                    None,
                    cx,
                );
            }
        }
        SlashResult::Handled
    }
}

struct PlanCommand;

impl SlashCommand for PlanCommand {
    fn name(&self) -> &str {
        builtin_meta("plan").name
    }
    fn description(&self) -> SharedString {
        i18n::t(builtin_meta("plan").description_key)
    }
    fn execute(
        &self,
        args: &str,
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> SlashResult {
        // Toggle plan mode. Entering: the engine wires the read-only gate
        // and injects the plan-mode instructions every turn; a prompt passed
        // alongside becomes the first planning turn. Leaving: the gate lifts.
        if workspace.thread_plan_mode(cx) {
            workspace.set_thread_plan_mode(false, cx);
            workspace.add_info_message(
                i18n::t("plan-mode-off-notice").to_string(),
                NoticeAnchor::TurnEnd,
                None,
                cx,
            );
            return SlashResult::Handled;
        }
        workspace.set_thread_plan_mode(true, cx);
        if args.trim().is_empty() {
            workspace.add_info_message(
                i18n::t("plan-mode-on-notice").to_string(),
                NoticeAnchor::TurnEnd,
                None,
                cx,
            );
            SlashResult::Handled
        } else {
            SlashResult::InjectUserTurn(args.to_string())
        }
    }
}

/// `/compact` — manually trigger a context-compaction pass on the current
/// thread. Summarizes older history into a handoff message, keeping a recent
/// user-message tail verbatim. No-op when a turn is in flight or there is
/// nothing to summarize; the side LLM call runs in a spawned task and the
/// result lands as a Recap card.
struct CompactCommand;

impl SlashCommand for CompactCommand {
    fn name(&self) -> &str {
        builtin_meta("compact").name
    }
    fn description(&self) -> SharedString {
        i18n::t(builtin_meta("compact").description_key)
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
        thread.with_mut(|t| t.compact(instructions));
        cx.notify();
        SlashResult::Handled
    }
}

/// `/exit` (alias `/quit`) — archive the current thread and start a fresh
/// one.
struct ExitCommand;

/// `/goal` manages the durable Goal lifecycle (ported from the retired
/// manox harness). Replacing an unfinished Goal requires the explicit
/// `/goal replace <objective>` confirmation command; a bare `/goal` opens
/// the status popover (existing goal) or prefills a new one.
struct GoalCommand;

impl SlashCommand for GoalCommand {
    fn name(&self) -> &str {
        builtin_meta("goal").name
    }
    fn description(&self) -> SharedString {
        i18n::t(builtin_meta("goal").description_key)
    }
    fn execute(
        &self,
        args: &str,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> SlashResult {
        let thread = workspace.thread.clone();
        let trimmed = args.trim();
        if let Some(objective) = trimmed.strip_prefix("replace ").map(str::trim) {
            thread.with_mut(|t| {
                if let Err(error) = t.replace_goal(
                    objective.to_string(),
                    None,
                    None,
                    agent::db::GoalActor::User,
                ) {
                    tracing::warn!(error = %error, "goal op failed");
                }
            });
            return SlashResult::Handled;
        }
        if let Some(objective) = trimmed.strip_prefix("edit ").map(str::trim) {
            thread.with_mut(|t| {
                let current = t.goal();
                let budget = current.as_ref().and_then(|goal| goal.token_budget);
                let max_rounds = current.as_ref().and_then(|goal| goal.max_rounds);
                if let Err(error) = t.edit_goal(
                    objective.to_string(),
                    budget,
                    max_rounds,
                    agent::db::GoalActor::User,
                ) {
                    tracing::warn!(error = %error, "goal op failed");
                }
            });
            return SlashResult::Handled;
        }
        if let Some(value) = trimmed.strip_prefix("budget ").map(str::trim) {
            thread.with_mut(|t| {
                let Some(goal) = t.goal() else {
                    tracing::warn!("thread has no Goal");
                    return;
                };
                let budget = if matches!(value, "none" | "unlimited") {
                    None
                } else {
                    match value.parse::<u64>() {
                        Ok(value) => Some(value),
                        Err(error) => {
                            tracing::warn!(error = %error, "goal op failed");
                            return;
                        }
                    }
                };
                if let Err(error) = t.edit_goal(
                    goal.objective,
                    budget,
                    goal.max_rounds,
                    agent::db::GoalActor::User,
                ) {
                    tracing::warn!(error = %error, "goal op failed");
                }
            });
            return SlashResult::Handled;
        }
        if let Some(value) = trimmed.strip_prefix("rounds ").map(str::trim) {
            thread.with_mut(|t| {
                let Some(goal) = t.goal() else {
                    tracing::warn!("thread has no Goal");
                    return;
                };
                let max_rounds = if matches!(value, "none" | "unlimited") {
                    None
                } else {
                    match value.parse::<u64>() {
                        Ok(value) => Some(value),
                        Err(error) => {
                            tracing::warn!(error = %error, "goal op failed");
                            return;
                        }
                    }
                };
                if let Err(error) = t.edit_goal(
                    goal.objective,
                    goal.token_budget,
                    max_rounds,
                    agent::db::GoalActor::User,
                ) {
                    tracing::warn!(error = %error, "goal op failed");
                }
            });
            return SlashResult::Handled;
        }
        match trimmed.to_lowercase().as_str() {
            "" => {
                if thread.read(|t| t.goal().is_some()) {
                    workspace.open_goal_popover(cx);
                } else {
                    workspace.begin_goal_new(window, cx);
                }
                SlashResult::Handled
            }
            "clear" => {
                thread.with_mut(|t| {
                    if let Err(error) = t.clear_goal(agent::db::GoalActor::User) {
                        tracing::warn!(error = %error, "goal op failed");
                    }
                });
                cx.notify();
                SlashResult::Handled
            }
            "pause" | "stop" => {
                thread.with_mut(|t| {
                    if let Err(error) = t.set_goal_status(
                        agent::goal::GoalStatus::Paused,
                        Some(agent::goal::GoalBlockReason {
                            code: "user-paused".into(),
                            message: "paused by user".into(),
                        }),
                        agent::db::GoalActor::User,
                    ) {
                        tracing::warn!(error = %error, "goal op failed");
                    }
                });
                SlashResult::Handled
            }
            "resume" => {
                thread.with_mut(|t| {
                    if let Err(error) = t.set_goal_status(
                        agent::goal::GoalStatus::Active,
                        None,
                        agent::db::GoalActor::User,
                    ) {
                        tracing::warn!(error = %error, "goal op failed");
                    }
                });
                SlashResult::Handled
            }

            "edit" => {
                workspace.begin_goal_edit(window, cx);
                SlashResult::Handled
            }
            "replace" => {
                workspace.begin_goal_replace(window, cx);
                SlashResult::Handled
            }
            _ => {
                let needs_confirmation = thread
                    .read(|t| t.goal())
                    .is_some_and(|goal| goal.status != agent::goal::GoalStatus::Complete);
                if needs_confirmation {
                    workspace.begin_goal_replace_with_objective(trimmed, window, cx);
                    return SlashResult::Handled;
                }
                let created = thread.with_mut(|t| match t.set_goal(trimmed.to_string()) {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::warn!(error = %error, "goal op failed");
                        false
                    }
                });
                if !created {
                    return SlashResult::Handled;
                }
                cx.notify();
                SlashResult::InjectUserTurn(trimmed.to_string())
            }
        }
    }
}

impl SlashCommand for ExitCommand {
    fn name(&self) -> &str {
        builtin_meta("exit").name
    }
    fn description(&self) -> SharedString {
        i18n::t(builtin_meta("exit").description_key)
    }
    fn aliases(&self) -> &[&str] {
        builtin_meta("exit").aliases
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
/// the outgoing thread's project, permission mode, and model: the conversation
/// starts empty but keeps the working context.
struct NewCommand;
impl SlashCommand for NewCommand {
    fn name(&self) -> &str {
        builtin_meta("new").name
    }
    fn description(&self) -> SharedString {
        i18n::t(builtin_meta("new").description_key)
    }
    fn aliases(&self) -> &[&str] {
        builtin_meta("new").aliases
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
            assert!(r.get("mode").is_none());
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
