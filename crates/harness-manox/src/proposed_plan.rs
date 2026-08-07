//! Streaming `<proposed_plan>…</proposed_plan>` tag parser.
//!
//! Splits assistant text deltas into visible text and a plan-delta stream.
//! Line-oriented, mirroring codex's `proposed_plan` block semantics: the
//! opening and closing tags must each occupy a line of their own — leading and
//! trailing whitespace on that line is tolerated, but any other text on the
//! line disqualifies it (the line is then ordinary visible text). A delta may
//! split a tag across boundaries (`<prop` + `osed_plan>`); the line buffer
//! holds the partial line until it can be proved not to be a tag. A block left
//! open at end-of-turn is closed by [`ProposedPlanParser::finish`].
//!
//! Re-implemented for manox (no external stream-parser dependency); the
//! visible text has plan blocks removed so they never leak into the assistant
//! message the model re-reads next turn.

const OPEN_TAG: &str = "<proposed_plan>";
const CLOSE_TAG: &str = "</proposed_plan>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposedPlanSegment {
    /// Visible assistant text — persist into the assistant message and render.
    Normal(String),
    /// The `<proposed_plan>` tag line was matched.
    Start,
    /// Text inside the block — accumulate as the plan, do not persist as
    /// assistant message text.
    Delta(String),
    /// The `</proposed_plan>` tag line was matched.
    End,
}

/// Stateful parser fed one text delta at a time.
#[derive(Debug, Default)]
pub struct ProposedPlanParser {
    /// Whether the parser is currently inside an open `<proposed_plan>` block.
    inside: bool,
    /// Whether the current line is still a candidate tag (buffered until
    /// disproven). Mirrors codex `detect_tag`.
    detect_tag: bool,
    /// The accumulating current line while it may still be a tag.
    line_buffer: String,
}

impl ProposedPlanParser {
    pub fn new() -> Self {
        Self {
            inside: false,
            detect_tag: true,
            line_buffer: String::new(),
        }
    }

    /// Feed one text delta; returns the ordered segments it produced. The
    /// caller routes `Normal` to the assistant message and `Delta` to the
    /// live plan accumulator.
    pub fn feed(&mut self, delta: &str) -> Vec<ProposedPlanSegment> {
        let mut segments = Vec::new();
        let mut run = String::new();

        for ch in delta.chars() {
            if self.detect_tag {
                if !run.is_empty() {
                    self.push_text(std::mem::take(&mut run), &mut segments);
                }
                self.line_buffer.push(ch);
                if ch == '\n' {
                    self.finish_line(&mut segments);
                    continue;
                }
                let slug = self.line_buffer.trim_start();
                if slug.is_empty() || self.is_tag_prefix(slug) {
                    continue;
                }
                // The buffered line can no longer become a tag — emit it as
                // text and resume normal streaming.
                let buffered = std::mem::take(&mut self.line_buffer);
                self.detect_tag = false;
                self.push_text(buffered, &mut segments);
                continue;
            }

            run.push(ch);
            if ch == '\n' {
                self.push_text(std::mem::take(&mut run), &mut segments);
                self.detect_tag = true;
            }
        }

        if !run.is_empty() {
            self.push_text(run, &mut segments);
        }

        segments
    }

    /// Flush the buffered tail and close any block left open at end-of-turn.
    pub fn finish(&mut self) -> Vec<ProposedPlanSegment> {
        let mut segments = Vec::new();
        if !self.line_buffer.is_empty() {
            let buffered = std::mem::take(&mut self.line_buffer);
            let without_newline = buffered.strip_suffix('\n').unwrap_or(&buffered);
            let slug = without_newline.trim_start().trim_end();
            if !self.inside && self.match_open(slug) {
                push_segment(&mut segments, ProposedPlanSegment::Start);
                self.inside = true;
            } else if self.inside && self.match_close(slug) {
                push_segment(&mut segments, ProposedPlanSegment::End);
                self.inside = false;
            } else {
                self.push_text(buffered, &mut segments);
            }
        }
        if self.inside {
            // Unterminated block — close it so the plan is still recoverable.
            push_segment(&mut segments, ProposedPlanSegment::End);
            self.inside = false;
        }
        self.detect_tag = true;
        segments
    }

