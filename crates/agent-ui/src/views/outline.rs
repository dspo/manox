//! User-turn outline: the render-agnostic model behind the Codex-style left
//! navigation rail.
//!
//! The rail draws one tick per user turn. This module owns everything that is
//! independent of gpui: pulling the user turns out of the conversation,
//! summarizing each turn for the hover card, and mapping a turn to the span of
//! list items it owns so the rail can light up whichever turns intersect the
//! viewport. Rendering and scroll wiring stay in `workspace.rs`, next to the
//! private `ListState`.

use std::ops::Range;

use crate::conversation::ConvItem;

/// Longest summary shown in the hover card; longer text is cut with an
/// ellipsis. The card wraps, so this bounds card height as well as width.
const SUMMARY_MAX_CHARS: usize = 120;

/// One user turn as the outline sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTurn {
    /// Index into the flat conversation item list — the scroll target.
    pub item_ix: usize,
    /// 0-based position among user turns; the tick's order on the rail.
    pub ordinal: usize,
    /// One-line hover-card summary of the user message.
    pub summary: String,
}

/// Collect the user turns from the conversation's flat item kinds, in order.
/// Pure over `ConvItem` so it unit-tests without a gpui context; the workspace
/// feeds it `items().iter().map(|e| e.read(cx).kind())`.
pub fn user_turns_from<'a>(items: impl Iterator<Item = &'a ConvItem>) -> Vec<UserTurn> {
    items
        .enumerate()
        .filter_map(|(item_ix, kind)| match kind {
            ConvItem::User(text) => Some((item_ix, text)),
            _ => None,
        })
        .enumerate()
        .map(|(ordinal, (item_ix, text))| UserTurn {
            item_ix,
            ordinal,
            summary: turn_summary(text),
        })
        .collect()
}

/// One-line summary of a user message for the hover card: the first non-blank
/// line, trimmed and length-capped. A message with no text (attachment-only)
/// yields an empty string, and the renderer then draws the tick without a card.
/// User text never crosses the model/UI language boundary, so no i18n.
pub fn turn_summary(raw: &str) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    truncate(line, SUMMARY_MAX_CHARS)
}

/// Character-count truncation with a trailing ellipsis.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}…")
    } else {
        s.to_string()
    }
}

/// The half-open span of conversation items owned by the turn at `ordinal`:
/// from its own item up to (excluding) the next turn's item, and to `total`
/// for the last turn. A turn "owns" its user message plus every assistant /
/// tool / reasoning item produced in reply, so the rail can treat any of them
/// being on screen as "this turn is visible".
pub fn turn_span(turns: &[UserTurn], ordinal: usize, total: usize) -> Range<usize> {
    debug_assert!(ordinal < turns.len(), "turn_span ordinal out of range");
    let start = turns.get(ordinal).map_or(total, |t| t.item_ix);
    let end = turns.get(ordinal + 1).map_or(total, |next| next.item_ix);
    start..end
}

/// Whether a turn's span intersects the list's visible item range. Empty
/// ranges (nothing measured yet) never intersect.
pub fn turn_is_visible(span: &Range<usize>, visible: &Range<usize>) -> bool {
    span.start < visible.end && visible.start < span.end
}

/// Ticks within this many positions of the hovered one are displaced by the
/// wave; beyond it they sit at rest.
pub const WAVE_RADIUS: usize = 3;

