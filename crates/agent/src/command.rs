//! Slash commands — user-triggered prompt macros.
//!
//! A slash command is a markdown file (`<name>.md`) whose frontmatter carries
//! `description` / `argument-hint` / `allowed-tools` / `disable-model-invocation`
//! and whose body is a prompt template with a `$ARGUMENTS` placeholder. The
//! user types `/plugin:command <args>`; the engine substitutes `$ARGUMENTS`
//! with the raw args and injects the rendered body as a user message. This
//! mirrors Claude Code's command mechanism — the body is a natural-language
//! instruction the model then carries out, not an imperative script.
//!
//! `allowed-tools` gates the turn: the model may only call tools whose name
//! appears in the list during that turn. Claude Code tool specs use PascalCase
//! names with optional argument constraints (e.g. `Bash(node:*)`); this module
//! parses each spec into a manox tool id (`bash`) so the registry can build a
//! matching subset without knowing Claude Code's naming.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::paths;
use crate::plugin::PluginManager;

#[derive(Debug, Clone, Deserialize)]
struct CommandMeta {
    #[serde(default)]
    description: String,
    #[serde(default, rename = "argument-hint")]
    argument_hint: Option<String>,
    /// Raw frontmatter value, parsed from a comma-separated string or a YAML
    /// list. Each entry is a Claude Code tool spec like `Bash(node:*)`.
    #[serde(default, rename = "allowed-tools")]
    allowed_tools: AllowedTools,
    #[serde(default, rename = "disable-model-invocation")]
    disable_model_invocation: bool,
}

/// `allowed-tools` may be a YAML list or a comma-separated string (Claude Code
/// uses both forms across its corpus). Deserialize into a `Vec<String>`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
enum AllowedTools {
    #[default]
    Empty,
    List(Vec<String>),
    Str(String),
}

impl AllowedTools {
    fn into_vec(self) -> Vec<String> {
        match self {
            AllowedTools::Empty => Vec::new(),
            AllowedTools::List(v) => v,
            AllowedTools::Str(s) => s.split(',').map(|t| t.trim().to_string()).collect(),
        }
    }
}

/// A loaded slash command.
#[derive(Debug, Clone)]
pub struct CommandDefinition {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    /// Manox tool ids this command permits during its turn. Empty = inherit all.
    pub allowed_tools: Vec<String>,
    pub disable_model_invocation: bool,
    pub body: String,
    pub source: PathBuf,
}

impl CommandDefinition {
    /// Render the command body, substituting `arguments` into the
    /// `{{ arguments }}` placeholder. Legacy `$ARGUMENTS` placeholders are
    /// rewritten first so old command files keep working. Command bodies are
    /// untrusted prose, so a Tera-incompatible literal `{{` falls back to plain
    /// substitution rather than breaking the render.
    pub fn render(&self, args: &str) -> String {
        crate::prompt::render_command_body(&self.body, args)
    }
}

/// Process-wide registry of slash commands, keyed by `plugin:name` or bare
/// `name`. Loaded once at startup; malformed files are skipped.
#[derive(Debug, Default)]
pub struct CommandRegistry {
    commands: BTreeMap<String, Arc<CommandDefinition>>,
}