    fn finish_line(&mut self, segments: &mut Vec<ProposedPlanSegment>) {
        let line = std::mem::take(&mut self.line_buffer);
        let without_newline = line.strip_suffix('\n').unwrap_or(&line);
        let slug = without_newline.trim_start().trim_end();

        if !self.inside && self.match_open(slug) {
            push_segment(segments, ProposedPlanSegment::Start);
            self.inside = true;
            self.detect_tag = true;
            return;
        }
        if self.inside && self.match_close(slug) {
            push_segment(segments, ProposedPlanSegment::End);
            self.inside = false;
            self.detect_tag = true;
            return;
        }
        self.detect_tag = true;
        self.push_text(line, segments);
    }

    fn push_text(&self, text: String, segments: &mut Vec<ProposedPlanSegment>) {
        if self.inside {
            push_segment(segments, ProposedPlanSegment::Delta(text));
        } else {
            push_segment(segments, ProposedPlanSegment::Normal(text));
        }
    }

    /// Whether `slug` (already `trim_start`'d) could still grow into a tag —
    /// i.e. some tag starts with `slug` (after trimming its own trailing
    /// whitespace). Holds the line buffer until the line is decided.
    fn is_tag_prefix(&self, slug: &str) -> bool {
        let slug = slug.trim_end();
        OPEN_TAG.starts_with(slug) || CLOSE_TAG.starts_with(slug)
    }

    fn match_open(&self, slug: &str) -> bool {
        slug == OPEN_TAG
    }

    fn match_close(&self, slug: &str) -> bool {
        slug == CLOSE_TAG
    }
}

fn push_segment(segments: &mut Vec<ProposedPlanSegment>, segment: ProposedPlanSegment) {
    match segment {
        ProposedPlanSegment::Normal(delta) => {
            if delta.is_empty() {
                return;
            }
            if let Some(ProposedPlanSegment::Normal(existing)) = segments.last_mut() {
                existing.push_str(&delta);
                return;
            }
            segments.push(ProposedPlanSegment::Normal(delta));
        }
        ProposedPlanSegment::Delta(delta) => {
            if delta.is_empty() {
                return;
            }
            if let Some(ProposedPlanSegment::Delta(existing)) = segments.last_mut() {
                existing.push_str(&delta);
                return;
            }
            segments.push(ProposedPlanSegment::Delta(delta));
        }
        ProposedPlanSegment::Start => segments.push(ProposedPlanSegment::Start),
        ProposedPlanSegment::End => segments.push(ProposedPlanSegment::End),
    }
}

/// Concatenate the visible (non-plan) text, removing plan blocks. Used when
/// rebuilding a turn's visible text from a persisted message.
pub fn strip_proposed_plan_blocks(text: &str) -> String {
    let mut parser = ProposedPlanParser::new();
    let mut out = String::new();
    for segment in parser.feed(text) {
        if let ProposedPlanSegment::Normal(text) = segment {
            out.push_str(&text);
        }
    }
    for segment in parser.finish() {
        if let ProposedPlanSegment::Normal(text) = segment {
            out.push_str(&text);
        }
    }
    out
}