/// Wave falloff for the tick at `ordinal` given the hovered tick (if any): a
/// `0.0..=1.0` weight that is 1 on the hovered tick, tapers linearly to 0 at
/// `WAVE_RADIUS`, and is 0 everywhere when nothing is hovered. The renderer
/// maps this onto extra tick length and spacing so the rail bulges around the
/// cursor like the Codex rail.
pub fn wave_weight(ordinal: usize, hovered: Option<usize>) -> f32 {
    let Some(h) = hovered else { return 0.0 };
    let dist = ordinal.abs_diff(h);
    if dist > WAVE_RADIUS {
        0.0
    } else {
        1.0 - (dist as f32 / (WAVE_RADIUS as f32 + 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::ToolCallItem;
    use agent::ToolCallStatus;

    fn assistant(text: &str) -> ConvItem {
        ConvItem::Assistant {
            text: text.to_string(),
            streaming: false,
            token_usage: None,
        }
    }

    fn tool(id: &str) -> ConvItem {
        ConvItem::ToolCall(ToolCallItem {
            id: id.to_string(),
            name: "bash".to_string(),
            title: String::new(),
            status: ToolCallStatus::Success,
            output: String::new(),
            is_error: false,
            streaming: false,
            collapsed: true,
            user_toggled: false,
        })
    }

    #[test]
    fn user_turns_pick_user_items_with_ordinals_and_indices() {
        let items = [
            ConvItem::User("first".to_string()),
            assistant("reply"),
            tool("t1"),
            ConvItem::User("second".to_string()),
            assistant("reply2"),
        ];
        let turns = user_turns_from(items.iter());
        assert_eq!(turns.len(), 2);
        assert_eq!((turns[0].item_ix, turns[0].ordinal), (0, 0));
        assert_eq!((turns[1].item_ix, turns[1].ordinal), (3, 1));
        assert_eq!(turns[0].summary, "first");
        assert_eq!(turns[1].summary, "second");
    }

    #[test]
    fn turn_summary_takes_first_nonblank_line_trimmed() {
        let tests = [
            ("hello world", "hello world"),
            ("  spaced  ", "spaced"),
            ("\n\n  first real line\nsecond", "first real line"),
            ("", ""),
            ("   \n  \t ", ""),
        ];
        for (input, want) in tests {
            assert_eq!(turn_summary(input), want, "input={input:?}");
        }
    }

    #[test]
    fn turn_summary_truncates_long_text() {
        let long = "x".repeat(SUMMARY_MAX_CHARS + 10);
        let got = turn_summary(&long);
        assert_eq!(got.chars().count(), SUMMARY_MAX_CHARS + 1); // +1 for the ellipsis
        assert!(got.ends_with('…'));
    }

    #[test]
    fn turn_span_covers_up_to_next_turn_then_to_total() {
        let turns = vec![
            UserTurn {
                item_ix: 0,
                ordinal: 0,
                summary: String::new(),
            },
            UserTurn {
                item_ix: 3,
                ordinal: 1,
                summary: String::new(),
            },
        ];
        assert_eq!(turn_span(&turns, 0, 5), 0..3);
        assert_eq!(turn_span(&turns, 1, 5), 3..5);
    }

    #[test]
    #[should_panic(expected = "turn_span ordinal out of range")]
    fn turn_span_debug_asserts_on_out_of_range_ordinal() {
        // Callers only ever pass ordinals from `user_turns_from`, but the
        // debug-assert guards the public API against a future off-by-one.
        let turns = [UserTurn {
            item_ix: 0,
            ordinal: 0,
            summary: String::new(),
        }];
        let _ = turn_span(&turns, 5, 1);
    }

    #[test]
    fn turn_visibility_is_range_intersection() {
        let tests = [
            (0..3, 1..2, true),  // viewport inside span
            (0..3, 2..6, true),  // partial overlap
            (3..5, 0..3, false), // adjacent, no overlap
            (3..5, 5..8, false), // adjacent on the other side
            (0..3, 3..3, false), // empty viewport never intersects
        ];
        for (span, visible, want) in tests {
            assert_eq!(
                turn_is_visible(&span, &visible),
                want,
                "span={span:?} visible={visible:?}"
            );
        }
    }

    #[test]
    fn wave_weight_peaks_at_hover_and_tapers_to_zero() {
        // Nothing hovered → flat rail.
        assert_eq!(wave_weight(5, None), 0.0);
        // Hovered tick is the crest.
        assert_eq!(wave_weight(5, Some(5)), 1.0);
        // Symmetric falloff around the crest.
        assert_eq!(wave_weight(4, Some(5)), wave_weight(6, Some(5)));
        // Strictly decreasing with distance, within the radius.
        assert!(wave_weight(5, Some(5)) > wave_weight(6, Some(5)));
        assert!(wave_weight(6, Some(5)) > wave_weight(7, Some(5)));
        // Beyond the radius the rail is at rest.
        assert_eq!(wave_weight(5 + WAVE_RADIUS + 1, Some(5)), 0.0);
        assert_eq!(wave_weight(0, Some(5)), 0.0);
    }
}
