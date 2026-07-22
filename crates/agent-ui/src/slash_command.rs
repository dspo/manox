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
//! Built-in commands: `/yolo` (toggle YOLO mode — bypass approvals +
//! unsandboxed bash, see [`YoloCommand`]), `/plan` (enter/exit plan mode,
//! see [`PlanCommand`]), `/goal` (set a completion condition, see
//! [`GoalCommand`]), `/compact` (manual context compaction, see
//! [`CompactCommand`]), and `/exit` (archive the current thread and start
//! a fresh one, see [`ExitCommand`]). Markdown prompt-macros
//! (`/gitwork:deliver`, etc.) are mirrored in at runtime via the
//! [`MarkdownSlashCommand`] adapter, and plugin/user skills via the
//! [`SkillSlashCommand`] adapter — so `/<plugin>:<skill>` is slash-invocable
//! the way it is in Claude Code.

use std::sync::{Arc, OnceLock};

use gpui::{App, Context, SharedString, Window};

use agent::command::CommandDefinition;
use agent::i18n;
use agent::skill::SkillDefinition;

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
/// markdown command and skill registries the adapters mirror. Idempotent via
/// `OnceLock::set`.
pub fn init(_cx: &mut App) {
    let mut commands: Vec<Box<dyn SlashCommand>> = vec![
        // /yolo: toggle YOLO mode (no args), or enable YOLO and immediately run
        // the prompt as a user turn (with args). Bypasses approvals and runs
        // bash unsandboxed for the session.
        Box::new(YoloCommand),
        Box::new(PlanCommand),
        Box::new(GoalCommand),
        Box::new(CompactCommand),
        Box::new(ExitCommand),
    ];
    // Names already claimed by built-ins and (below) markdown macros, so a
    // skill sharing one is skipped — keeps one popover row per name and routes
    // dispatch to the higher-priority command/built-in.
    let mut command_keys: std::collections::HashSet<String> = std::collections::HashSet::from([
        "yolo".to_string(),
        "plan".to_string(),
        "goal".to_string(),
        "compact".to_string(),
        "exit".to_string(),
    ]);
    // Mirror every loaded markdown prompt-macro (`/gitwork:deliver`, etc.) into
    // the registry so `parse` recognizes them and the `⁄` popover lists them.
    // The adapter delegates to `Workspace::run_command_turn`, which substitutes
    // `$ARGUMENTS` and applies `allowed-tools` via `Thread::submit_command`.
    // `agent::command::try_global` is `None` only before `agent::init` (which
    // `main` calls before us); fall back to no macros rather than panicking.
    for (key, def) in agent::command::try_global()
        .map(|r| r.entries())
        .unwrap_or_default()
    {
        // A macro sharing a built-in name (e.g. `commands/yolo.md`) is skipped —
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

/// `/yolo` — toggle YOLO mode on the current thread.
///
/// `/yolo` (no args) toggles YOLO on/off and pushes a notice.
/// `/yolo [prompt]` enables YOLO (if not already on) and immediately sends
/// `prompt` as a user turn so the agent starts working with full autonomy.
struct YoloCommand;

impl SlashCommand for YoloCommand {
    fn name(&self) -> &str {
        "yolo"
    }
    fn description(&self) -> SharedString {
        i18n::t("slash-yolo-desc")
    }
    fn execute(
        &self,
        args: &str,
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> SlashResult {
        if args.is_empty() {
            workspace.toggle_yolo(cx);
            SlashResult::Handled
        } else {
            workspace.start_yolo_turn(args.to_string(), cx);
            SlashResult::Handled
        }
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
///
/// - No args: cycle the collaboration mode (Default ↔ Plan) and consume the
///   input (nothing sent to the model). The state change is reflected in the
///   mode chip.
/// - With args: switch to Plan mode, then send `args` as a normal user message
///   so the agent plans against that prompt. `set_collaboration_mode(Plan)`
///   runs before `InjectUserTurn` returns, so the turn `submit_input` then
///   launches builds its request with the read-only tool set already active.
struct PlanCommand;

impl SlashCommand for PlanCommand {
    fn name(&self) -> &str {
        "plan"
    }
    fn description(&self) -> SharedString {
        i18n::t("slash-plan-desc")
    }
    fn execute(
        &self,
        args: &str,
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> SlashResult {
        let thread = workspace.thread.clone();
        let mode = thread.read(cx).collaboration_mode();
        if args.is_empty() {
            thread.update(cx, |t, cx| t.set_collaboration_mode(mode.next(), cx));
            cx.notify();
            SlashResult::Handled
        } else {
            if mode != agent::ModeKind::Plan {
                thread.update(cx, |t, cx| {
                    t.set_collaboration_mode(agent::ModeKind::Plan, cx)
                });
            }
            cx.notify();
            SlashResult::InjectUserTurn(args.to_string())
        }
    }
}

/// `/goal` manages the durable Goal lifecycle. Replacing an unfinished Goal
/// requires the explicit `/goal replace <objective>` confirmation command.
struct GoalCommand;

impl SlashCommand for GoalCommand {
    fn name(&self) -> &str {
        "goal"
    }
    fn description(&self) -> SharedString {
        i18n::t("slash-goal-desc")
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
            thread.update(cx, |t, cx| {
                if let Err(error) =
                    t.replace_goal(objective.to_string(), None, agent::db::GoalActor::User, cx)
                {
                    cx.emit(agent::ThreadEvent::Error(error));
                }
            });
            return SlashResult::Handled;
        }
        if let Some(objective) = trimmed.strip_prefix("edit ").map(str::trim) {
            thread.update(cx, |t, cx| {
                let budget = t.goal().and_then(|goal| goal.token_budget);
                if let Err(error) = t.edit_goal(
                    objective.to_string(),
                    budget,
                    agent::db::GoalActor::User,
                    cx,
                ) {
                    cx.emit(agent::ThreadEvent::Error(error));
                }
            });
            return SlashResult::Handled;
        }
        if let Some(value) = trimmed.strip_prefix("budget ").map(str::trim) {
            thread.update(cx, |t, cx| {
                let Some(goal) = t.goal().cloned() else {
                    cx.emit(agent::ThreadEvent::Error(anyhow::anyhow!(
                        "thread has no Goal"
                    )));
                    return;
                };
                let budget = if matches!(value, "none" | "unlimited") {
                    None
                } else {
                    match value.parse::<u64>() {
                        Ok(value) => Some(value),
                        Err(error) => {
                            cx.emit(agent::ThreadEvent::Error(error.into()));
                            return;
                        }
                    }
                };
                if let Err(error) =
                    t.edit_goal(goal.objective, budget, agent::db::GoalActor::User, cx)
                {
                    cx.emit(agent::ThreadEvent::Error(error));
                }
            });
            return SlashResult::Handled;
        }
        match trimmed.to_lowercase().as_str() {
            "" => {
                if thread.read(cx).goal().is_some() {
                    workspace.open_goal_popover(cx);
                } else {
                    workspace.begin_goal_new(window, cx);
                }
                SlashResult::Handled
            }
            "clear" => {
                thread.update(cx, |t, cx| {
                    if let Err(error) = t.clear_goal(agent::db::GoalActor::User, cx) {
                        cx.emit(agent::ThreadEvent::Error(error));
                    }
                });
                cx.notify();
                SlashResult::Handled
            }
            "pause" | "stop" => {
                thread.update(cx, |t, cx| {
                    if let Err(error) = t.set_goal_status(
                        agent::goal::GoalStatus::Paused,
                        Some("paused by user".into()),
                        agent::db::GoalActor::User,
                        cx,
                    ) {
                        cx.emit(agent::ThreadEvent::Error(error));
                    }
                });
                SlashResult::Handled
            }
            "resume" => {
                thread.update(cx, |t, cx| {
                    if let Err(error) = t.set_goal_status(
                        agent::goal::GoalStatus::Active,
                        None,
                        agent::db::GoalActor::User,
                        cx,
                    ) {
                        cx.emit(agent::ThreadEvent::Error(error));
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
                    .read(cx)
                    .goal()
                    .is_some_and(|goal| goal.status != agent::goal::GoalStatus::Complete);
                if needs_confirmation {
                    workspace.begin_goal_replace_with_objective(trimmed, window, cx);
                    return SlashResult::Handled;
                }
                let created =
                    thread.update(cx, |t, cx| match t.set_goal(trimmed.to_string(), cx) {
                        Ok(()) => true,
                        Err(error) => {
                            cx.emit(agent::ThreadEvent::Error(error));
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
        _args: &str,
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> SlashResult {
        let thread = workspace.thread.clone();
        thread.update(cx, |t, cx| t.compact(cx));
        cx.notify();
        SlashResult::Handled
    }
}

/// `/exit` — archive the current thread and start a fresh one.
struct ExitCommand;

impl SlashCommand for ExitCommand {
    fn name(&self) -> &str {
        "exit"
    }
    fn description(&self) -> SharedString {
        i18n::t("slash-exit-desc")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        assert!(r.get("goal").is_some());
        assert!(r.get("compact").is_some());
        assert!(r.get("exit").is_some());
        assert!(r.get("nope").is_none());
    }

    #[test]
    fn parse_goal_command() {
        // `/goal` bare, `/goal clear`, and `/goal <condition>` all parse.
        register_for_tests();
        let p = parse("/goal").unwrap();
        assert_eq!(p.name, "goal");
        assert_eq!(p.args, "");
        let p = parse("/goal tests pass").unwrap();
        assert_eq!(p.name, "goal");
        assert_eq!(p.args, "tests pass");
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
    fn skill_adapter_name_and_kind() {
        // A mirrored skill surfaces under its full registry key and renders with
        // the Skill kind so the `⁄` popover picks the skill icon. The description
        // is the skill's own (author language, not i18n).
        let def = Arc::new(SkillDefinition {
            name: "deliver".to_string(),
            description: "deliver a PR".to_string(),
            body: "body".to_string(),
            source: PathBuf::new(),
        });
        let cmd = SkillSlashCommand::new("gitwork:deliver".to_string(), def);
        assert_eq!(cmd.name(), "gitwork:deliver");
        assert_eq!(cmd.kind(), CompletionKind::Skill);
        assert_eq!(cmd.description().as_ref(), "deliver a PR");
    }

    #[test]
    fn registry_lookup_finds_mirrored_skill() {
        // `init` mirrors skills into the same registry `parse` consults; verify
        // a registry holding a SkillSlashCommand resolves the namespaced key and
        // reports the Skill kind (command-wins-on-collision is exercised by init
        // ordering, not by this lookup).
        let def = Arc::new(SkillDefinition {
            name: "deliver".to_string(),
            description: "deliver a PR".to_string(),
            body: "body".to_string(),
            source: PathBuf::new(),
        });
        let reg = SlashCommandRegistry::new(vec![Box::new(SkillSlashCommand::new(
            "gitwork:deliver".to_string(),
            def,
        )) as Box<dyn SlashCommand>]);
        let found = reg.get("gitwork:deliver").expect("skill key resolves");
        assert_eq!(found.name(), "gitwork:deliver");
        assert_eq!(found.kind(), CompletionKind::Skill);
    }

    #[test]
    fn registry_command_wins_over_same_key_skill() {
        // `init` skips a skill whose key a command/built-in already owns. Model
        // the post-init ordering directly: command pushed before a same-key skill
        // → `get` (first match) returns the command, so dispatch never lands on
        // the shadowed skill.
        let cmd_def = Arc::new(CommandDefinition {
            name: "yolo".to_string(),
            description: "macro yolo".to_string(),
            argument_hint: None,
            allowed_tools: Vec::new(),
            disable_model_invocation: false,
            body: "body".to_string(),
            source: PathBuf::new(),
        });
        let skill_def = Arc::new(SkillDefinition {
            name: "yolo".to_string(),
            description: "skill yolo".to_string(),
            body: "body".to_string(),
            source: PathBuf::new(),
        });
        let reg = SlashCommandRegistry::new(vec![
            Box::new(MarkdownSlashCommand::new("yolo".to_string(), cmd_def))
                as Box<dyn SlashCommand>,
            Box::new(SkillSlashCommand::new("yolo".to_string(), skill_def))
                as Box<dyn SlashCommand>,
        ]);
        let found = reg.get("yolo").expect("key resolves");
        assert_eq!(found.kind(), CompletionKind::Command);
        assert_eq!(found.description().as_ref(), "macro yolo");
    }

    /// Ensure the registry is populated for tests (idempotent).
    fn register_for_tests() {
        if REGISTRY.get().is_some() {
            return;
        }
        let _ = REGISTRY.set(SlashCommandRegistry::new(vec![
            Box::new(YoloCommand),
            Box::new(PlanCommand),
            Box::new(GoalCommand),
            Box::new(CompactCommand),
            Box::new(ExitCommand),
        ]));
    }
}
