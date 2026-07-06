//! Bridge Claude Code model aliases to manox `ResolvedModel` ids.
//!
//! Plugin agent/command frontmatter commonly pins `model: sonnet` (or
//! `opus` / `haiku`), assuming a Claude Code runtime backed by Anthropic. manox
//! connects to arbitrary providers declared in `cx.providers.config.yaml`, so a
//! literal `"sonnet"` id rarely resolves. This layer bridges that assumption:
//!
//! 1. An exact manox id match (`provider/model/wire`) wins outright.
//! 2. Otherwise a Claude/OpenAI alias table maps the ref to a substring probe
//!    against the live model list — `sonnet` → any model whose id contains
//!    `sonnet`.
//! 3. As a last resort, the ref itself is used as a substring probe.
//!
//! Falls back to `None` when nothing matches, in which case the caller inherits
//! the parent thread's model — the same behavior as an unset `model` field.

use crate::language_model::AnyLanguageModel;
use crate::provider::registry::global;

/// `(alias, id_substring_probe)` pairs. Probes are matched case-insensitively
/// against each live model's full id. Longer aliases are listed first so a
/// `claude-sonnet` ref does not collapse to the bare `sonnet` probe before the
/// more specific entry gets a chance.
const ALIASES: &[(&str, &str)] = &[
    ("claude-sonnet", "sonnet"),
    ("claude-opus", "opus"),
    ("claude-haiku", "haiku"),
    ("sonnet", "sonnet"),
    ("opus", "opus"),
    ("haiku", "haiku"),
    ("gpt-4o", "gpt-4o"),
    ("gpt-5", "gpt-5"),
    ("o3", "o3"),
];

/// Resolve a model reference (a manox id or a Claude/OpenAI alias) to a live
/// model. Returns `None` when no model matches, leaving the caller to inherit.
pub fn resolve_model_ref(model_ref: &str) -> Option<AnyLanguageModel> {
    let reg = global();
    if let Some(m) = reg.get_model(model_ref) {
        return Some(m);
    }
    let lower = model_ref.to_lowercase();
    if let Some((_, probe)) = ALIASES.iter().find(|(k, _)| *k == lower)
        && let Some(m) = reg
            .models()
            .iter()
            .find(|m| m.id().to_lowercase().contains(probe))
    {
        return Some(m.clone());
    }
    reg.models()
        .iter()
        .find(|m| m.id().to_lowercase().contains(&lower))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_table_has_claude_aliases() {
        let keys: Vec<&str> = ALIASES.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&"sonnet"));
        assert!(keys.contains(&"opus"));
        assert!(keys.contains(&"haiku"));
    }

    #[test]
    fn longer_alias_listed_first() {
        // `claude-sonnet` must precede `sonnet` so the specific match wins.
        let sonnet_idx = ALIASES.iter().position(|(k, _)| *k == "sonnet").unwrap();
        let claude_sonnet_idx = ALIASES
            .iter()
            .position(|(k, _)| *k == "claude-sonnet")
            .unwrap();
        assert!(claude_sonnet_idx < sonnet_idx);
    }
}
