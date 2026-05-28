//! 日期工具，无外部依赖（chrono 仅在 RFC3339 解析处使用）。

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde_json::Value;

pub(super) fn today_date_string() -> Result<String> {
    Ok(Local::now().format("%Y-%m-%d").to_string())
}

pub(super) fn date_from_iso(s: &str) -> String {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return dt.with_timezone(&Local).format("%Y-%m-%d").to_string();
    }
    if s.len() >= 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' {
        s[..10].to_string()
    } else {
        String::new()
    }
}

pub(super) fn date_field(value: Option<&Value>) -> Option<String> {
    let date = date_from_iso(value.and_then(Value::as_str).unwrap_or_default());
    if date.is_empty() { None } else { Some(date) }
}

/// Howard Hinnant date 算法：unix 秒 → "YYYY-MM-DD"（UTC）。
fn unix_to_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp.wrapping_sub(9) };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", year, m, d)
}

pub(super) fn parse_ymd(s: &str) -> Option<(i64, u32, u32)> {
    if s.len() < 10 {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let m: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    Some((y, m, d))
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

pub(super) fn date_offset(s: &str, days: i64) -> Result<String> {
    let (y, m, d) = parse_ymd(s).context("无法解析日期")?;
    let new_days = days_from_civil(y, m, d) + days;
    Ok(unix_to_date(new_days * 86_400))
}

pub(super) fn days_diff(date: &str, today: &str) -> Option<i64> {
    let (y1, m1, d1) = parse_ymd(today)?;
    let (y2, m2, d2) = parse_ymd(date)?;
    Some(days_from_civil(y1, m1, d1) - days_from_civil(y2, m2, d2))
}
