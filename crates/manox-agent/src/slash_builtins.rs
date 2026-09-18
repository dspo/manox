//! Built-in slash-command metadata shared by every host surface.
//!
//! The gpui host (agent-ui's `⁄` popover / registry) and the headless actor
//! behind the VS Code webview (`list_commands`, submit routing) each
//! implement the six built-in commands' execution semantics in their own
//! layer, but both enumerate the command set from this table — names,
//! aliases, and description keys — so the two surfaces can never drift
//! apart on which `/name` invocations exist.
//!
//! Execution semantics intentionally stay host-side: the gpui host toggles
//! workspace notices and popovers, the actor drives `display_text` and
//! session lifecycle. Only the *set* is shared here.

/// Metadata for one built-in slash command.
pub struct BuiltinSlashMeta {
    /// Canonical name without the leading `/` (e.g. `plan`).
    pub name: &'static str,
    /// Alternate invocation names (`/quit` for `/exit`). Typeaheads list
    /// only the canonical name; dispatch matches any alias.
    pub aliases: &'static [&'static str],
    /// One-line English description of what the command does.
    pub description: &'static str,
}

/// The built-in command set, in popover listing order.
pub const BUILTIN_SLASH_COMMANDS: &[BuiltinSlashMeta] = &[
    BuiltinSlashMeta {
        name: "mode",
        aliases: &[],
        description: "Cycle the permission mode (Read Only → Workspace Access → Full Access); `/mode <name>` sets a mode, and with a prompt switches and starts working immediately",
    },
    BuiltinSlashMeta {
        name: "plan",
        aliases: &[],
        description: "Toggle plan mode (read-only research, plan file, structured approval); `/plan <prompt>` enters plan mode and starts planning the prompt",
    },
    BuiltinSlashMeta {
        name: "compact",
        aliases: &[],
        description: "Compact the conversation: summarize older history into a handoff note so the thread can keep going past the context limit",
    },
    BuiltinSlashMeta {
        name: "exit",
        aliases: &["quit"],
        description: "Archive the current thread and start a fresh one",
    },
    BuiltinSlashMeta {
        name: "new",
        aliases: &["clear", "archive"],
        description: "Archive the current thread and start a fresh one that keeps the project, permission mode, and model",
    },
    BuiltinSlashMeta {
        name: "goal",
        aliases: &[],
        description: "Create or manage a persistent Goal (`/goal <objective>`, pause, resume, edit, clear)",
    },
];

/// Resolve an invocation name (canonical or alias) to its metadata.
pub fn canonical_builtin(name: &str) -> Option<&'static BuiltinSlashMeta> {
    BUILTIN_SLASH_COMMANDS
        .iter()
        .find(|meta| meta.name == name || meta.aliases.contains(&name))
}

/// Whether `name` (canonical or alias) is a built-in command.
pub fn is_builtin(name: &str) -> bool {
    canonical_builtin(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_canonical_names() {
        for meta in BUILTIN_SLASH_COMMANDS {
            assert!(canonical_builtin(meta.name).is_some(), "{}", meta.name);
        }
    }

    #[test]
    fn resolves_aliases() {
        assert_eq!(canonical_builtin("quit").expect("/quit").name, "exit");
        assert_eq!(canonical_builtin("clear").expect("/clear").name, "new");
        assert_eq!(canonical_builtin("archive").expect("/archive").name, "new");
    }

    #[test]
    fn rejects_unknown_and_bare_slash() {
        assert!(canonical_builtin("nope").is_none());
        assert!(canonical_builtin("").is_none());
    }

    #[test]
    fn every_description_is_a_real_sentence() {
        for meta in BUILTIN_SLASH_COMMANDS {
            assert!(
                !meta.description.trim().is_empty(),
                "empty description for {}",
                meta.name
            );
            // A description that echoes the command name would render as a
            // useless popover row; require prose beyond the bare name.
            assert!(
                meta.description.len() > meta.name.len() + 3,
                "description for {} looks like a placeholder: {}",
                meta.name,
                meta.description
            );
        }
    }
}