impl CommandRegistry {
    pub fn load() -> Self {
        let mut commands = BTreeMap::new();
        // Built-in commands compiled into the binary. Inserted first so user
        // and plugin commands with the same `name` override them, mirroring
        // `agent_def.rs`'s `builtin_definitions` pattern.
        for cmd in builtin_commands() {
            commands.insert(cmd.name.clone(), Arc::new(cmd));
        }
        if let Ok(dir) = paths::commands_dir() {
            scan_commands_dir(&dir, None, &mut commands);
        }
        for plugin in PluginManager::installed() {
            let dir = plugin.root.join("commands");
            if !dir.exists() {
                continue;
            }
            scan_commands_dir(&dir, Some(&plugin.name), &mut commands);
        }
        Self { commands }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<CommandDefinition>> {
        self.commands.get(name)
    }

    pub fn list(&self) -> Vec<&Arc<CommandDefinition>> {
        self.commands.values().collect()
    }

    /// `(registry_key, definition)` pairs, for populating a UI command popover
    /// where the surfaced name must be the full key (e.g. `gitwork:deliver`),
    /// not the bare filename stem stored in `CommandDefinition.name`.
    pub fn entries(&self) -> Vec<(&String, &Arc<CommandDefinition>)> {
        self.commands.iter().collect()
    }
}

fn scan_commands_dir(
    dir: &Path,
    namespace: Option<&str>,
    out: &mut BTreeMap<String, Arc<CommandDefinition>>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!("failed to read commands dir {}: {e}", dir.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        match load_command_file(&path, stem) {
            Ok(cmd) => {
                let key = match namespace {
                    Some(ns) => format!("{ns}:{}", cmd.name),
                    None => cmd.name.clone(),
                };
                out.insert(key, Arc::new(cmd));
            }
            Err(e) => tracing::warn!("skipping command {}: {e:#}", path.display()),
        }
    }
}

fn load_command_file(path: &Path, fallback_name: String) -> Result<CommandDefinition> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed = crate::frontmatter::parse::<CommandMeta>(&raw)
        .map_err(|e| anyhow::anyhow!("parsing command {}: {e:#}", path.display()))?;
    let front = parsed.front;
    // The command name is the filename stem; frontmatter carries no `name` key.
    let name = fallback_name;
    let allowed_tools = front
        .allowed_tools
        .into_vec()
        .into_iter()
        .filter_map(|spec| tool_id_from_spec(&spec))
        .collect();
    Ok(CommandDefinition {
        name,
        description: front.description,
        argument_hint: front.argument_hint,
        allowed_tools,
        disable_model_invocation: front.disable_model_invocation,
        body: parsed.body,
        source: path.to_path_buf(),
    })
}

/// Map a Claude Code tool spec (`Bash(node:*)`, `Read`, `AskUserQuestion`) to a
/// manox tool id (`bash`, `read_file`, `ask_user`). Returns `None` for specs
/// that have no manox equivalent — the caller drops them, narrowing the turn's
/// tool set to what manox actually provides.
fn tool_id_from_spec(spec: &str) -> Option<String> {
    let head = spec.split('(').next().unwrap_or(spec).trim();
    if head.is_empty() {
        return None;
    }
    Some(match head.to_lowercase().as_str() {
        "read" | "read_file" => crate::tools::READ.to_string(),
        "write" | "write_file" => crate::tools::WRITE.to_string(),
        "edit" | "edit_file" | "multiedit" => crate::tools::EDIT.to_string(),
        "list" | "list_directory" | "ls" => crate::tools::LIST.to_string(),
        "bash" => crate::tools::BASH.to_string(),
        "bashoutput" | "bash_output" => crate::tools::BASH_OUTPUT.to_string(),
        "grep" => crate::tools::GREP.to_string(),
        "glob" => crate::tools::GLOB.to_string(),
        "askuserquestion" | "ask_user" => crate::tools::ASK_USER_QUESTION.to_string(),
        "agent" | "task" => crate::tools::AGENT.to_string(),
        "skill" => crate::tools::SKILL.to_string(),
        "self_info" | "selfinfo" => crate::tools::SELF_INFO.to_string(),
        "monitor" => crate::tools::MONITOR.to_string(),
        other => other.to_string(),
    })
}

static REGISTRY: OnceLock<CommandRegistry> = OnceLock::new();

pub fn init() {
    let registry = CommandRegistry::load();
    if let Err(existing) = REGISTRY.set(registry) {
        tracing::warn!(
            "command registry already initialized ({} commands)",
            existing.list().len()
        );
    }
}

pub fn global() -> &'static CommandRegistry {
    REGISTRY.get().expect("command registry not initialized")
}

/// Non-panicking accessor for callers that may run before `agent::init`
/// (e.g. the UI slash-command registry init, which `main` calls after
/// `agent::init` but is still safer not to assume).
pub fn try_global() -> Option<&'static CommandRegistry> {
    REGISTRY.get()
}

/// The built-in slash commands compiled into the binary. Each entry is
/// `include_str!`-embedded markdown (frontmatter + body) living next to the
/// source, mirroring `agents/explore.md`. A malformed builtin is a compile-time
/// authoring error, so failures are surfaced as panics at load time rather than
/// silently skipped — same policy as `agent_def::builtin_definitions`.
fn builtin_commands() -> Vec<CommandDefinition> {
    const HEALTHZ: &str = include_str!("commands/healthz.md");
    vec![parse_builtin_command(HEALTHZ, "healthz").expect("builtin healthz command must parse")]
}

