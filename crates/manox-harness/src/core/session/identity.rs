// Immutable session identity (Session Core, format v1).
//
// A session's identity lives exactly once — in `header.json` inside its
// directory under the state root — and is never rewritten after
// publication. Every relationship a session has (its project anchor, its
// subagent lineage) is a header field, not a mutable sidecar fact:
// identity never migrates, so the redirect/supersede machinery of the
// previous model has no equivalent here.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// On-disk format version of `header.json`. Bumped only when the header
/// schema itself changes shape; the log carries its own evolution.
pub const HEADER_FORMAT_VERSION: u32 = 1;

/// Where the session came from. Root sessions are user conversations;
/// subagent sessions are delegated children whose parent, spawning tool
/// call, and run id are pinned in the header at spawn time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineageKind {
    Root,
    Subagent,
}

/// Immutable lineage facts, present on every session. Root sessions carry
/// `depth = 0` and no parent facts; subagent sessions carry all of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lineage {
    pub kind: LineageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub depth: u32,
}

impl Lineage {
    /// The lineage of a user conversation.
    pub fn root() -> Self {
        Self {
            kind: LineageKind::Root,
            parent: None,
            tool_call_id: None,
            run_id: None,
            depth: 0,
        }
    }

    /// The lineage of a delegated child. `run_id` equals the child's own
    /// session id and chat id — the invariant the swarm projection leans
    /// on; callers mint the child id first and pass it here.
    pub fn subagent(parent: String, tool_call_id: String, run_id: String, depth: u32) -> Self {
        Self {
            kind: LineageKind::Subagent,
            parent: Some(parent),
            tool_call_id: Some(tool_call_id),
            run_id: Some(run_id),
            depth,
        }
    }
}

/// The model-side configuration a session starts with. Recorded once so a
/// later reopen (or a cold list) can answer "what did this session run on"
/// without replaying the log. Mid-session switches are `model_selection`
/// events, never header edits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPreset {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// The immutable identity of one session, written once as `header.json`
/// when the session first materializes and never touched again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub format_version: u32,
    pub id: String,
    pub created_at: DateTime<Utc>,
    /// The directory the session was created under. Anchors the project
    /// bucket and every sandbox decision for the session's lifetime; later
    /// directory switches are `cwd_change` events, never header edits.
    pub origin_cwd: String,
    pub lineage: Lineage,
    #[serde(default)]
    pub agent_preset: AgentPreset,
}

impl SessionHeader {
    /// A root-session header under `origin_cwd`.
    pub fn new_root(id: String, origin_cwd: String, created_at: DateTime<Utc>) -> Self {
        Self {
            format_version: HEADER_FORMAT_VERSION,
            id,
            created_at,
            origin_cwd,
            lineage: Lineage::root(),
            agent_preset: AgentPreset::default(),
        }
    }

    /// Reject a header whose facts contradict its lineage kind. This runs
    /// before any write so a materialization can never publish a header
    /// its own validator would disown.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("session id must be non-empty".into());
        }
        if self.origin_cwd.trim().is_empty() {
            return Err("origin cwd must be non-empty".into());
        }
        match self.lineage.kind {
            LineageKind::Root => {
                if self.lineage.depth != 0 {
                    return Err("a root session carries depth 0".into());
                }
                if self.lineage.parent.is_some()
                    || self.lineage.tool_call_id.is_some()
                    || self.lineage.run_id.is_some()
                {
                    return Err("a root session carries no parent facts".into());
                }
            }
            LineageKind::Subagent => {
                if self.lineage.depth == 0 {
                    return Err("a subagent session carries depth >= 1".into());
                }
                if self.lineage.parent.as_deref().is_none_or(str::is_empty) {
                    return Err("a subagent session names its parent".into());
                }
                if self.lineage.run_id.as_deref().is_none_or(str::is_empty) {
                    return Err("a subagent session names its run id".into());
                }
            }
        }
        Ok(())
    }
}

/// Mint a fresh session id (the claim driver's `Mint` arm and the swarm's
/// child-id mint share this).
/// A session's directory under the v1.0.0 core root
/// (`~/.manox/core/<project-slug>/<id>`). The state home follows the same
/// resolution the provider config uses (`$HOME/.manox`).
pub fn core_session_dir_for(cwd: &str, id: &str) -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map_or_else(|| std::path::PathBuf::from("."), std::path::PathBuf::from);
    home.join(".manox")
        .join("core")
        .join(project_slug(cwd))
        .join(id)
}

pub fn mint_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The project bucket name for a working directory: a flattened, lowercase
/// rendering of the full path. Two directories share a bucket only when
/// their paths differ in nothing but separators and non-alphanumeric runs.
pub fn project_slug(cwd: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in cwd.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if slug.is_empty() {
        return "default".into();
    }
    // Buckets stay readable in a directory listing; a collision between
    // two long paths costs nothing but co-tenancy in one bucket.
    slug.truncate(80);
    slug
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_header() -> SessionHeader {
        SessionHeader::new_root(
            "s-1".into(),
            "/Users/x/work/manox".into(),
            DateTime::<Utc>::UNIX_EPOCH,
        )
    }

    #[test]
    fn root_header_round_trips_camel_case() {
        let header = root_header();
        let wire = serde_json::to_value(&header).unwrap();
        assert_eq!(wire["formatVersion"], HEADER_FORMAT_VERSION);
        assert_eq!(wire["originCwd"], "/Users/x/work/manox");
        assert_eq!(wire["lineage"]["kind"], "root");
        assert!(wire["lineage"].get("parent").is_none());
        let back: SessionHeader = serde_json::from_value(wire).unwrap();
        assert_eq!(back, header);
    }

    #[test]
    fn subagent_lineage_round_trips_with_parent_facts() {
        let header = SessionHeader {
            lineage: Lineage::subagent("parent-1".into(), "call-9".into(), "sub-42".into(), 1),
            ..root_header()
        };
        let wire = serde_json::to_value(&header).unwrap();
        assert_eq!(wire["lineage"]["kind"], "subagent");
        assert_eq!(wire["lineage"]["toolCallId"], "call-9");
        assert_eq!(wire["lineage"]["runId"], "sub-42");
        assert_eq!(wire["lineage"]["depth"], 1);
        let back: SessionHeader = serde_json::from_value(wire).unwrap();
        assert_eq!(back, header);
    }

    #[test]
    fn validation_rejects_contradictory_lineage() {
        let mut header = root_header();
        assert!(header.validate().is_ok());

        header.lineage.parent = Some("p".into());
        assert!(header.validate().is_err());

        let mut child = root_header();
        child.lineage = Lineage::subagent("".into(), "call".into(), "run".into(), 1);
        assert!(child.validate().is_err());

        let mut child = root_header();
        child.lineage = Lineage::subagent("p".into(), "call".into(), "run".into(), 0);
        assert!(child.validate().is_err());
    }

    #[test]
    fn project_slug_flattens_paths_stably() {
        assert_eq!(project_slug("/Users/x/work/manox"), "users-x-work-manox");
        assert_eq!(project_slug("/Users/x/../work/manox"), "users-x-work-manox");
        assert_eq!(project_slug("///"), "default");
        assert_eq!(project_slug("A B/C_D"), "a-b-c-d");
    }
}
