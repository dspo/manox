//! Claude-alias → manox model resolution for the retired manox harness.
//! The pi-side counterpart lives in `agent::model_alias::resolve_pi_model_ref`.

use crate::language_model::AnyLanguageModel;
use agent::model_alias::{matches_segment, ALIASES};

/// Resolve a model reference (a manox id or a Claude/OpenAI alias) to a live
/// model. Returns `None` when no model matches, leaving the caller to inherit.
pub fn resolve_model_ref(model_ref: &str) -> Option<AnyLanguageModel> {
    let reg = crate::provider::registry::global();
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

    /// Segment-probe semantics are exercised by the agent-side tests of
    /// `resolve_pi_model_ref`; the manox variant shares the same helpers.
    #[test]
    fn probe_semantics_shared() {
        assert!(matches_segment("anthropic/o3-mini", "o3"));
        assert!(!matches_segment("proto3-server", "o3"));
    }
}
