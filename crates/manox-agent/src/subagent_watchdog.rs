//! Per-subagent health watchdog — the state machine that folds a child
//! session's event stream into "is it actually making progress" verdicts.
//!
//! The dispatch run task feeds every child `AgentEvent` into
//! [`SubagentWatchdog`] and asks it on a fixed tick whether the run has
//! stalled. A stall has two possible responses: a report (the state flips
//! to `Stalled` and the health surfaces show it) and, when the dispatch
//! armed an idle budget, an enforcement signal that settles the run the
//! way a wall-clock timeout does. Loop detection is mark-only: a repeated
//! identical call surfaces as `Looping` but never kills anything on its
//! own — heuristics have false positives, explicit budgets do the killing.

use std::time::{Duration, Instant};

use pi::types::AgentEvent;

/// The fixed cadence of [`SubagentWatchdog::tick`].
pub const WATCHDOG_TICK: Duration = Duration::from_secs(5);

/// Stall-report threshold when the dispatch did not arm an idle budget:
/// no child event AND nothing in flight for this long marks the run
/// stalled (report-only — nothing kills it).
pub const DEFAULT_STALL_REPORT_MS: u64 = 120_000;

/// While a stall episode lasts, re-report every this many ticks so the
/// surfaced "stalled Ns" line keeps counting instead of freezing at the
/// first report (~30s refresh at the fixed tick cadence).
const STALL_ECHO_TICKS: u64 = 6;

/// The same (tool, args) call this many times back-to-back reads as a loop.
const LOOP_STREAK: usize = 3;

/// The watchdog's verdict about a live subagent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// Dispatched, no child event observed yet.
    Starting,
    /// Recent events; the run is making progress.
    Working,
    /// A tool is in flight — legitimately long-running work (a 20-minute
    /// `cargo test`) is not a stall while a tool executes.
    ToolRunning { name: String, since: Instant },
    /// No events and nothing in flight since `since`.
    Stalled { since: Instant },
    /// The same tool call repeated `LOOP_STREAK`+ times back-to-back.
    Looping { call: String },
}

/// The outcome of a [`SubagentWatchdog::tick`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    /// Nothing actionable.
    Idle,
    /// The run crossed the report threshold — surface the stall; repeats
    /// every [`STALL_ECHO_TICKS`] while the episode lasts so the surfaced
    /// elapsed time stays fresh, and resets on fresh activity.
    ReportStall,
    /// The armed idle budget expired — the caller settles the run.
    EnforceStall,
}

/// One subagent's progress state machine. Cheap to lock-and-fold: the run
/// task holds the only writer role behind an `Arc<Mutex<…>>` shared with
/// the status surfaces.
pub struct SubagentWatchdog {
    health: Health,
    started_at: Instant,
    last_event_at: Instant,
    in_flight: usize,
    turns: u64,
    tool_calls: u64,
    last_activity: Option<String>,
    loop_key: Option<String>,
    loop_streak: usize,
    stall_reported: bool,
    stall_echo_ticks: u64,
    idle_budget: Option<Duration>,
    report_threshold: Duration,
}

impl SubagentWatchdog {
    pub fn new(now: Instant, idle_timeout_ms: Option<u64>) -> Self {
        SubagentWatchdog {
            health: Health::Starting,
            started_at: now,
            last_event_at: now,
            in_flight: 0,
            turns: 0,
            tool_calls: 0,
            last_activity: None,
            loop_key: None,
            loop_streak: 0,
            stall_reported: false,
            stall_echo_ticks: 0,
            idle_budget: idle_timeout_ms.map(Duration::from_millis),
            report_threshold: Duration::from_millis(DEFAULT_STALL_REPORT_MS),
        }
    }

