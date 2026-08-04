// SSE (Server-Sent Events) incremental parser.
//
// Bytes arrive in arbitrary chunks from the HTTP body. `feed` appends bytes
// and returns every complete event payload that became available. An event is
// terminated by a blank line; a `data:` line contributes its payload. Comment
// lines (`:` prefix, used for heartbeats) are ignored. This parser is
// transport-level and shared by all SSE-based providers.

/// An incremental SSE decoder.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Bytes received but not yet terminated by a newline.
    buf: Vec<u8>,
    /// Payload lines accumulated for the in-progress event.
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a chunk of bytes and drain any complete event payloads.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut events = Vec::new();
        self.buf.extend_from_slice(chunk);

        // Consume as many full lines as the buffer currently holds.
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop(); // drop '\n'
            if line.last() == Some(&b'\r') {
                line.pop(); // drop '\r'
            }
            let line = String::from_utf8_lossy(&line).into_owned();

            if line.is_empty() {
                // Blank line terminates the current event.
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                    self.data.clear();
                }
                continue;
            }
            if line.starts_with(':') {
                // Comment / heartbeat line.
                continue;
            }
            if let Some(payload) = line.strip_prefix("data:") {
                // Per spec, a single leading space after "data:" is stripped.
                let payload = payload.strip_prefix(' ').unwrap_or(payload);
                self.data.push(payload.to_string());
            }
            // `event:`, `id:`, `retry:` lines are not needed by these APIs.
        }

        events
    }

    /// Flush any trailing event not terminated by a blank line (stream end).
    pub fn finish(&mut self) -> Option<String> {
        if self.data.is_empty() {
            None
        } else {
            let payload = self.data.join("\n");
            self.data.clear();
            Some(payload)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(payload: &str) -> String {
        payload.to_string()
    }

    #[test]
    fn single_event_one_chunk() {
        let mut p = SseParser::new();
        let out = p.feed(b"data: {\"a\":1}\n\n");
        assert_eq!(out, vec![ev("{\"a\":1}")]);
    }

    #[test]
    fn event_split_across_chunks() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: {\"a").is_empty());
        assert!(p.feed(b"\":1}\n").is_empty());
        let out = p.feed(b"\n");
        assert_eq!(out, vec![ev("{\"a\":1}")]);
    }

    #[test]
    fn multiple_events_one_chunk() {
        let mut p = SseParser::new();
        let out = p.feed(b"data: one\n\ndata: two\n\n");
        assert_eq!(out, vec![ev("one"), ev("two")]);
    }

    #[test]
    fn crlf_line_endings() {
        let mut p = SseParser::new();
        let out = p.feed(b"data: hello\r\n\r\n");
        assert_eq!(out, vec![ev("hello")]);
    }

    #[test]
    fn heartbeat_comments_ignored() {
        let mut p = SseParser::new();
        let out = p.feed(b": ping\n\ndata: real\n\n");
        assert_eq!(out, vec![ev("real")]);
    }

    #[test]
    fn multiline_data_joined() {
        let mut p = SseParser::new();
        let out = p.feed(b"data: line1\ndata: line2\n\n");
        assert_eq!(out, vec![ev("line1\nline2")]);
    }

    #[test]
    fn finish_flushes_unterminated_event() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: tail\n").is_empty());
        assert_eq!(p.finish(), Some(ev("tail")));
        assert_eq!(p.finish(), None);
    }

    #[test]
    fn data_prefix_space_stripped() {
        let mut p = SseParser::new();
        let out = p.feed(b"data:value\n\n");
        assert_eq!(out, vec![ev("value")]);
    }
}
