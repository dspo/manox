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

/// Parse an ISO 8601 timestamp string into Unix seconds since epoch.
/// Returns `None` if the string cannot be parsed or yields a pre-1970 timestamp.
pub(super) fn timestamp_secs_from_iso(s: &str) -> Option<u64> {
    DateTime::parse_from_rfc3339(s).ok().and_then(|dt| {
        let secs = dt.timestamp();
        if secs >= 0 { Some(secs as u64) } else { None }
    })
}

/// Unix seconds → `"YYYY-MM-DD"` (UTC).
pub(super) fn date_from_unix_secs(secs: i64) -> String {
    unix_to_date(secs)
}

/// Howard Hinnant date algorithm: unix seconds → "YYYY-MM-DD" (UTC).
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

/// 计算上个月有多少天（根据今天的日期）。
/// 例如今天是 2026-06-01，则返回 5 月的天数 31。
/// 如果日期解析失败，回退返回 30（4 个月中大多数月份的近似值）。
pub(super) fn previous_month_days(today: &str) -> i64 {
    let Some((year, month, _)) = parse_ymd(today) else {
        return 30;
    };
    let prev_year = if month == 1 { year - 1 } else { year };
    let prev_month = if month == 1 { 12 } else { month - 1 };
    let days = match prev_month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (prev_year % 4 == 0 && prev_year % 100 != 0) || (prev_year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 30,
    };
    days
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_from_unix_secs_converts_to_ymd() {
        assert_eq!(date_from_unix_secs(0), "1970-01-01");
        assert_eq!(date_from_unix_secs(1_779_772_684), "2026-05-26");
    }

    #[test]
    fn previous_month_days_varies_by_month() {
        // 1 月 → 上年 12 月，31 天
        assert_eq!(previous_month_days("2026-01-15"), 31);
        // 2 月 → 1 月，31 天
        assert_eq!(previous_month_days("2026-02-01"), 31);
        // 3 月 → 2 月，平年 28 天
        assert_eq!(previous_month_days("2026-03-01"), 28);
        // 4 月 → 3 月，31 天
        assert_eq!(previous_month_days("2026-04-01"), 31);
        // 5 月 → 4 月，30 天
        assert_eq!(previous_month_days("2026-05-01"), 30);
        // 6 月 → 5 月，31 天
        assert_eq!(previous_month_days("2026-06-01"), 31);
        // 7 月 → 6 月，30 天
        assert_eq!(previous_month_days("2026-07-01"), 30);
        // 8 月 → 7 月，31 天
        assert_eq!(previous_month_days("2026-08-01"), 31);
        // 9 月 → 8 月，31 天
        assert_eq!(previous_month_days("2026-09-01"), 31);
        // 10 月 → 9 月，30 天
        assert_eq!(previous_month_days("2026-10-01"), 30);
        // 11 月 → 10 月，31 天
        assert_eq!(previous_month_days("2026-11-01"), 31);
        // 12 月 → 11 月，30 天
        assert_eq!(previous_month_days("2026-12-01"), 30);
    }

    #[test]
    fn previous_month_days_handles_leap_year() {
        // 闰年 2 月
        assert_eq!(previous_month_days("2020-03-01"), 29);
        // 非闰年 2 月
        assert_eq!(previous_month_days("2021-03-01"), 28);
        // 整百年非闰年
        assert_eq!(previous_month_days("1900-03-01"), 28);
        // 整四百年闰年
        assert_eq!(previous_month_days("2000-03-01"), 29);
    }

    #[test]
    fn previous_month_days_returns_30_on_invalid_date() {
        assert_eq!(previous_month_days("invalid"), 30);
    }
}
