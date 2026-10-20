// The settlement face of a run: every ending maps to exactly one kind and
// one line of host-authored judgment. The verdict is attributed to the
// host, never to the child, and it always exists — including the explicit
// no-closing-message line when salvage found nothing. This module is the
// single wording table; delivery paths compose it, the journal carries the
// kind verbatim, and nothing downstream string-matches — with one exception
// the table itself must keep true: `manox-agent`'s transcript rebuild reads
// a restored row's status off the delivery's first line (the verdict), which
// is the only copy that survives a peer delivery, so it matches the line's
// TAIL (never a substring: the line embeds the run's label). Reword a verdict
// and that rebuild falls back to Error, its safe side; keep every kind's tail
// distinct and it stays exact.

use crate::core::session::event::SettlementKind;
use crate::subagent::StopReason;

/// The durable settlement kind for a run's stop reason. A promoted run has
/// no settlement here — it settles later, through its continued run's own
/// reason.
pub fn kind_of(reason: &StopReason) -> SettlementKind {
    match reason {
        StopReason::Completed => SettlementKind::Completed,
        StopReason::TimedOut => SettlementKind::TimedOut,
        StopReason::Stalled => SettlementKind::Stalled,
        StopReason::Aborted => SettlementKind::Aborted,
        StopReason::Error => SettlementKind::Error,
        StopReason::Refusal => SettlementKind::Refusal,
        StopReason::MaxTokens => SettlementKind::MaxTokens,
        StopReason::Promoted | StopReason::Unknown(_) => SettlementKind::Error,
    }
}

/// One line of host-authored judgment for the settlement.
pub fn verdict(label: &str, kind: SettlementKind) -> String {
    match kind {
        SettlementKind::Completed => format!("`{label}` completed."),
        SettlementKind::TimedOut => format!("`{label}` exceeded its time budget and was stopped."),
        SettlementKind::Stalled => format!("`{label}` stalled (no activity) and was stopped."),
        SettlementKind::Aborted => format!("`{label}` was stopped before it finished."),
        SettlementKind::Error => format!("`{label}` failed before it finished."),
        SettlementKind::Refusal => format!("`{label}` declined the task."),
        SettlementKind::MaxTokens => format!("`{label}` ran out of room (token ceiling)."),
    }
}

/// The explicit line a settlement delivers when salvage found no text —
/// silence is never an acceptable substitute.
pub const NO_CLOSING_MESSAGE: &str = "It left no closing message.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stop_reason_keeps_its_own_kind_and_a_verdict() {
        let table = [
            (StopReason::Completed, SettlementKind::Completed),
            (StopReason::TimedOut, SettlementKind::TimedOut),
            (StopReason::Stalled, SettlementKind::Stalled),
            (StopReason::Aborted, SettlementKind::Aborted),
            (StopReason::Error, SettlementKind::Error),
            (StopReason::Refusal, SettlementKind::Refusal),
            (StopReason::MaxTokens, SettlementKind::MaxTokens),
        ];
        for (reason, kind) in table {
            assert_eq!(kind_of(&reason), kind);
            assert!(!verdict("scout", kind).is_empty());
        }
        assert_eq!(
            kind_of(&StopReason::Unknown("x".into())),
            SettlementKind::Error
        );
    }
}
