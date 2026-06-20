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

#[cfg(test)]
mod tests {
    use super::format_tokens;

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
