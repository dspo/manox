//! OMP (Oh My Pi) stats 解析器，从 SQLite 数据库读取聚合数据。
//!
//! OMP 在 `~/.omp/stats.db` 中维护 `messages` 表，记录每次 LLM 调用的用量。
//! 解析时执行 GROUP BY day, model, provider 的聚合查询，产出已聚合的 [`RawEntry`]。

use rusqlite::Connection;
use std::path::Path;

use super::RawEntry;

/// OMP 的 agent 标识。
const AGENT_OMP: &str = "omp";

/// 从 OMP SQLite 数据库读取聚合后的用量数据。
///
/// 执行按 (day, model, provider) 聚合的查询，将每一行转换为一个 [`RawEntry`]。
/// 由于数据已经聚合，`dedup_primary` 为 `None`。
pub(crate) fn parse_omp_db(db_path: &Path) -> Vec<RawEntry> {
    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut stmt = match conn.prepare(
        "SELECT
            date(timestamp / 1000, 'unixepoch') AS day,
            model,
            provider,
            SUM(input_tokens)   AS input_tokens,
            SUM(output_tokens)  AS output_tokens,
            SUM(cache_read_tokens)  AS cache_read,
            SUM(cache_write_tokens) AS cache_write
         FROM messages
         GROUP BY day, model, provider
         ORDER BY day DESC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let rows = match stmt.query_map([], |row| {
        let day: String = row.get(0)?;
        let model: String = row.get(1)?;
        let input_tokens: u64 = row.get::<_, i64>(3)?.max(0) as u64;
        let output_tokens: u64 = row.get::<_, i64>(4)?.max(0) as u64;
        let cache_read: u64 = row.get::<_, i64>(5)?.max(0) as u64;
        let cache_write: u64 = row.get::<_, i64>(6)?.max(0) as u64;
        Ok(RawEntry {
            agent: AGENT_OMP.to_string(),
            model,
            date: day,
            input_tokens,
            output_tokens,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_write,
            reasoning_output_tokens: 0,
            dedup_primary: None,
            dedup_secondary: None,
            is_sidechain: false,
        })
    }) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    rows.filter_map(|r| r.ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::PathBuf;

    fn create_test_db() -> PathBuf {
        let path = std::env::temp_dir().join("cx_test_omp_stats.db");
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                timestamp INTEGER,
                model TEXT,
                provider TEXT,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER,
                total_tokens INTEGER,
                cost_total REAL
            );
            INSERT INTO messages VALUES
                (1748918400000, 'claude-sonnet-4-20250514', 'anthropic', 1000, 500, 200, 0, 1500, 0.012);
            INSERT INTO messages VALUES
                (1748918400000, 'claude-sonnet-4-20250514', 'anthropic', 800, 300, 100, 0, 1100, 0.008);
            INSERT INTO messages VALUES
                (1749004800000, 'gpt-5.4', 'openai', 2000, 800, 0, 0, 2800, 0.025);
            ",
        )
        .unwrap();
        path
    }

    #[test]
    fn aggregates_by_day_model_provider() {
        let db_path = create_test_db();
        let entries = parse_omp_db(&db_path);
        let _ = std::fs::remove_file(&db_path);

        assert_eq!(
            entries.len(),
            2,
            "should have 2 rows (2 unique day+model+provider combos)"
        );

        let mut sorted = entries.clone();
        sorted.sort_by(|a, b| {
            a.date
                .cmp(&b.date)
                .then_with(|| a.model.cmp(&b.model))
        });

        // 2025-06-03: claude-sonnet-4-20250514 — aggregated (1000+800=1800, 500+300=800)
        assert_eq!(sorted[0].date, "2025-06-03");
        assert_eq!(sorted[0].agent, "omp");
        assert_eq!(sorted[0].model, "claude-sonnet-4-20250514");
        assert_eq!(sorted[0].input_tokens, 1800);
        assert_eq!(sorted[0].output_tokens, 800);
        assert_eq!(sorted[0].cache_read_input_tokens, 300);
        assert_eq!(sorted[0].cache_creation_input_tokens, 0);
        assert_eq!(sorted[0].dedup_primary, None);
        assert_eq!(sorted[0].is_sidechain, false);

        // 2025-06-04: gpt-5.4
        assert_eq!(sorted[1].date, "2025-06-04");
        assert_eq!(sorted[1].agent, "omp");
        assert_eq!(sorted[1].model, "gpt-5.4");
        assert_eq!(sorted[1].input_tokens, 2000);
        assert_eq!(sorted[1].output_tokens, 800);
        assert_eq!(sorted[1].cache_read_input_tokens, 0);
        assert_eq!(sorted[1].cache_creation_input_tokens, 0);
    }

    #[test]
    fn handles_empty_file() {
        let path = std::env::temp_dir().join("cx_test_omp_empty.db");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "").unwrap();

        let entries = parse_omp_db(&path);
        let _ = std::fs::remove_file(&path);

        assert!(entries.is_empty());
    }

    #[test]
    fn handles_missing_file() {
        let entries = parse_omp_db(std::path::Path::new(
            "/tmp/cx_test_nonexistent_omp_stats.db",
        ));
        assert!(entries.is_empty());
    }
}