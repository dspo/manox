//! The durable child descriptor — the versioned, model-invisible identity
//! record a subagent run is resumed/audited from (the dsh
//! `subagent/descriptor` vocabulary, one-shot form). Persisted inside the
//! child session header's `metadata.subagent` object; keys are
//! whitelisted so an unknown field is a construction error, not silent
//! data loss.

use serde::Serialize;

use crate::ext::subagent::types::ToolFilter;

/// Descriptor schema version. Bumped on any payload shape change.
pub const DESCRIPTOR_VERSION: u8 = 1;

/// The one-shot child's identity record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Descriptor {
    pub version: u8,
    /// The delegating provider's registry name.
    pub provider: String,
    /// The dispatch label (the tool call's `description`).
    pub label: String,
    /// The delegation kind — this record exists only for one-shot runs;
    /// continuable runs (future) carry their own fields.
    pub mode: &'static str,
    /// The definition name the child was configured from (`Explore`,
    /// `Sailor`, a user manifest name).
    pub kind: String,
    /// The child's absolute delegation depth (`parent + 1`).
    pub depth: u32,
    /// Per-request persona prefix, when one was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// Per-request tool scoping, when one was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_filter: Option<ToolFilter>,
}

impl Descriptor {
    /// Build a one-shot descriptor. `mode` is fixed to `"one-shot"`.
    pub fn one_shot(
        provider: impl Into<String>,
        label: impl Into<String>,
        kind: impl Into<String>,
        depth: u32,
    ) -> Self {
        Descriptor {
            version: DESCRIPTOR_VERSION,
            provider: provider.into(),
            label: label.into(),
            mode: "one-shot",
            kind: kind.into(),
            depth,
            persona: None,
            tool_filter: None,
        }
    }

    /// The child session header's `metadata.subagent` object. The `type` key
    /// keeps the pre-descriptor lineage shape (definition name + parent) so
    /// the sidebar's session filter and usage accounting keep working
    /// unchanged.
    pub fn metadata(&self, parent_session: Option<&str>) -> serde_json::Value {
        let mut meta = serde_json::json!({
            "type": self.kind,
            "label": self.label,
            "provider": self.provider,
            "mode": self.mode,
            "depth": self.depth,
            "descriptor_version": self.version,
        });
        if let Some(parent) = parent_session {
            meta["parent"] = serde_json::json!(parent);
        }
        if let Some(persona) = &self.persona {
            meta["persona"] = serde_json::json!(persona);
        }
        if let Some(filter) = &self.tool_filter {
            meta["tool_filter"] = serde_json::json!({
                "allow": filter.allow,
                "deny": filter.deny,
            });
        }
        meta
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The descriptor is strictly whitelisted: only the declared fields
    /// serialize, and the session metadata keeps the `subagent.type` +
    /// `parent` lineage shape the sidebar filter and restore consume.
    #[test]
    fn one_shot_metadata_keeps_lineage_shape() {
        let mut descriptor = Descriptor::one_shot("spawn", "survey auth", "Explore", 1);
        descriptor.persona = Some("read-only".into());
        let meta = descriptor.metadata(Some("thread-9"));
        assert_eq!(meta["type"], "Explore");
        assert_eq!(meta["parent"], "thread-9");
        assert_eq!(meta["provider"], "spawn");
        assert_eq!(meta["mode"], "one-shot");
        assert_eq!(meta["label"], "survey auth");
        assert_eq!(meta["depth"], 1);
        assert_eq!(meta["descriptor_version"], 1);
        assert_eq!(meta["persona"], "read-only");
        assert!(meta.get("tool_filter").is_none(), "absent fields omitted");
    }

    #[test]
    fn without_parent_the_lineage_key_is_absent() {
        let descriptor = Descriptor::one_shot("spawn", "l", "Sailor", 1);
        let meta = descriptor.metadata(None);
        assert!(meta.get("parent").is_none());
    }
}