/// Parse a built-in command from an embedded markdown string. Shared
/// frontmatter + `tool_id_from_spec` logic with [`load_command_file`], but
/// takes the raw text and name from the caller — no disk path. The `source`
/// is `PathBuf::new()` (no on-disk origin), symmetring `agent_def.rs`'s
/// `root: None` for built-in agent definitions.
fn parse_builtin_command(raw: &str, name: &str) -> Result<CommandDefinition> {
    let parsed = crate::frontmatter::parse::<CommandMeta>(raw)
        .map_err(|e| anyhow::anyhow!("parsing builtin command {name}: {e:#}"))?;
    let front = parsed.front;
    let allowed_tools = front
        .allowed_tools
        .into_vec()
        .into_iter()
        .filter_map(|spec| tool_id_from_spec(&spec))
        .collect();
    Ok(CommandDefinition {
        name: name.to_string(),
        description: front.description,
        argument_hint: front.argument_hint,
        allowed_tools,
        disable_model_invocation: front.disable_model_invocation,
        body: parsed.body,
        source: PathBuf::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_frontmatter() {
        let raw = "---\n\
description: Run a review\n\
argument-hint: '[--base <ref>]'\n\
allowed-tools: Bash(node:*), AskUserQuestion, Read\n\
---\nReview this: $ARGUMENTS\n";
        let parsed = crate::frontmatter::parse::<CommandMeta>(raw).unwrap();
        assert_eq!(parsed.front.description, "Run a review");
        assert_eq!(
            parsed.front.argument_hint.as_deref(),
            Some("[--base <ref>]")
        );
        let v = parsed.front.allowed_tools.into_vec();
        assert_eq!(v, vec!["Bash(node:*)", "AskUserQuestion", "Read"]);
        assert!(parsed.body.contains("$ARGUMENTS"));
    }

    #[test]
    fn tool_id_from_spec_maps_clude_names() {
        assert_eq!(tool_id_from_spec("Bash(node:*)"), Some("Bash".to_string()));
        assert_eq!(tool_id_from_spec("Read"), Some("Read".to_string()));
        assert_eq!(
            tool_id_from_spec("AskUserQuestion"),
            Some("AskUserQuestion".to_string())
        );
        assert_eq!(tool_id_from_spec("Agent"), Some("Agent".to_string()));
        assert_eq!(tool_id_from_spec(""), None);
    }

    #[test]
    fn render_substitutes_arguments() {
        let cmd = CommandDefinition {
            name: "review".to_string(),
            description: String::new(),
            argument_hint: None,
            allowed_tools: Vec::new(),
            disable_model_invocation: false,
            body: "Review $ARGUMENTS now".to_string(),
            source: PathBuf::new(),
        };
        assert_eq!(cmd.render("HEAD~1"), "Review HEAD~1 now");
    }

    #[test]
    fn allowed_tools_list_form() {
        let raw = "---\nallowed-tools:\n  - Read\n  - Glob\n---\nbody\n";
        let parsed = crate::frontmatter::parse::<CommandMeta>(raw).unwrap();
        let v = parsed.front.allowed_tools.into_vec();
        assert_eq!(v, vec!["Read", "Glob"]);
    }
}

#[cfg(test)]
mod builtin_tests {
    use super::*;

    #[test]
    fn builtin_commands_parse_and_contain_healthz() {
        let cmds = builtin_commands();
        let healthz = cmds
            .iter()
            .find(|c| c.name == "healthz")
            .expect("builtin commands must contain healthz");
        assert!(
            !healthz.description.is_empty(),
            "healthz description must be non-empty"
        );
        assert!(!healthz.body.is_empty(), "healthz body must be non-empty");
        assert_eq!(
            healthz.source,
            PathBuf::new(),
            "builtin command has no on-disk source"
        );
    }

    #[test]
    fn builtin_healthz_has_no_allowed_tools() {
        let cmds = builtin_commands();
        let healthz = cmds
            .iter()
            .find(|c| c.name == "healthz")
            .expect("builtin commands must contain healthz");
        assert!(
            healthz.allowed_tools.is_empty(),
            "healthz must inherit all tools (empty allowed_tools), got: {:?}",
            healthz.allowed_tools
        );
    }

    #[test]
    fn load_includes_builtin_command() {
        let registry = CommandRegistry::load();
        assert!(
            registry.get("healthz").is_some(),
            "CommandRegistry::load must include the builtin healthz command"
        );
    }
}
