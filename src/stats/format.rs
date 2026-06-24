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
/// 正常值保留 1 位小数（如 `42.3%`）。
/// 当值大于 0 但四舍五入后会变成 `0.0%`（即 pct < 0.05），
/// 显示为 `<0.1%` 以避免歧义——`0.0%` 容易让人误以为模型未被使用。
pub(super) fn format_share(pct: f64) -> String {
    if pct > 0.0 && pct < 0.05 {
        "<0.1%".to_string()
    } else {
        format!("{:.1}%", pct)
    }
}

#[cfg(test)]
mod tests {
    use super::{format_share, format_tokens};

    #[test]
    fn format_share_normal_values() {
        assert_eq!(format_share(42.3), "42.3%");
        assert_eq!(format_share(0.1), "0.1%");
        assert_eq!(format_share(100.0), "100.0%");
        assert_eq!(format_share(0.0), "0.0%");
    }

    #[test]
    fn format_share_small_nonzero_values() {
        // 大于 0 但四舍五入为 0.0% 的值，显示 <0.1% 避免歧义
        assert_eq!(format_share(0.04), "<0.1%");
        assert_eq!(format_share(0.01), "<0.1%");
        assert_eq!(format_share(0.001), "<0.1%");
    }

    #[test]
    fn format_share_boundary() {
        // 0.05 是四舍五入的边界：0.05 → 0.1%（刚好不会变成 0.0%）
        assert_eq!(format_share(0.05), "0.1%");
        // 0.049 四舍五入为 0.0%，所以显示 <0.1%
        assert_eq!(format_share(0.049), "<0.1%");
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
