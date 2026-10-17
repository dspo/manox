// The event vocabulary of the session log (Session Core, format v1).
//
// One serde vocabulary is the whole durable schema: the log line IS the
// wire form of an `Event` wrapped in an `Envelope`. The AHP projection
// translates events outward; nothing outside that translation layer may
// build a second vocabulary. Two rules hold for every future addition:
// a user-visible fact that must survive a restart is an event, and an
// identity field never appears in an event payload — events carry id
// references only, identity lives in the immutable header.
//
// The conversation domain (deltas, tool calls, turns) joins this enum when
// the engine's writers are re-homed onto it; its row names are unchanged
// from the previous journal, so the move is mechanical. This file starts
// with the domains that have no legacy shape to inherit: session facts,
// swarm lifecycle, and system notes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// The budgets armed on a swarm run at dispatch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Budgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_ms: Option<u64>,
}

/// Every way a swarm run can end. The complete set is deliberate: a run
/// never ends silently, and no two kinds collapse into one another on the
/// wire — the "timed out" of the previous model, which rode a string
/// prefix inside a generic failure, is a first-class kind here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SettlementKind {
    Completed,
    TimedOut,
    Stalled,
    Aborted,
    Error,
    Refusal,
    MaxTokens,
}

/// One fact in the session log. Wire names use the domain-prefixed forms
/// (`run/started`, `session/disposed`); payloads are camelCase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum Event {
    #[serde(rename = "cwd_change")]
    CwdChange { to: String },
    #[serde(rename = "rename")]
    Rename { title: String },
    #[serde(rename = "model_selection")]
    ModelSelection { model: String },
    #[serde(rename = "archived")]
    Archived { on: bool },
    #[serde(rename = "pinned")]
    Pinned { on: bool },
    #[serde(rename = "session/disposed")]
    Disposed,
    #[serde(rename = "run/started")]
    RunStarted {
        run_id: String,
        kind: String,
        label: String,
        child_session: String,
        tool_call_id: String,
        budgets: Budgets,
        depth: u32,
    },
    #[serde(rename = "run/settled")]
    RunSettled {
        run_id: String,
        settlement: SettlementKind,
        /// One line of host-authored judgment, always present — including
        /// the explicit "It left no closing message." when salvage found
        /// nothing. Attributed to the host, never to the child.
        verdict: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        salvage: Option<String>,
        turns: u64,
        tool_calls: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gated_denials: Option<Vec<String>>,
    },
    #[serde(rename = "run/escalated")]
    RunEscalated {
        run_id: String,
        question: String,
        auth_id: String,
    },
    #[serde(rename = "run/continued")]
    RunContinued { run_id: String, follow_up: String },
    #[serde(rename = "note")]
    Note {
        kind: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<JsonValue>,
    },
    #[serde(rename = "error")]
    Error { text: String },
}

impl Event {
    /// The wire `type` of this event.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::CwdChange { .. } => "cwd_change",
            Event::Rename { .. } => "rename",
            Event::ModelSelection { .. } => "model_selection",
            Event::Archived { .. } => "archived",
            Event::Pinned { .. } => "pinned",
            Event::Disposed => "session/disposed",
            Event::RunStarted { .. } => "run/started",
            Event::RunSettled { .. } => "run/settled",
            Event::RunEscalated { .. } => "run/escalated",
            Event::RunContinued { .. } => "run/continued",
            Event::Note { .. } => "note",
            Event::Error { .. } => "error",
        }
    }

    /// Whether the event proves the user interacted with the session.
    /// Blank detection (`claim` reuse, sidebar hiding) keys on this; the
    /// conversation-domain events (turns, messages) that answer true join
    /// with the engine re-homing. Every event in this first vocabulary is
    /// a host-side fact — including disposal — so none of them mark
    /// interaction.
    pub fn marks_interaction(&self) -> bool {
        false
    }
}

/// One log line: a dense sequence number, the commit time, and the event
/// itself flattened in beside them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub event: Event,
}

impl Envelope {
    pub fn new(seq: u64, event: Event) -> Self {
        Self {
            seq,
            ts: Utc::now(),
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_wire_carries_the_domain_type_names() {
        let envelope = Envelope {
            seq: 3,
            ts: DateTime::<Utc>::UNIX_EPOCH,
            event: Event::CwdChange {
                to: "/work/other".into(),
            },
        };
        let wire = serde_json::to_string(&envelope).unwrap();
        assert_eq!(
            wire,
            r#"{"seq":3,"ts":"1970-01-01T00:00:00Z","type":"cwd_change","to":"/work/other"}"#
        );
        let back: Envelope = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, envelope);
    }

    #[test]
    fn run_lifecycle_round_trips_with_settlement_kinds() {
        let started = Event::RunStarted {
            run_id: "sub-1".into(),
            kind: "Explore".into(),
            label: "survey the projection layer".into(),
            child_session: "sub-1".into(),
            tool_call_id: "call-7".into(),
            budgets: Budgets {
                timeout_ms: Some(240_000),
                idle_timeout_ms: None,
            },
            depth: 1,
        };
        let settled = Event::RunSettled {
            run_id: "sub-1".into(),
            settlement: SettlementKind::TimedOut,
            verdict: "exceeded its 240s budget; promoted, then settled".into(),
            salvage: Some("partial findings …".into()),
            turns: 12,
            tool_calls: 34,
            gated_denials: Some(vec!["Bash".into(), "Bash".into()]),
        };
        for event in [started, settled.clone()] {
            let envelope = Envelope {
                seq: 1,
                ts: DateTime::<Utc>::UNIX_EPOCH,
                event,
            };
            let wire = serde_json::to_string(&envelope).unwrap();
            let back: Envelope = serde_json::from_str(&wire).unwrap();
            assert_eq!(back.event, envelope.event);
        }
        let wire = serde_json::to_value(&settled).unwrap();
        assert_eq!(wire["type"], "run/settled");
        assert_eq!(wire["settlement"], "timed-out");
        assert_eq!(wire["gatedDenials"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn every_settlement_kind_keeps_its_own_wire_name() {
        for (kind, name) in [
            (SettlementKind::Completed, "completed"),
            (SettlementKind::TimedOut, "timed-out"),
            (SettlementKind::Stalled, "stalled"),
            (SettlementKind::Aborted, "aborted"),
            (SettlementKind::Error, "error"),
            (SettlementKind::Refusal, "refusal"),
            (SettlementKind::MaxTokens, "max-tokens"),
        ] {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::String(name.into())
            );
        }
    }

    #[test]
    fn unknown_event_types_are_refused_not_guessed() {
        let bad = r#"{"seq":1,"ts":"1970-01-01T00:00:00Z","type":"quantum/flip"}"#;
        assert!(serde_json::from_str::<Envelope>(bad).is_err());
    }
}
