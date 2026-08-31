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
    /// Fluent key in the agent locales carrying the one-line description.
    pub description_key: &'static str,
}

/// The built-in command set, in popover listing order.
pub const BUILTIN_SLASH_COMMANDS: &[BuiltinSlashMeta] = &[
    BuiltinSlashMeta {
        name: "mode",
        aliases: &[],
        description_key: "slash-mode-desc",
    },
    BuiltinSlashMeta {
        name: "plan",
        aliases: &[],
        description_key: "slash-plan-desc",
    },
    BuiltinSlashMeta {
        name: "compact",
        aliases: &[],
        description_key: "slash-compact-desc",
    },
    BuiltinSlashMeta {
        name: "exit",
        aliases: &["quit"],
        description_key: "slash-exit-desc",
    },
    BuiltinSlashMeta {
        name: "new",
        aliases: &["clear", "archive"],
        description_key: "slash-new-desc",
    },
    BuiltinSlashMeta {
        name: "goal",
        aliases: &[],
        description_key: "slash-goal-desc",
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
    fn every_description_key_is_localized() {
        // `t` falls back to the raw key when the bundle lacks it; a resolved
        // value therefore proves the fluent copy exists.
        for meta in BUILTIN_SLASH_COMMANDS {
            let en = crate::i18n::t(meta.description_key);
            assert_ne!(
                en.as_str(),
                meta.description_key,
                "missing fluent copy for {}",
                meta.description_key
            );
        }
    }
}