    /// Fold one child event into the state machine. Returns whether the
    /// health state changed — the caller throttles health emissions off it.
    pub fn observe(&mut self, event: &AgentEvent, now: Instant) -> bool {
        let before = self.health.clone();
        self.last_event_at = now;
        // Any activity closes the stall episode; the next stall re-reports
        // from a fresh echo count.
        self.stall_reported = false;
        self.stall_echo_ticks = 0;
        match event {
            AgentEvent::TurnEnd { .. } => {
                self.turns += 1;
                self.enter(Health::Working);
            }
            AgentEvent::ToolExecutionStart {
                tool_name,
                arguments,
                ..
            } => {
                self.tool_calls += 1;
                self.in_flight = self.in_flight.saturating_add(1);
                self.last_activity = Some(match crate::engine::adapt::arg_hint(arguments) {
                    Some((key, value)) => format!("{tool_name} {key}={value}"),
                    None => tool_name.clone(),
                });
                self.track_loop(tool_name, arguments, now);
            }
            AgentEvent::ToolExecutionEnd { .. } => {
                self.in_flight = self.in_flight.saturating_sub(1);
                // A looping run stays marked across the loop's own tool-end.
                if !matches!(self.health, Health::Looping { .. }) {
                    self.enter(Health::Working);
                }
            }
            _ => {
                // Text/thinking/turn-start/retry: progress, and any loop
                // streak breaks the moment the subagent does something else.
                self.loop_key = None;
                self.loop_streak = 0;
                self.enter(Health::Working);
            }
        }
        self.health != before
    }

    /// Mark a loop when the identical call repeats; the mark persists until
    /// a non-tool event or a different call breaks the streak.
    fn track_loop(&mut self, tool_name: &str, arguments: &serde_json::Value, now: Instant) {
        let key = format!("{tool_name} {}", canonical_args(arguments));
        if self.loop_key.as_deref() == Some(key.as_str()) {
            self.loop_streak += 1;
        } else {
            self.loop_key = Some(key);
            self.loop_streak = 1;
            // A different call breaks an active loop mark.
            if matches!(self.health, Health::Looping { .. }) {
                self.enter(Health::ToolRunning {
                    name: tool_name.to_string(),
                    since: now,
                });
                return;
            }
        }
        if self.loop_streak >= LOOP_STREAK {
            let call = self
                .last_activity
                .clone()
                .unwrap_or_else(|| tool_name.to_string());
            self.enter(Health::Looping { call });
        } else {
            self.enter(Health::ToolRunning {
                name: tool_name.to_string(),
                since: now,
            });
        }
    }

    fn enter(&mut self, health: Health) {
        self.health = health;
    }

    /// The periodic stall check. A running tool exempts the run (long
    /// commands are legitimate work); enforcement wins over reporting when
    /// both thresholds are crossed.
    pub fn tick(&mut self, now: Instant) -> TickOutcome {
        if self.in_flight > 0 {
            return TickOutcome::Idle;
        }
        let idle = now.saturating_duration_since(self.last_event_at);
        if self.idle_budget.is_some_and(|budget| idle >= budget) {
            self.enter(Health::Stalled {
                since: self.last_event_at,
            });
            return TickOutcome::EnforceStall;
        }
        if idle >= self.report_threshold {
            let first = !self.stall_reported;
            self.stall_reported = true;
            self.enter(Health::Stalled {
                since: self.last_event_at,
            });
            if first {
                return TickOutcome::ReportStall;
            }
            self.stall_echo_ticks += 1;
            if self.stall_echo_ticks >= STALL_ECHO_TICKS {
                self.stall_echo_ticks = 0;
                return TickOutcome::ReportStall;
            }
        }
        TickOutcome::Idle
    }

    pub fn health(&self) -> &Health {
        &self.health
    }

    pub fn turns(&self) -> u64 {
        self.turns
    }

    pub fn tool_calls(&self) -> u64 {
        self.tool_calls
    }

    pub fn last_activity(&self) -> Option<&str> {
        self.last_activity.as_deref()
    }

    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    /// The silent window so far — the input of a stall report's wording.
    pub fn idle_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_event_at)
    }

    /// One-line health rendering for the rail / status surfaces.
    pub fn health_line(&self, now: Instant) -> String {
        match &self.health {
            Health::Starting => "starting".to_string(),
            Health::Working => "working".to_string(),
            Health::ToolRunning { name, since } => {
                format!("tool: {name} {}", elapsed_short(now, *since))
            }
            Health::Stalled { since } => format!("stalled {}", elapsed_short(now, *since)),
            Health::Looping { call } => format!("looping: {call}"),
        }
    }
}

