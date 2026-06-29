//! 共享数据模型与视图/周期枚举。

#[derive(Debug, Clone)]
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

    pub(super) fn add(&mut self, other: &UsageTotals) {
        self.in_tokens = self.in_tokens.saturating_add(other.in_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.out_tokens = self.out_tokens.saturating_add(other.out_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Period {
    All,
    Today,
    Lastday,
    LastDays(u16),
    LastMonthDays,
}

impl Period {
    pub(super) fn label(self, today: &str) -> String {
        match self {
            Period::All => "All time".to_string(),
            Period::Today => "Today".to_string(),
            Period::Lastday => "Yesterday".to_string(),
            Period::LastDays(days) => format!("Last {days} days"),
            Period::LastMonthDays => {
                let days = super::date::previous_month_days(today);
                format!("Last {days} days")
            }
        }
    }

    pub(super) fn cycle(self) -> Self {
        match self {
            Period::All => Period::Today,
            Period::Today => Period::Lastday,
            Period::Lastday => Period::LastDays(7),
            Period::LastDays(_) => Period::LastMonthDays,
            Period::LastMonthDays => Period::All,
        }
    }

    pub(super) fn includes(self, date: &str, today: &str) -> bool {
        match self {
            Period::All => true,
            Period::Today => date == today,
            Period::Lastday => super::date::days_diff(date, today) == Some(1),
            Period::LastDays(days) => super::date::days_diff(date, today)
                .is_some_and(|d| (0..i64::from(days)).contains(&d)),
            Period::LastMonthDays => {
                let days = super::date::previous_month_days(today) as i64;
                super::date::days_diff(date, today).is_some_and(|d| (0..days).contains(&d))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RaceInterval {
    AllTime,
    LastMonthDays,
}

impl RaceInterval {
    pub(super) fn label(self, today: &str) -> String {
        match self {
            RaceInterval::AllTime => "All time".to_string(),
            RaceInterval::LastMonthDays => {
                let days = super::date::previous_month_days(today);
                format!("Last {days} days")
            }
        }
    }

    pub(super) fn cycle(self) -> Self {
        match self {
            RaceInterval::AllTime => RaceInterval::LastMonthDays,
            RaceInterval::LastMonthDays => RaceInterval::AllTime,
        }
    }

    pub(super) fn includes(self, date: &str, today: &str) -> bool {
        match self {
            RaceInterval::AllTime => true,
            RaceInterval::LastMonthDays => {
                let days = super::date::previous_month_days(today) as i64;
                super::date::days_diff(date, today).is_some_and(|d| (0..days).contains(&d))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RaceWindow {
    PerDay,
    Rolling7,
}

impl RaceWindow {
    pub(super) fn label(self) -> &'static str {
        match self {
            RaceWindow::PerDay => "Per day",
            RaceWindow::Rolling7 => "Rolling 7 days",
        }
    }

    pub(super) fn cycle(self) -> Self {
        match self {
            RaceWindow::PerDay => RaceWindow::Rolling7,
            RaceWindow::Rolling7 => RaceWindow::PerDay,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_periods_exclude_future_dates() {
        assert!(Period::LastDays(7).includes("2026-05-23", "2026-05-29"));
        assert!(Period::LastDays(7).includes("2026-05-29", "2026-05-29"));
        assert!(!Period::LastDays(7).includes("2026-05-22", "2026-05-29"));
        assert!(!Period::LastDays(7).includes("2026-05-30", "2026-05-29"));

        assert!(Period::LastMonthDays.includes("2026-04-30", "2026-05-29"));
        assert!(!Period::LastMonthDays.includes("2026-04-29", "2026-05-29"));
        assert!(!Period::LastMonthDays.includes("2026-05-30", "2026-05-29"));
    }

    #[test]
    fn custom_last_days_period_uses_exact_window_size() {
        assert!(Period::LastDays(10).includes("2026-05-20", "2026-05-29"));
        assert!(Period::LastDays(10).includes("2026-05-29", "2026-05-29"));
        assert!(!Period::LastDays(10).includes("2026-05-19", "2026-05-29"));
        assert!(!Period::LastDays(10).includes("2026-05-30", "2026-05-29"));
    }

    #[test]
    fn today_only_includes_today() {
        assert!(Period::Today.includes("2026-05-29", "2026-05-29"));
        assert!(!Period::Today.includes("2026-05-28", "2026-05-29"));
        assert!(!Period::Today.includes("2026-05-30", "2026-05-29"));
    }

    #[test]
    fn lastday_only_includes_yesterday() {
        assert!(Period::Lastday.includes("2026-05-28", "2026-05-29"));
        assert!(!Period::Lastday.includes("2026-05-29", "2026-05-29"));
        assert!(!Period::Lastday.includes("2026-05-27", "2026-05-29"));
        assert!(!Period::Lastday.includes("2026-05-30", "2026-05-29"));
    }

    #[test]
    fn race_interval_cycle_alternates() {
        assert_eq!(RaceInterval::AllTime.cycle(), RaceInterval::LastMonthDays);
        assert_eq!(RaceInterval::LastMonthDays.cycle(), RaceInterval::AllTime);
    }

    #[test]
    fn race_interval_label() {
        assert_eq!(RaceInterval::AllTime.label("2026-06-24"), "All time");
        // 6 月 → 上月 5 月有 31 天
        assert_eq!(
            RaceInterval::LastMonthDays.label("2026-06-24"),
            "Last 31 days"
        );
    }

    #[test]
    fn race_interval_last_month_days_includes_matches_period_last30() {
        // 6 月 24 → 上月 5 月有 31 天，所以 Last 31 days 覆盖 5 月 25 ~ 6 月 24（不含 5 月 24）
        // 这与 Period::LastMonthDays 的行为一致：(0..days).contains
        assert!(RaceInterval::LastMonthDays.includes("2026-05-25", "2026-06-24"));
        assert!(RaceInterval::LastMonthDays.includes("2026-06-24", "2026-06-24"));
        assert!(!RaceInterval::LastMonthDays.includes("2026-05-24", "2026-06-24"));
        assert!(!RaceInterval::LastMonthDays.includes("2026-06-25", "2026-06-24"));
    }

    #[test]
    fn race_window_cycle_alternates() {
        assert_eq!(RaceWindow::PerDay.cycle(), RaceWindow::Rolling7);
        assert_eq!(RaceWindow::Rolling7.cycle(), RaceWindow::PerDay);
    }

    #[test]
    fn race_window_label() {
        assert_eq!(RaceWindow::PerDay.label(), "Per day");
        assert_eq!(RaceWindow::Rolling7.label(), "Rolling 7 days");
    }
}
