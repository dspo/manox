//! 显示格式化辅助。

use chrono::{DateTime, Local, Utc};

pub(super) fn format_tokens(n: u64) -> String {
    const BILLION: u64 = 1_000_000_000;
    const MILLION: u64 = 1_000_000;
    const THOUSAND: u64 = 1_000;

    if n >= BILLION {
        let billions = n / BILLION;
        let remainder_in_millions = (n % BILLION) / MILLION;
        if remainder_in_millions == 0 {
            format!("{}b", billions)
        } else {
            format!("{}b,{}m", billions, remainder_in_millions)
        }
    } else if n >= MILLION {
        let millions = n / MILLION;
        let remainder_in_thousands = (n % MILLION) / THOUSAND;
        if remainder_in_thousands == 0 {
            format!("{}m", millions)
        } else {
            format!("{}m,{}k", millions, remainder_in_thousands)
        }
    } else if n >= THOUSAND {
        let thousands = n / THOUSAND;
        let remainder = n % THOUSAND;
        if remainder == 0 {
            format!("{}k", thousands)
        } else {
            format!("{}k,{}", thousands, remainder)
        }
    } else {
        n.to_string()
    }
}

pub(super) fn short_date(s: &str) -> String {
    if let Some((_, m, d)) = super::date::parse_ymd(s) {
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        return format!("{} {:02}", MONTHS[(m as usize - 1).min(11)], d);
    }
    s.to_string()
}

/// 将 unix 毫秒转换为 RFC3339 字符串，保留毫秒精度（用于 copilot OTel 时间戳归一化）。
pub(super) fn iso_from_unix_ms(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.with_timezone(&Local).to_rfc3339())
        .unwrap_or_default()
}

/// 格式化占比百分比。
///
/// - 整数位 ≥ 2 位（≥10%）：保留 1 位小数，如 `52.8%`、`10.3%`
/// - 整数位仅 1 位（<10%）：保留 2 位小数，如 `1.20%`、`0.55%`
/// - 最小显示值 `0.01%`：大于 0 但四舍五入后会显示为 `0.00%` 的值，
///   上取整为 `0.01%` 以避免歧义
/// - `0.0%` 仅出现在实际占比为零的情况
pub(super) fn format_share(pct: f64) -> String {
    if pct == 0.0 {
        return "0.00%".to_string();
    }
    // 大于 0 但按精度四舍五入后会显示为 0.00% 的值 → 上取整为 0.01%
    let floor_pct = if pct >= 10.0 {
        (pct * 10.0).floor() / 10.0 // 1 位小数精度
    } else {
        (pct * 100.0).floor() / 100.0 // 2 位小数精度
    };
    let effective = if floor_pct == 0.0 { 0.01 } else { floor_pct };
    if effective >= 10.0 {
        format!("{:.1}%", effective)
    } else {
        format!("{:.2}%", effective)
    }
}

#[cfg(test)]
mod tests {
    use super::{format_share, format_tokens};

    #[test]
    fn format_share_two_digit_integer() {
        // 整数位 ≥ 2 位 → 1 位小数
        assert_eq!(format_share(52.83), "52.8%");
        assert_eq!(format_share(10.34), "10.3%");
        assert_eq!(format_share(100.0), "100.0%");
        assert_eq!(format_share(99.99), "99.9%");
    }

    #[test]
    fn format_share_one_digit_integer() {
        // 整数位仅 1 位 → 2 位小数
        assert_eq!(format_share(1.204), "1.20%");
        assert_eq!(format_share(0.557), "0.55%");
        assert_eq!(format_share(9.876), "9.87%");
    }

    #[test]
    fn format_share_minimum() {
        // 大于 0 但四舍五入为 0.00% → 上取整为 0.01%
        assert_eq!(format_share(0.004), "0.01%");
        assert_eq!(format_share(0.001), "0.01%");
        assert_eq!(format_share(0.009), "0.01%");
        // 刚好在 0.01% 边界
        assert_eq!(format_share(0.01), "0.01%");
    }

    #[test]
    fn format_share_zero() {
        assert_eq!(format_share(0.0), "0.00%");
    }

    #[test]
    fn format_tokens_uses_integer_units_with_remainder() {
        assert_eq!(format_tokens(1_357_000_000), "1b,357m");
        assert_eq!(format_tokens(1_000_000_000), "1b");
        assert_eq!(format_tokens(1_300_000), "1m,300k");
        assert_eq!(format_tokens(1_000_000), "1m");
        assert_eq!(format_tokens(1_300), "1k,300");
        assert_eq!(format_tokens(1_000), "1k");
        assert_eq!(format_tokens(999), "999");
    }
}
