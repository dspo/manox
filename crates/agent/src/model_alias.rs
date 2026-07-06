//! Bridge Claude Code model aliases to manox `ResolvedModel` ids.
//!
//! Plugin agent/command frontmatter commonly pins `model: sonnet` (or
//! `opus` / `haiku`), assuming a Claude Code runtime backed by Anthropic. manox
//! connects to arbitrary providers declared in `cx.providers.config.yaml`, so a
//! literal `"sonnet"` id rarely resolves. This layer bridges that assumption:
//!
//! 1. An exact manox id match (`provider/model/wire`) wins outright.
//! 2. Otherwise a Claude/OpenAI alias table maps the ref to a segment probe
//!    against the live model list — `sonnet` → any model whose id has a path
//!    segment whose first hyphen/dot/underscore token is `sonnet`.
//! 3. As a last resort, the ref itself is used as a segment probe.
//!
//! First-token matching (not raw substring) avoids false positives: `o3` does
//! not match `proto3-server` (its first token is `proto3`), and `sonnet` does
//! not match `crimsonsonnet-x` (its first token is `crimsonsonnet`). Yet `o3`
//! still matches `o3-mini` and `sonnet` still matches `sonnet-4-5`.
//! Falls back to `None` when nothing matches, in which case the caller inherits
//! the parent thread's model — the same behavior as an unset `model` field.

use crate::language_model::AnyLanguageModel;
use crate::provider::registry::global;

/// `(alias, segment_probe)` pairs. The probe must be the first hyphen/dot/
/// underscore-delimited token of a live model id segment (case-insensitive),
/// so `o3` matches `o3-mini` but not `proto3-server`, and `sonnet` matches
/// `sonnet-4` but not `crimsonsonnet-x`.
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

/// True when `id` has a `/`- or `:`-delimited segment whose first `-`/`.`/`_`
/// token equals `probe` (case-insensitive). First-token equality — not raw
/// substring containment — so `o3` matches `anthropic/o3-mini` (token `o3`)
/// but not `proto3-server` (token `proto3`), and `sonnet` matches `sonnet-4`
/// but not `crimsonsonnet-x`.
fn matches_segment(id: &str, probe: &str) -> bool {
    let probe = probe.to_lowercase();
    id.to_lowercase()
        .split(['/', ':'])
        .any(|seg| segment_first_token(seg) == probe)
}

/// The leading sub-token of a model segment, splitting on `-`, `.`, and `_`.
fn segment_first_token(seg: &str) -> &str {
    seg.split(['-', '.', '_']).next().unwrap_or("")
}

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
            .find(|m| matches_segment(&m.id(), probe))
    {
        return Some(m.clone());
    }
    reg.models()
        .iter()
        .find(|m| matches_segment(&m.id(), &lower))
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

    #[test]
    fn segment_match_rejects_substring_false_positives() {
        // `o3` as a segment must not match `proto3-server` (no `o3` segment).
        assert!(!matches_segment("provider/proto3-server", "o3"));
        // It must match a real `o3` segment.
        assert!(matches_segment("provider/o3-mini", "o3"));
        // `sonnet` must not match `crimsonsonnet-x`.
        assert!(!matches_segment("provider/crimsonsonnet-x", "sonnet"));
        assert!(matches_segment("provider/sonnet-4", "sonnet"));
    }
}
