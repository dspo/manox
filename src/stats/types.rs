//! 共享数据模型与视图/周期枚举。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct UsageRecord {
    pub(super) agent: String,
    pub(super) model: String,
    /// `YYYY-MM-DD`
    pub(super) date: String,
    pub(super) in_tokens: u64,
    pub(super) total_tokens: u64,
    pub(super) out_tokens: u64,
    pub(super) cache_read_input_tokens: u64,
    pub(super) cache_creation_input_tokens: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct UsageTotals {
    pub(super) in_tokens: u64,
    pub(super) total_tokens: u64,
    pub(super) out_tokens: u64,
    pub(super) cache_read_input_tokens: u64,
    pub(super) cache_creation_input_tokens: u64,
}

impl UsageTotals {
    /// 视图主统计口径：与 Claude Code Stats 的模型排行/折线统计一致，只使用 input + output。
    /// cache read/create 单独展示，不参与模型排序、占比和折线总量。
    pub(super) fn total_tokens(self) -> u64 {
        self.in_tokens + self.out_tokens
    }

    pub(super) fn add_record(&mut self, record: &UsageRecord) {
        self.in_tokens += record.in_tokens;
        self.total_tokens += record.total_tokens;
        self.out_tokens += record.out_tokens;
        self.cache_read_input_tokens += record.cache_read_input_tokens;
        self.cache_creation_input_tokens += record.cache_creation_input_tokens;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CacheEntry {
    pub(super) mtime_secs: u64,
    pub(super) size: u64,
    pub(super) raw: Vec<super::parser::RawEntry>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct ScanCache {
    pub(super) version: u32,
    pub(super) files: HashMap<String, CacheEntry>,
}

impl ScanCache {
    pub(super) fn new(version: u32) -> Self {
        Self {
            version,
            files: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Period {
    All,
    Today,
    Last7,
    Last30,
}

impl Period {
    pub(super) fn label(self, today: &str) -> String {
        match self {
            Period::All => "All time".to_string(),
            Period::Today => "Today".to_string(),
            Period::Last7 => "Last 7 days".to_string(),
            Period::Last30 => {
                let days = super::date::previous_month_days(today);
                format!("Last {days} days")
            }
        }
    }

    pub(super) fn cycle(self) -> Self {
        match self {
            Period::All => Period::Today,
            Period::Today => Period::Last7,
            Period::Last7 => Period::Last30,
            Period::Last30 => Period::All,
        }
    }

    pub(super) fn includes(self, date: &str, today: &str) -> bool {
        match self {
            Period::All => true,
            Period::Today => date == today,
            Period::Last7 => {
                super::date::days_diff(date, today).is_some_and(|d| (0..7).contains(&d))
            }
            Period::Last30 => {
                let days = super::date::previous_month_days(today) as i64;
                super::date::days_diff(date, today).is_some_and(|d| (0..days).contains(&d))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_periods_exclude_future_dates() {
        assert!(Period::Last7.includes("2026-05-23", "2026-05-29"));
        assert!(Period::Last7.includes("2026-05-29", "2026-05-29"));
        assert!(!Period::Last7.includes("2026-05-22", "2026-05-29"));
        assert!(!Period::Last7.includes("2026-05-30", "2026-05-29"));

        assert!(Period::Last30.includes("2026-04-30", "2026-05-29"));
        assert!(!Period::Last30.includes("2026-04-29", "2026-05-29"));
        assert!(!Period::Last30.includes("2026-05-30", "2026-05-29"));
    }

    #[test]
    fn today_only_includes_today() {
        assert!(Period::Today.includes("2026-05-29", "2026-05-29"));
        assert!(!Period::Today.includes("2026-05-28", "2026-05-29"));
        assert!(!Period::Today.includes("2026-05-30", "2026-05-29"));
    }
}
