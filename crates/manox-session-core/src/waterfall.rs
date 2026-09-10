//! ServerCall waterfall state machine (architecture §D.4): the multi-client
//! adjudication semantics for `Approve` / `AskUserQuestion` / `PlanVerdict`.
//!
//! One request fans out to every `owner(session) ∩ capability` client;
//! settlement is fan-in: **all** recipients must answer *next* for the
//! kernel to proceed, **any** *rejected* settles immediately and every
//! remaining delivery is cancelled (a cancel frame is owed to those
//! clients). A delivery that times out counts as rejected — fail-closed,
//! never silently waiting forever. Replies after settlement, duplicate
//! replies and replies from non-recipients are ignored.
//!
//! Single-recipient waterfalls degenerate to the current v1 semantics
//! (one owner answering), so the migration keeps behavior identical there.

use std::collections::BTreeMap;

/// How one adjudication settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaterfallOutcome {
    /// Every recipient answered next.
    Allowed,
    /// A recipient rejected (by client id) or a delivery expired.
    Rejected { by: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryState {
    Waiting,
    Next,
    Rejected,
    Cancelled,
}

/// One in-flight waterfall.
pub struct Waterfall {
    pub session_id: String,
    deliveries: BTreeMap<String, DeliveryState>,
    settled: Option<WaterfallOutcome>,
}

impl Waterfall {
    /// Fan out to `recipients` (client ids). Empty means nobody can answer:
    /// the caller treats that as fail-closed before constructing one.
    pub fn new(session_id: impl Into<String>, recipients: Vec<String>) -> Self {
        Waterfall {
            session_id: session_id.into(),
            deliveries: recipients
                .into_iter()
                .map(|id| (id, DeliveryState::Waiting))
                .collect(),
            settled: None,
        }
    }

    /// Record one recipient's reply. Returns the outcome iff this reply
    /// settles the waterfall.
    pub fn reply(&mut self, client_id: &str, next: bool) -> Option<WaterfallOutcome> {
        if self.settled.is_some() {
            return None; // late reply after settlement
        }
        let Some(state) = self.deliveries.get_mut(client_id) else {
            return None; // not a recipient of this waterfall
        };
        match state {
            DeliveryState::Waiting => {
                *state = if next {
                    DeliveryState::Next
                } else {
                    DeliveryState::Rejected
                };
            }
            // Duplicate reply: first answer stands.
            _ => return None,
        }
        if !next {
            return Some(self.settle(WaterfallOutcome::Rejected {
                by: Some(client_id.to_string()),
            }));
        }
        if self
            .deliveries
            .values()
            .all(|s| matches!(s, DeliveryState::Next))
        {
            return Some(self.settle(WaterfallOutcome::Allowed));
        }
        None
    }

    /// A delivery timed out (RpcPeer timeout): fail-closed rejection by the
    /// expired client.
    pub fn expire(&mut self, client_id: &str) -> Option<WaterfallOutcome> {
        if self.settled.is_some() {
            return None;
        }
        if let Some(state) = self.deliveries.get_mut(client_id)
            && *state == DeliveryState::Waiting
        {
            *state = DeliveryState::Rejected;
            return Some(self.settle(WaterfallOutcome::Rejected {
                by: Some(client_id.to_string()),
            }));
        }
        None
    }

    /// The recipients still owed a cancel frame after settlement (those
    /// still waiting when the waterfall settled — `settle` re-marks them
    /// `Cancelled`, which is exactly this set).
    pub fn cancelled_recipients(&self) -> Vec<String> {
        self.deliveries
            .iter()
            .filter(|(_, state)| matches!(state, DeliveryState::Cancelled))
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn settled(&self) -> Option<WaterfallOutcome> {
        self.settled.clone()
    }

    pub fn recipients(&self) -> Vec<String> {
        self.deliveries.keys().cloned().collect()
    }

    fn settle(&mut self, outcome: WaterfallOutcome) -> WaterfallOutcome {
        if self.settled.is_none() {
            if outcome == WaterfallOutcome::Allowed {
                for state in self.deliveries.values_mut() {
                    *state = DeliveryState::Next;
                }
            } else {
                for state in self.deliveries.values_mut() {
                    if *state == DeliveryState::Waiting {
                        *state = DeliveryState::Cancelled;
                    }
                }
            }
            self.settled = Some(outcome.clone());
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_recipient_degenerates_to_v1_semantics() {
        let mut w = Waterfall::new("s1", vec!["desktop".into()]);
        assert_eq!(w.reply("desktop", true), Some(WaterfallOutcome::Allowed));
        assert_eq!(w.settled(), Some(WaterfallOutcome::Allowed));
        // No cancels owed.
        assert!(w.cancelled_recipients().is_empty());
    }

    #[test]
    fn all_next_settles_allowed() {
        let mut w = Waterfall::new("s1", vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(w.reply("a", true), None);
        assert_eq!(w.reply("b", true), None);
        assert_eq!(w.reply("c", true), Some(WaterfallOutcome::Allowed));
        assert_eq!(w.settled(), Some(WaterfallOutcome::Allowed));
    }

    #[test]
    fn any_rejection_settles_and_cancels_the_rest() {
        let mut w = Waterfall::new("s1", vec!["a".into(), "b".into(), "c".into()]);
        w.reply("a", true);
        assert_eq!(
            w.reply("b", false),
            Some(WaterfallOutcome::Rejected {
                by: Some("b".into())
            })
        );
        // c is owed a cancel frame.
        assert_eq!(w.cancelled_recipients(), vec!["c".to_string()]);
        // Late replies are ignored; settled state stands.
        assert_eq!(w.reply("c", true), None);
        assert_eq!(
            w.settled(),
            Some(WaterfallOutcome::Rejected {
                by: Some("b".into())
            })
        );
    }

    #[test]
    fn duplicate_and_foreign_replies_are_ignored() {
        let mut w = Waterfall::new("s1", vec!["a".into(), "b".into()]);
        assert_eq!(w.reply("a", true), None);
        assert_eq!(w.reply("a", false), None, "first answer stands");
        assert_eq!(w.reply("zzz", false), None, "not a recipient");
        assert!(w.settled().is_none());
        assert_eq!(w.reply("b", true), Some(WaterfallOutcome::Allowed));
    }

    #[test]
    fn expiry_fails_closed() {
        let mut w = Waterfall::new("s1", vec!["a".into(), "b".into()]);
        w.reply("a", true);
        assert_eq!(
            w.expire("b"),
            Some(WaterfallOutcome::Rejected {
                by: Some("b".into())
            })
        );
        // Expiring an already-answered client is a no-op.
        let mut w2 = Waterfall::new("s1", vec!["a".into()]);
        w2.reply("a", true);
        assert_eq!(w2.expire("a"), None);
    }
}
