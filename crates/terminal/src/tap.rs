//! Incremental OSC byte tap.
//!
//! The readiness marker (OSC 6973) and shell cwd reports (OSC 7) ride the PTY
//! byte stream as OSC sequences, but vte dispatches neither (unknown codes
//! are dropped; OSC 7 falls to `unhandled`), so this tap observes raw bytes
//! in parallel with `Processor::advance` and extracts the two payloads. It is
//! purely observational — the stream reaches the Term unchanged.
//!
//! OSC payloads may split across reads; the tap accumulates until a
//! terminator (BEL or ST). Accumulation is capped so a malformed sequence
//! cannot grow memory without bound; overflow drops the sequence and returns
//! to the ground state.

use std::path::PathBuf;

/// Events extracted from the PTY byte stream, parallel to vte parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapEvent {
    /// The spawn-time readiness marker matched the nonce this terminal was
    /// spawned with.
    ReadyMarker,
    /// OSC 7 cwd report whose URI carried a `file://` scheme and decoded to
    /// an absolute path; anything else is dropped.
    Cwd(PathBuf),
}

/// Longest OSC payload the tap buffers before dropping the sequence. The
/// marker and cwd reports are far below this; the cap only bounds malformed
/// input.
const MAX_OSC_LEN: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// ESC seen; `]` starts an OSC.
    Esc,
    /// Accumulating an OSC payload (bytes after `ESC ]`).
    Osc,
    /// ESC inside an OSC payload; `\` terminates as ST.
    OscEsc,
}

pub struct OscTap {
    state: State,
    buf: Vec<u8>,
    /// Nonce of this terminal's readiness marker; `None` when the source was
    /// spawned without a wrapper (heuristic readiness).
    nonce: Option<String>,
}

impl OscTap {
    pub fn new(nonce: Option<String>) -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
            nonce,
        }
    }

    /// Observe a chunk of PTY output, returning every event these bytes
    /// complete. The same chunk must still be fed to `Processor::advance` —
    /// the tap never consumes from the Term's perspective.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<TapEvent> {
        let mut events = Vec::new();
        for &b in chunk {
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => {
                    self.state = match b {
                        b']' => {
                            self.buf.clear();
                            State::Osc
                        }
                        0x1b => State::Esc,
                        _ => State::Ground,
                    };
                }
                State::Osc => match b {
                    0x07 => {
                        events.extend(self.finish());
                        self.state = State::Ground;
                    }
                    0x1b => self.state = State::OscEsc,
                    _ => {
                        if self.buf.len() >= MAX_OSC_LEN {
                            tracing::debug!("OSC tap: sequence over {MAX_OSC_LEN} bytes, dropped");
                            self.buf.clear();
                            self.state = State::Ground;
                        } else {
                            self.buf.push(b);
                        }
                    }
                },
                State::OscEsc => {
                    if b == b'\\' {
                        events.extend(self.finish());
                    }
                    // With or without the ST `\`, the sequence is over; a
                    // fresh ESC starts the next one.
                    self.state = if b == 0x1b { State::Esc } else { State::Ground };
                }
            }
        }
        events
    }

    /// Parse the accumulated payload; the buffer is always cleared.
    fn finish(&mut self) -> Option<TapEvent> {
        let payload = std::mem::take(&mut self.buf);
        let text = std::str::from_utf8(&payload).ok()?;
        if let Some(rest) = text.strip_prefix("6973;manox-ready=") {
            if self.nonce.as_deref() == Some(rest) {
                return Some(TapEvent::ReadyMarker);
            }
            return None;
        }
        if let Some(uri) = text.strip_prefix("7;") {
            return parse_cwd_uri(uri).map(TapEvent::Cwd);
        }
        None
    }
}

/// Validate an OSC 7 URI: `file://<host><path>`, percent-decoded, absolute.
/// The host segment is accepted verbatim — the terminal only ever drives
/// local shells, so any host still refers to this machine.
fn parse_cwd_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path_start = rest.find('/')?;
    let decoded = percent_decode(&rest[path_start..])?;
    if !decoded.starts_with('/') {
        return None;
    }
    Some(PathBuf::from(decoded))
}