/// Extract the concatenated plan text from the (first) `<proposed_plan>` block,
/// if any. Used to re-derive a plan from persisted history.
pub fn extract_proposed_plan_text(text: &str) -> Option<String> {
    let mut parser = ProposedPlanParser::new();
    let mut plan_text = String::new();
    let mut saw_plan_block = false;
    let process = |segments: Vec<ProposedPlanSegment>, plan: &mut String, saw: &mut bool| {
        for segment in segments {
            match segment {
                ProposedPlanSegment::Start => {
                    *saw = true;
                    plan.clear();
                }
                ProposedPlanSegment::Delta(delta) => plan.push_str(&delta),
                ProposedPlanSegment::Normal(_) | ProposedPlanSegment::End => {}
            }
        }
    };
    process(parser.feed(text), &mut plan_text, &mut saw_plan_block);
    process(parser.finish(), &mut plan_text, &mut saw_plan_block);
    saw_plan_block.then_some(plan_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(delta: &str) -> (String, Vec<ProposedPlanSegment>) {
        let mut parser = ProposedPlanParser::new();
        let mut visible = String::new();
        let mut segments = Vec::new();
        for seg in parser.feed(delta) {
            if let ProposedPlanSegment::Normal(t) = &seg {
                visible.push_str(t);
            }
            segments.push(seg);
        }
        for seg in parser.finish() {
            if let ProposedPlanSegment::Normal(t) = &seg {
                visible.push_str(t);
            }
            segments.push(seg);
        }
        (visible, segments)
    }

    #[test]
    fn streams_segments_and_visible_text_across_delta_split() {
        let (visible, segments) =
            collect("Intro text\n<proposed_plan>\n- step 1\n</proposed_plan>\nOutro");
        assert_eq!(visible, "Intro text\nOutro");
        assert_eq!(
            segments,
            vec![
                ProposedPlanSegment::Normal("Intro text\n".to_string()),
                ProposedPlanSegment::Start,
                ProposedPlanSegment::Delta("- step 1\n".to_string()),
                ProposedPlanSegment::End,
                ProposedPlanSegment::Normal("Outro".to_string()),
            ]
        );
    }

    #[test]
    fn split_tag_across_two_deltas() {
        let mut parser = ProposedPlanParser::new();
        let mut visible = String::new();
        let mut segments = Vec::new();
        for chunk in [
            "Intro text\n<prop",
            "osed_plan>\n- step 1\n",
            "</proposed_plan>\nOutro",
        ] {
            for seg in parser.feed(chunk) {
                if let ProposedPlanSegment::Normal(t) = &seg {
                    visible.push_str(t);
                }
                segments.push(seg);
            }
        }
        for seg in parser.finish() {
            if let ProposedPlanSegment::Normal(t) = &seg {
                visible.push_str(t);
            }
            segments.push(seg);
        }
        assert_eq!(visible, "Intro text\nOutro");
        assert_eq!(
            segments,
            vec![
                ProposedPlanSegment::Normal("Intro text\n".to_string()),
                ProposedPlanSegment::Start,
                ProposedPlanSegment::Delta("- step 1\n".to_string()),
                ProposedPlanSegment::End,
                ProposedPlanSegment::Normal("Outro".to_string()),
            ]
        );
    }

    #[test]
    fn rejects_tag_line_with_extra_text() {
        let (visible, segments) = collect("  <proposed_plan> extra\n");
        assert_eq!(visible, "  <proposed_plan> extra\n");
        assert_eq!(
            segments,
            vec![ProposedPlanSegment::Normal(
                "  <proposed_plan> extra\n".to_string()
            )]
        );
    }

    #[test]
    fn closes_unterminated_block_on_finish() {
        let (visible, segments) = collect("<proposed_plan>\n- step 1\n");
        assert_eq!(visible, "");
        assert_eq!(
            segments,
            vec![
                ProposedPlanSegment::Start,
                ProposedPlanSegment::Delta("- step 1\n".to_string()),
                ProposedPlanSegment::End,
            ]
        );
    }

    #[test]
    fn no_block_is_all_normal() {
        let (visible, segments) = collect("just text\nmore text");
        assert_eq!(visible, "just text\nmore text");
        assert_eq!(
            segments,
            vec![ProposedPlanSegment::Normal(
                "just text\nmore text".to_string()
            )]
        );
    }

    #[test]
    fn strip_helper_removes_block() {
        let text = "before\n<proposed_plan>\n- step\n</proposed_plan>\nafter";
        assert_eq!(strip_proposed_plan_blocks(text), "before\nafter");
    }

    #[test]
    fn extract_helper_returns_block_text() {
        let text = "before\n<proposed_plan>\n- step\n</proposed_plan>\nafter";
        assert_eq!(
            extract_proposed_plan_text(text),
            Some("- step\n".to_string())
        );
    }

    #[test]
    fn extract_returns_none_without_block() {
        assert_eq!(extract_proposed_plan_text("no plan here"), None);
    }

    #[test]
    fn extract_takes_last_block_when_multiple_present() {
        // The parser does not enforce single-block discipline — that is the
        // prompt's contract ("at most one <proposed_plan> block per turn").
        // When multiple well-formed blocks appear, `extract` clears on each
        // Start and returns the last block's text.
        let text = "<proposed_plan>\na\n</proposed_plan>\n<proposed_plan>\nb\n</proposed_plan>";
        assert_eq!(extract_proposed_plan_text(text), Some("b\n".to_string()));
    }
}