/// Compact `45s` / `3m12s` rendering for health lines.
fn elapsed_short(now: Instant, since: Instant) -> String {
    let secs = now.saturating_duration_since(since).as_secs();
    if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Canonical argument rendering for loop detection: object keys sort
/// recursively, so semantically identical calls whose JSON key order
/// drifted still read as the same call.
fn canonical_args(arguments: &serde_json::Value) -> String {
    match arguments {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let inner = entries
                .iter()
                .map(|(k, v)| format!("{}:{}", k, canonical_args(v)))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi::tool::AgentToolResult;
    use pi::types::AgentMessage;

    fn tool_start(name: &str, args: serde_json::Value) -> AgentEvent {
        AgentEvent::ToolExecutionStart {
            tool_call_id: format!("call-{name}"),
            tool_name: name.to_string(),
            arguments: args,
        }
    }

    fn tool_end(name: &str) -> AgentEvent {
        AgentEvent::ToolExecutionEnd {
            tool_call_id: format!("call-{name}"),
            tool_name: name.to_string(),
            result: AgentToolResult::text("ok"),
            is_error: false,
        }
    }

    fn turn_end() -> AgentEvent {
        AgentEvent::TurnEnd {
            message: Box::new(AgentMessage::user("x")),
            tool_results: vec![],
        }
    }

    #[test]
    fn starts_at_starting_and_first_event_moves_to_working() {
        let t0 = Instant::now();
        let mut wd = SubagentWatchdog::new(t0, None);
        assert_eq!(wd.health(), &Health::Starting);
        assert!(wd.observe(&turn_end(), t0));
        assert_eq!(wd.health(), &Health::Working);
        assert_eq!(wd.turns(), 1);
    }

    #[test]
    fn tool_lifecycle_tracks_in_flight_and_tool_running() {
        let t0 = Instant::now();
        let mut wd = SubagentWatchdog::new(t0, None);
        assert!(wd.observe(
            &tool_start("Bash", serde_json::json!({"command": "cargo test"})),
            t0
        ));
        assert!(matches!(wd.health(), Health::ToolRunning { name, .. } if name == "Bash"));
        // A running tool exempts the run from stall detection.
        assert_eq!(wd.tick(t0 + Duration::from_secs(3600)), TickOutcome::Idle);
        assert!(wd.observe(&tool_end("Bash"), t0));
        assert_eq!(wd.health(), &Health::Working);
        assert_eq!(wd.tool_calls(), 1);
        assert_eq!(wd.last_activity(), Some("Bash command=cargo test"));
    }

    #[test]
    fn silent_run_reports_stall_once() {
        let t0 = Instant::now();
        let mut wd = SubagentWatchdog::new(t0, None);
        wd.observe(&turn_end(), t0);
        // Below the report threshold: nothing.
        assert_eq!(wd.tick(t0 + Duration::from_secs(119)), TickOutcome::Idle);
        assert_eq!(
            wd.tick(t0 + Duration::from_millis(DEFAULT_STALL_REPORT_MS)),
            TickOutcome::ReportStall
        );
        assert!(matches!(wd.health(), Health::Stalled { .. }));
        // A continuing episode stays quiet until the echo cadence.
        assert_eq!(wd.tick(t0 + Duration::from_secs(300)), TickOutcome::Idle);
        // Fresh activity clears the episode; a fresh stall re-reports.
        let t1 = t0 + Duration::from_secs(400);
        wd.observe(&turn_end(), t1);
        assert_eq!(wd.health(), &Health::Working);
        assert_eq!(
            wd.tick(t1 + Duration::from_millis(DEFAULT_STALL_REPORT_MS)),
            TickOutcome::ReportStall
        );
    }

    #[test]
    fn stall_report_echoes_periodically_and_resets_on_activity() {
        let t0 = Instant::now();
        let mut wd = SubagentWatchdog::new(t0, None);
        wd.observe(&turn_end(), t0);
        let threshold = Duration::from_millis(DEFAULT_STALL_REPORT_MS);
        assert_eq!(wd.tick(t0 + threshold), TickOutcome::ReportStall);
        // Echoes only every STALL_ECHO_TICKS ticks while the episode lasts.
        for i in 1..STALL_ECHO_TICKS {
            assert_eq!(
                wd.tick(t0 + threshold + Duration::from_secs(5 * i)),
                TickOutcome::Idle
            );
        }
        assert_eq!(
            wd.tick(t0 + threshold + Duration::from_secs(5 * STALL_ECHO_TICKS)),
            TickOutcome::ReportStall
        );
        // Activity resets the episode and the echo count.
        let t1 = t0 + Duration::from_secs(600);
        wd.observe(&turn_end(), t1);
        assert_eq!(wd.tick(t1 + threshold), TickOutcome::ReportStall);
        for i in 1..STALL_ECHO_TICKS {
            assert_eq!(
                wd.tick(t1 + threshold + Duration::from_secs(5 * i)),
                TickOutcome::Idle
            );
        }
    }

    #[test]
    fn loop_detection_ignores_json_key_order() {
        let t0 = Instant::now();
        let mut wd = SubagentWatchdog::new(t0, None);
        let a = serde_json::json!({"path": "src/a.rs", "offset": 1});
        let b = serde_json::json!({"offset": 1, "path": "src/a.rs"});
        for (i, args) in [a.clone(), b, a].into_iter().enumerate() {
            let secs = i as u64;
            wd.observe(&tool_start("Read", args), t0 + Duration::from_secs(secs));
            wd.observe(&tool_end("Read"), t0 + Duration::from_secs(secs));
        }
        assert!(
            matches!(wd.health(), Health::Looping { .. }),
            "key-order drift must not hide a loop: {:?}",
            wd.health()
        );
    }

    #[test]
    fn armed_idle_budget_enforces() {
        let t0 = Instant::now();
        let mut wd = SubagentWatchdog::new(t0, Some(10_000));
        wd.observe(&turn_end(), t0);
        assert_eq!(wd.tick(t0 + Duration::from_secs(9)), TickOutcome::Idle);
        assert_eq!(
            wd.tick(t0 + Duration::from_secs(10)),
            TickOutcome::EnforceStall
        );
        assert!(matches!(wd.health(), Health::Stalled { .. }));
        assert_eq!(wd.idle_for(t0 + Duration::from_secs(10)).as_secs(), 10);
    }

    #[test]
    fn repeated_identical_call_marks_looping() {
        let t0 = Instant::now();
        let mut wd = SubagentWatchdog::new(t0, None);
        let call = tool_start("Read", serde_json::json!({"path": "src/a.rs"}));
        for i in 0..2 {
            wd.observe(&call, t0 + Duration::from_secs(i));
            wd.observe(&tool_end("Read"), t0 + Duration::from_secs(i));
            assert!(!matches!(wd.health(), Health::Looping { .. }));
        }
        wd.observe(&call, t0 + Duration::from_secs(2));
        assert!(
            matches!(wd.health(), Health::Looping { call } if call.contains("Read")),
            "third identical call marks the loop: {:?}",
            wd.health()
        );
        // The loop mark survives the loop's own tool-end…
        wd.observe(&tool_end("Read"), t0 + Duration::from_secs(2));
        assert!(matches!(wd.health(), Health::Looping { .. }));
        // …breaks on a different tool call…
        wd.observe(
            &tool_start("Grep", serde_json::json!({"pattern": "foo"})),
            t0 + Duration::from_secs(3),
        );
        assert!(
            matches!(wd.health(), Health::ToolRunning { name, .. } if name == "Grep"),
            "a different call clears the loop mark: {:?}",
            wd.health()
        );
        wd.observe(&tool_end("Grep"), t0 + Duration::from_secs(3));
        // …and on any other activity.
        wd.observe(&turn_end(), t0 + Duration::from_secs(4));
        assert_eq!(wd.health(), &Health::Working);
    }

    #[test]
    fn different_calls_do_not_loop() {
        let t0 = Instant::now();
        let mut wd = SubagentWatchdog::new(t0, None);
        for i in 0..5 {
            wd.observe(
                &tool_start("Read", serde_json::json!({"path": format!("src/{i}.rs")})),
                t0 + Duration::from_secs(i),
            );
            wd.observe(&tool_end("Read"), t0 + Duration::from_secs(i));
        }
        assert!(!matches!(wd.health(), Health::Looping { .. }));
        assert_eq!(wd.tool_calls(), 5);
    }

    #[test]
    fn health_line_renders_state_and_elapsed() {
        let t0 = Instant::now();
        let mut wd = SubagentWatchdog::new(t0, None);
        assert_eq!(wd.health_line(t0), "starting");
        wd.observe(&tool_start("Bash", serde_json::json!({})), t0);
        assert_eq!(
            wd.health_line(t0 + Duration::from_secs(75)),
            "tool: Bash 1m15s"
        );
    }
}