/// Percent-decode `%XX` triplets; other bytes pass through. Malformed
/// triplets or invalid UTF-8 reject the whole string.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = *bytes.get(i + 1)?;
            let lo = *bytes.get(i + 2)?;
            out.push(hex_val(hi)? * 16 + hex_val(lo)?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "test-nonce-1";

    fn tap() -> OscTap {
        OscTap::new(Some(NONCE.to_string()))
    }

    fn marker() -> String {
        format!("\x1b]6973;manox-ready={NONCE}\x07")
    }

    #[test]
    fn marker_single_chunk() {
        let mut t = tap();
        assert_eq!(t.feed(marker().as_bytes()), vec![TapEvent::ReadyMarker]);
    }

    #[test]
    fn marker_split_across_chunks() {
        let bytes = marker().into_bytes();
        for cut in 1..bytes.len() {
            let mut t = tap();
            assert_eq!(t.feed(&bytes[..cut]), vec![]);
            assert_eq!(t.feed(&bytes[cut..]), vec![TapEvent::ReadyMarker]);
        }
    }

    #[test]
    fn marker_with_st_terminator() {
        let mut t = tap();
        let seq = format!("\x1b]6973;manox-ready={NONCE}\x1b\\");
        assert_eq!(t.feed(seq.as_bytes()), vec![TapEvent::ReadyMarker]);
    }

    #[test]
    fn wrong_nonce_is_ignored() {
        let mut t = tap();
        let seq = "\x1b]6973;manox-ready=someone-else\x07";
        assert_eq!(t.feed(seq.as_bytes()), vec![]);
    }

    #[test]
    fn no_nonce_never_ready() {
        let mut t = OscTap::new(None);
        let seq = "\x1b]6973;manox-ready=anything\x07";
        assert_eq!(t.feed(seq.as_bytes()), vec![]);
    }

    #[test]
    fn marker_embedded_in_output() {
        let mut t = tap();
        let chunk = format!("plain text\r\n{}more text", marker());
        assert_eq!(t.feed(chunk.as_bytes()), vec![TapEvent::ReadyMarker]);
    }

    #[test]
    fn osc7_file_uri() {
        let mut t = tap();
        assert_eq!(
            t.feed(b"\x1b]7;file://localhost/tmp/work\x07"),
            vec![TapEvent::Cwd(PathBuf::from("/tmp/work"))]
        );
    }

    #[test]
    fn osc7_percent_decoded() {
        let mut t = tap();
        assert_eq!(
            t.feed(b"\x1b]7;file://host/tmp/my%20dir\x07"),
            vec![TapEvent::Cwd(PathBuf::from("/tmp/my dir"))]
        );
    }

    #[test]
    fn osc7_rejects_non_file_scheme() {
        let mut t = tap();
        assert_eq!(t.feed(b"\x1b]7;http://example.com/tmp\x07"), vec![]);
    }

    #[test]
    fn osc7_rejects_relative_path() {
        let mut t = tap();
        // No `/` after the host segment — nothing absolute to decode.
        assert_eq!(t.feed(b"\x1b]7;file://localhost\x07"), vec![]);
    }

    #[test]
    fn osc7_rejects_malformed_percent() {
        let mut t = tap();
        assert_eq!(t.feed(b"\x1b]7;file://h/tmp/%zz\x07"), vec![]);
    }

    #[test]
    fn oversized_sequence_is_dropped_and_recovers() {
        let mut t = tap();
        let mut huge = b"\x1b]7;file://".to_vec();
        huge.extend(std::iter::repeat_n(b'a', MAX_OSC_LEN + 100));
        huge.push(0x07);
        assert_eq!(t.feed(&huge), vec![]);
        // The tap resynchronizes on the next ESC.
        assert_eq!(t.feed(marker().as_bytes()), vec![TapEvent::ReadyMarker]);
    }

    #[test]
    fn unterminated_sequence_interrupted_by_esc() {
        let mut t = tap();
        // An OSC abandoned mid-payload (ESC not followed by `\`) is dropped.
        let chunk = format!("\x1b]7;file://host/tmp\x1b[31m{}", marker());
        assert_eq!(t.feed(chunk.as_bytes()), vec![TapEvent::ReadyMarker]);
    }

    #[test]
    fn other_osc_codes_pass_through_unnoticed() {
        let mut t = tap();
        assert_eq!(t.feed(b"\x1b]0;window title\x07"), vec![]);
        assert_eq!(t.feed(b"\x1b]52;c;aGVsbG8=\x07"), vec![]);
        assert_eq!(t.feed(b"\x1b]8;;https://example.com\x1b\\"), vec![]);
    }
}
