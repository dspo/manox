//! Runtime overlap diagnostics for the message list, enabled by
//! `MANOX_OVERLAP_DIAG=1`.
//!
//! Each frame records the prepaint bounds of every list row wrapper
//! (`record_row`), the row-index → message-id mapping (`record_mapping`), and
//! every message body (`record_body`). The list wrapper's `on_prepaint` — which
//! runs before that frame's row/body callbacks fill fresh records — drains and
//! cross-checks the records accumulated by the previous frame, so each check
//! compares a completed frame. A body escaping its row is the overlap signature
//! and is appended to `/tmp/manox-overlap-diag.log` with enough context to
//! localize the fault.
//!
//! The RichText leaf runs its own paint-height check (same log file) from the
//! components crate, which cannot depend on this module.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use gpui::{Bounds, Pixels, px};

static ENABLED: OnceLock<bool> = OnceLock::new();

pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var("MANOX_OVERLAP_DIAG")
            .map(|value| value == "1")
            .unwrap_or(false)
    })
}

pub fn log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("manox-overlap-diag.log")
}

pub fn append_log(line: &str) {
    use std::io::Write as _;
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    else {
        return;
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let _ = writeln!(file, "[{timestamp}] {line}");
}

#[derive(Default)]
struct FrameRecords {
    rows: HashMap<usize, Bounds<Pixels>>,
    bodies: HashMap<usize, Bounds<Pixels>>,
    mapping: HashMap<usize, usize>,
}

static CURRENT: OnceLock<Mutex<FrameRecords>> = OnceLock::new();

fn current() -> &'static Mutex<FrameRecords> {
    CURRENT.get_or_init(|| Mutex::new(FrameRecords::default()))
}

pub fn record_row(ix: usize, bounds: Bounds<Pixels>) {
    current().lock().unwrap().rows.insert(ix, bounds);
}

pub fn record_mapping(ix: usize, message_id: usize) {
    current().lock().unwrap().mapping.insert(ix, message_id);
}

pub fn record_body(message_id: usize, bounds: Bounds<Pixels>) {
    current().lock().unwrap().bodies.insert(message_id, bounds);
}

/// Rotate the frame records and check the just-completed frame. Rows and
/// bodies come from the same painted frame; a body whose top/bottom escapes
/// its mapped row is appended to the diagnostic log.
pub fn check_completed_frame(list_bounds: Bounds<Pixels>, item_count: usize) {
    let finished = {
        let mut records = current().lock().unwrap();
        std::mem::take(&mut *records)
    };
    if finished.rows.is_empty() {
        return;
    }
    let mut violations = Vec::new();
    for (ix, message_id) in &finished.mapping {
        let Some(row) = finished.rows.get(ix) else {
            continue;
        };
        let Some(body) = finished.bodies.get(message_id) else {
            continue;
        };
        let tolerance = px(1.);
        if body.top() < row.top() - tolerance || body.bottom() > row.bottom() + tolerance {
            violations.push(format!(
                "OVERLAP row ix={ix} id={message_id} row={} body={} list={} items={item_count}",
                format_bounds(*row),
                format_bounds(*body),
                format_bounds(list_bounds),
            ));
        }
    }
    for (orphan, body) in &finished.bodies {
        let mapped = finished.mapping.values().any(|mapped| mapped == orphan);
        // Bodies without a mapped row render outside the list entirely
        // (overlay cards); only flag ones intersecting the list viewport.
        if !mapped && list_bounds.intersects(body) {
            violations.push(format!(
                "ORPHAN_BODY id={orphan} body={} list={}",
                format_bounds(*body),
                format_bounds(list_bounds),
            ));
        }
    }
    if !violations.is_empty() {
        append_log(&violations.join("\n"));
    }
}

fn format_bounds(bounds: Bounds<Pixels>) -> String {
    format!(
        "({:.0},{:.0} {:.0}x{:.0})",
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.y),
        f32::from(bounds.size.width),
        f32::from(bounds.size.height)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    // One test: the registry + log file are process-global, so splitting these
    // cases across parallel tests would race on the shared frame records.
    #[test]
    fn escape_and_orphan_are_logged_contained_is_not() {
        let _ = std::fs::remove_file(log_path());
        // Row ix 0 spans y 100..200; body 42 spans y 100..300 → escapes 100px.
        record_mapping(0, 42);
        record_row(0, bounds(0., 100., 800., 100.));
        record_body(42, bounds(0., 100., 800., 200.));
        // Row ix 1 contains its body cleanly.
        record_mapping(1, 43);
        record_row(1, bounds(0., 200., 800., 120.));
        record_body(43, bounds(0., 205., 800., 110.));
        // Body 99 has no row mapping but intersects the list viewport.
        record_body(99, bounds(10., 10., 200., 50.));
        check_completed_frame(bounds(0., 0., 800., 600.), 2);

        let log = std::fs::read_to_string(log_path()).expect("diagnostic log written");
        assert!(
            log.contains("OVERLAP row ix=0 id=42"),
            "escaping body must be reported, got: {log}"
        );
        assert!(
            !log.contains("id=43"),
            "contained body must not be reported, got: {log}"
        );
        assert!(
            log.contains("ORPHAN_BODY id=99"),
            "orphan body inside the list must be reported, got: {log}"
        );
        let _ = std::fs::remove_file(log_path());
    }
}
