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

/// `(alias, segment_probe)` pairs. The probe must be the first hyphen/dot/
/// underscore-delimited token of a live model id segment (case-insensitive),
/// so `o3` matches `o3-mini` but not `proto3-server`, and `sonnet` matches
/// `sonnet-4` but not `crimsonsonnet-x`.
pub const ALIASES: &[(&str, &str)] = &[
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
pub fn matches_segment(id: &str, probe: &str) -> bool {
    let probe = probe.to_lowercase();
    id.to_lowercase()
        .split(['/', ':'])
        .any(|seg| segment_first_token(seg) == probe)
}

/// The leading sub-token of a model segment, splitting on `-`, `.`, and `_`.
pub fn segment_first_token(seg: &str) -> &str {
    seg.split(['-', '.', '_']).next().unwrap_or("")
}

/// Resolve a model reference against the pi provider registry — the pi
/// harness's selection source. Mirrors [`resolve_model_ref`]: an exact
/// model id wins outright, then the alias table maps to a segment probe,
/// and the ref itself probes as a last resort.
pub fn resolve_pi_model_ref(model_ref: &str) -> Option<pi::types::Model> {
    let models = crate::pi_providers::global().models();
    if let Some(exact) = models.iter().find(|m| m.id == model_ref) {
        return Some(exact.clone());
    }
    // Alias lookup is case-insensitive (parity with the retired manox
    // `resolve_model_ref`): `Sonnet` / `CLAUDE-SONNET` still map.
    let probe = ALIASES
        .iter()
        .find(|(alias, _)| *alias == model_ref.to_lowercase())
        .map(|(_, probe)| *probe)
        .unwrap_or(model_ref);
    models.into_iter().find(|m| matches_segment(&m.id, probe))
}
