//! Terminal readiness detection.
//!
//! A terminal is "ready" once its shell finished init (rc files, first
//! prompt) or its agent TUI drew the initial frame — from then on user input
//! is meaningful and the starting indicator can hide. Two strategies, chosen
//! by what the PTY source supports:
//!
//! - `Marker`: the spawn wrapper prints a private OSC marker before exec'ing
//!   the shell; the byte tap reports it. A fallback timeout covers wrapper
//!   failure (mangled echo, exotic rc setup) — waiting forever is worse than
//!   a slightly-early "ready".
//! - `Heuristic`: agent-backed sources cannot inject a marker (cx spawns the
//!   agent binary directly), so readiness is inferred from output timing:
//!   once the stream goes quiet for a window after the first byte, the
//!   initial frame has settled. A longer global fallback covers silent or
//!   endlessly chatty programs.
//!
//! The tracker is a pure state machine — time is passed in — so the readiness
//! pump (a timer task in `Terminal::spawn`) and tests share the same logic.

use std::time::{Duration, Instant};

/// Quiet window after the last output byte that marks the initial frame as
/// settled (heuristic mode).
pub const QUIET_WINDOW: Duration = Duration::from_millis(400);
/// Fallback after spawn when no marker arrives (marker mode).
pub const MARKER_FALLBACK: Duration = Duration::from_secs(5);
/// Fallback after spawn regardless of output (heuristic mode).
pub const HEURISTIC_FALLBACK: Duration = Duration::from_secs(10);

/// Readiness detection strategy for a PTY source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessMode {
    /// Spawn wrapper injects an OSC marker; the tap hit marks readiness.
    Marker,
    /// No marker possible; output-timing heuristic.
    Heuristic,
}

/// Tracks one terminal's readiness. `ready` flips exactly once.
pub struct ReadinessTracker {
    mode: ReadinessMode,
    ready: bool,
    spawned_at: Instant,
    last_output_at: Option<Instant>,
}

impl ReadinessTracker {
    pub fn new(mode: ReadinessMode, now: Instant) -> Self {
        Self {
            mode,
            ready: false,
            spawned_at: now,
            last_output_at: None,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Record a PTY output chunk (feeds the heuristic quiet window).
    pub fn on_output(&mut self, now: Instant) {
        self.last_output_at = Some(now);
    }

    /// Record the readiness-marker tap hit. Returns whether this call
    /// transitioned to ready.
    pub fn on_marker(&mut self) -> bool {
        if self.ready {
            return false;
        }
        self.ready = true;
        true
    }

    /// Advance heuristic / fallback conditions. Returns whether this call
    /// transitioned to ready.
    pub fn poll(&mut self, now: Instant) -> bool {
        if self.ready {
            return false;
        }
        let elapsed = now.duration_since(self.spawned_at);
        let hit = match self.mode {
            ReadinessMode::Marker => elapsed >= MARKER_FALLBACK,
            ReadinessMode::Heuristic => {
                elapsed >= HEURISTIC_FALLBACK
                    || self
                        .last_output_at
                        .is_some_and(|t| now.duration_since(t) >= QUIET_WINDOW)
            }
        };
        if hit {
            self.ready = true;
        }
        hit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_hit_transitions_once() {
        let mut r = ReadinessTracker::new(ReadinessMode::Marker, Instant::now());
        assert!(!r.is_ready());
        assert!(r.on_marker());
        assert!(r.is_ready());
        assert!(!r.on_marker());
    }

    #[test]
    fn marker_fallback_fires_without_marker() {
        let t0 = Instant::now();
        let mut r = ReadinessTracker::new(ReadinessMode::Marker, t0);
        assert!(!r.poll(t0 + MARKER_FALLBACK - Duration::from_millis(1)));
        assert!(!r.is_ready());
        assert!(r.poll(t0 + MARKER_FALLBACK));
        assert!(r.is_ready());
        assert!(!r.poll(t0 + MARKER_FALLBACK + Duration::from_secs(1)));
    }

    #[test]
    fn heuristic_waits_for_quiet_after_first_output() {
        let t0 = Instant::now();
        let mut r = ReadinessTracker::new(ReadinessMode::Heuristic, t0);
        // No output yet: not ready even well past the quiet window.
        assert!(!r.poll(t0 + Duration::from_secs(2)));
        r.on_output(t0 + Duration::from_secs(3));
        // Output too recent: quiet window has not elapsed.
        assert!(!r.poll(t0 + Duration::from_secs(3) + QUIET_WINDOW - Duration::from_millis(1)));
        assert!(r.poll(t0 + Duration::from_secs(3) + QUIET_WINDOW));
        assert!(r.is_ready());
    }

    #[test]
    fn heuristic_quiet_window_slides_with_output() {
        let t0 = Instant::now();
        let mut r = ReadinessTracker::new(ReadinessMode::Heuristic, t0);
        r.on_output(t0 + Duration::from_millis(100));
        r.on_output(t0 + Duration::from_millis(400));
        assert!(!r.poll(t0 + Duration::from_millis(600)));
        r.on_output(t0 + Duration::from_millis(700));
        assert!(!r.poll(t0 + Duration::from_millis(1000)));
        assert!(r.poll(t0 + Duration::from_millis(1100)));
    }

    #[test]
    fn heuristic_global_fallback_without_any_output() {
        let t0 = Instant::now();
        let mut r = ReadinessTracker::new(ReadinessMode::Heuristic, t0);
        assert!(!r.poll(t0 + HEURISTIC_FALLBACK - Duration::from_millis(1)));
        assert!(r.poll(t0 + HEURISTIC_FALLBACK));
    }

    #[test]
    fn marker_mode_ignores_output_timing() {
        let t0 = Instant::now();
        let mut r = ReadinessTracker::new(ReadinessMode::Marker, t0);
        r.on_output(t0 + Duration::from_secs(1));
        // Quiet-window logic must not leak into marker mode.
        assert!(!r.poll(t0 + Duration::from_secs(2)));
    }
}
