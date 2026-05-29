//! cx stats — Token 用量统计 TUI
//!
//! 扫描各 agent 的本地日志，聚合 (agent, model, date) 维度的 token 用量，
//! 提供 Models TUI 视图。
//!
//! 设计参考 ccusage `rust/crates/ccusage/src/adapter/`：
//! - 解析阶段：每个文件解析为 `Vec<RawEntry>`（带去重主键）。
//! - 全局去重阶段：只对解析器显式提供 `dedup_primary` 的 agent 去重。Claude Code
//!   Stats 视图是 raw-sum 口径，因此 Claude 记录不提供去重键。
//! - 聚合阶段：按 (agent, model, date) 累加成 [`UsageRecord`]，供视图使用。

mod aggregate;
mod cache;
mod date;
mod format;
mod parser;
mod tui;
mod types;
mod view;

use anyhow::Result;
use dirs::home_dir;
use ratatui::style::Color;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use cache::{cache_path, load_cache, save_cache};
use parser::{RawEntry, SourceKind, parse_file};
use types::{CacheEntry, ScanCache, UsageRecord, UsageTotals};

const CACHE_VERSION: u32 = 6;

pub(crate) const AGENT_CLAUDE: &str = "claude";
pub(crate) const AGENT_CODEX: &str = "codex";
pub(crate) const AGENT_ZED: &str = "zed";
pub(crate) const AGENT_COPILOT: &str = "copilot";

pub(crate) const MATRIX_AGENTS: &[(&str, &str)] = &[
    (AGENT_CLAUDE, "claude code"),
    (AGENT_CODEX, "codex"),
    (AGENT_ZED, "zed agent"),
    (AGENT_COPILOT, "copilot"),
];

/// 折线图调色板（与 Claude `/usage` 风格相近）。
pub(crate) const PALETTE: &[Color] = &[
    Color::Cyan,
    Color::LightYellow,
    Color::LightGreen,
    Color::LightMagenta,
    Color::LightRed,
    Color::LightBlue,
    Color::Yellow,
    Color::Green,
];

struct LogSource {
    root: PathBuf,
    /// 单文件路径（用于环境变量指定的 copilot 单文件）。
    extra_file: Option<PathBuf>,
    kind: SourceKind,
}

fn log_sources() -> Vec<LogSource> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let copilot_extra = std::env::var("COPILOT_OTEL_FILE_EXPORTER_PATH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_file());
    vec![
        LogSource {
            root: home.join(".claude/projects"),
            extra_file: None,
            kind: SourceKind::Claude,
        },
        LogSource {
            root: home.join(".codex/sessions"),
            extra_file: None,
            kind: SourceKind::CodexLike(AGENT_CODEX),
        },
        LogSource {
            root: home.join("Library/Application Support/Zed/codex/sessions"),
            extra_file: None,
            kind: SourceKind::CodexLike(AGENT_ZED),
        },
        LogSource {
            root: home.join(".copilot/otel"),
            extra_file: copilot_extra,
            kind: SourceKind::Copilot(AGENT_COPILOT),
        },
    ]
}

/// 递归收集 `*.jsonl` 文件。
fn collect_jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            out.push(path);
        }
    }
    out
}

pub fn run_stats() -> Result<()> {
    let today = date::today_date_string()?;

    let cache_path = cache_path()?;
    let mut cache = load_cache(&cache_path).unwrap_or_else(|_| ScanCache::new(CACHE_VERSION));
    if cache.version != CACHE_VERSION {
        cache = ScanCache::new(CACHE_VERSION);
    }

    let mut all_raw: Vec<RawEntry> = Vec::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();

    for source in log_sources() {
        let mut files = if source.root.exists() {
            collect_jsonl_files(&source.root)
        } else {
            Vec::new()
        };
        if let Some(extra) = source.extra_file {
            if !files.iter().any(|p| p == &extra) {
                files.push(extra);
            }
        }
        for path in files {
            let path_key = path.to_string_lossy().to_string();
            visited.insert(path_key.clone());

            let meta = match fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let size = meta.len();

            let raw_entries = if let Some(entry) = cache.files.get(&path_key) {
                if entry.mtime_secs == mtime && entry.size == size {
                    entry.raw.clone()
                } else {
                    parse_file(&path, source.kind)
                }
            } else {
                parse_file(&path, source.kind)
            };

            cache.files.insert(
                path_key,
                CacheEntry {
                    mtime_secs: mtime,
                    size,
                    raw: raw_entries.clone(),
                },
            );

            all_raw.extend(raw_entries);
        }
    }

    cache.files.retain(|k, _| visited.contains(k));
    let _ = save_cache(&cache_path, &cache);

    let deduped = dedupe(all_raw);
    let records = bucket(deduped);

    if std::env::var("CX_STATS_DUMP").ok().as_deref() == Some("1") {
        return dump_records(&records, &today);
    }

    tui::run_tui(records, today)
}

/// 跨文件去重。
///
/// 参考 ccusage `adapter/claude/mod.rs:234 push_deduped_entry`：
/// - exact 命中（agent + dedup_primary + dedup_secondary）：保留 token 多者。
/// - sidechain 兜底：当 candidate 是 sidechain replay（同 message_id 不同 requestId）时，
///   找已有的 parent，用 token 多者保留。
fn dedupe(raw: Vec<RawEntry>) -> Vec<RawEntry> {
    let mut deduped: Vec<RawEntry> = Vec::with_capacity(raw.len());
    let mut by_exact: HashMap<(String, String, Option<String>), usize> = HashMap::new();
    let mut by_message: HashMap<(String, String), Vec<usize>> = HashMap::new();

    for entry in raw {
        let Some(primary) = entry.dedup_primary.clone() else {
            deduped.push(entry);
            continue;
        };
        let exact_key = (
            entry.agent.clone(),
            primary.clone(),
            entry.dedup_secondary.clone(),
        );
        let message_key = (entry.agent.clone(), primary.clone());

        if let Some(&idx) = by_exact.get(&exact_key) {
            if should_replace(&entry, &deduped[idx]) {
                deduped[idx] = entry;
            }
            continue;
        }

        // sidechain 兜底：相同 message_id，不同 secondary，且至少一方是 sidechain
        if let Some(indexes) = by_message.get(&message_key) {
            let candidate_is_sidechain = entry.is_sidechain;
            if let Some(&idx) = indexes.iter().find(|&&i| {
                let existing = &deduped[i];
                candidate_is_sidechain || existing.is_sidechain
            }) {
                if should_replace(&entry, &deduped[idx]) {
                    deduped[idx] = entry.clone();
                    by_exact.insert(exact_key, idx);
                }
                continue;
            }
        }

        let idx = deduped.len();
        deduped.push(entry);
        by_exact.insert(exact_key, idx);
        by_message.entry(message_key).or_default().push(idx);
    }

    deduped
}

fn should_replace(candidate: &RawEntry, existing: &RawEntry) -> bool {
    if candidate.is_sidechain != existing.is_sidechain {
        return existing.is_sidechain;
    }
    let total = |e: &RawEntry| -> u64 {
        e.input_tokens + e.output_tokens + e.cache_read_input_tokens + e.cache_creation_input_tokens
    };
    total(candidate) > total(existing)
}

fn bucket(deduped: Vec<RawEntry>) -> Vec<UsageRecord> {
    let mut acc: BTreeMap<(String, String, String), UsageTotals> = BTreeMap::new();
    for e in deduped {
        let key = (e.agent.clone(), e.model.clone(), e.date.clone());
        let t = acc.entry(key).or_default();
        t.in_tokens += e.input_tokens;
        t.out_tokens += e.output_tokens;
        t.cache_read_input_tokens += e.cache_read_input_tokens;
        t.cache_creation_input_tokens += e.cache_creation_input_tokens;
    }
    acc.into_iter()
        .map(|((agent, model, date), t)| UsageRecord {
            agent,
            model,
            date,
            in_tokens: t.in_tokens,
            // total_tokens 字段保留为兼容 dump，但显示口径以 UsageTotals::total_tokens() 为准。
            total_tokens: t.total_tokens(),
            out_tokens: t.out_tokens,
            cache_read_input_tokens: t.cache_read_input_tokens,
            cache_creation_input_tokens: t.cache_creation_input_tokens,
        })
        .collect()
}

fn dump_records(records: &[UsageRecord], today: &str) -> Result<()> {
    let mut by_agent_model: BTreeMap<(String, String), (UsageTotals, BTreeSet<String>)> =
        BTreeMap::new();
    for r in records {
        let entry = by_agent_model
            .entry((r.agent.clone(), r.model.clone()))
            .or_insert((UsageTotals::default(), BTreeSet::new()));
        entry.0.add_record(r);
        entry.1.insert(r.date.clone());
    }
    println!("today: {today}  total records: {}", records.len());
    println!(
        "{:<10} {:<28} {:>14} {:>14} {:>14} {:>14} {:>5}",
        "agent", "model", "in", "out", "cache_read", "cache_create", "days"
    );
    for ((agent, model), (usage, days)) in &by_agent_model {
        println!(
            "{:<10} {:<28} {:>14} {:>14} {:>14} {:>14} {:>5}",
            agent,
            model,
            format::format_tokens(usage.in_tokens),
            format::format_tokens(usage.out_tokens),
            format::format_tokens(usage.cache_read_input_tokens),
            format::format_tokens(usage.cache_creation_input_tokens),
            days.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        agent: &str,
        primary: Option<&str>,
        secondary: Option<&str>,
        is_sidechain: bool,
        input: u64,
        output: u64,
        cache_read: u64,
    ) -> RawEntry {
        RawEntry {
            agent: agent.to_string(),
            model: "m".to_string(),
            date: "2026-05-27".to_string(),
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 0,
            dedup_primary: primary.map(str::to_string),
            dedup_secondary: secondary.map(str::to_string),
            is_sidechain,
        }
    }

    #[test]
    fn dedupes_identical_message_id_and_request_id_across_files() {
        let raw = vec![
            entry("claude", Some("msg-1"), Some("req-1"), false, 100, 7, 20),
            entry("claude", Some("msg-1"), Some("req-1"), false, 100, 7, 20),
        ];
        let d = dedupe(raw);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].input_tokens, 100);
    }

    #[test]
    fn keeps_larger_token_count_on_collision() {
        let raw = vec![
            entry("claude", Some("msg-1"), Some("req-1"), false, 50, 3, 0),
            entry("claude", Some("msg-1"), Some("req-1"), false, 100, 7, 0),
        ];
        let d = dedupe(raw);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].input_tokens, 100);
    }

    #[test]
    fn sidechain_replay_collapses_into_parent() {
        let raw = vec![
            entry(
                "claude",
                Some("msg-1"),
                Some("req-parent"),
                false,
                100,
                7,
                20,
            ),
            entry(
                "claude",
                Some("msg-1"),
                Some("req-replay"),
                true,
                50,
                7,
                50_000,
            ),
        ];
        let d = dedupe(raw);
        // sidechain replay 即便 token 更多，也不应顶替非 sidechain 的 parent。
        // 参见 ccusage `should_replace_deduped_entry`：只有 existing 为 sidechain 时才允许替换。
        assert_eq!(d.len(), 1);
        assert!(!d[0].is_sidechain);
        assert_eq!(d[0].cache_read_input_tokens, 20);
    }

    #[test]
    fn parent_replaces_sidechain_when_arrived_later() {
        let raw = vec![
            entry("claude", Some("msg-1"), Some("req-replay"), true, 1, 1, 1),
            entry(
                "claude",
                Some("msg-1"),
                Some("req-parent"),
                false,
                10,
                10,
                10,
            ),
        ];
        let d = dedupe(raw);
        // 非 sidechain 总是优先于 sidechain（不论 token 多少），参考 ccusage should_replace
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].is_sidechain, false);
    }

    #[test]
    fn keeps_unrelated_messages_separately() {
        let raw = vec![
            entry("claude", Some("msg-a"), Some("r"), false, 1, 1, 0),
            entry("claude", Some("msg-b"), Some("r"), false, 2, 2, 0),
            entry("codex", Some("msg-a"), Some("r"), false, 3, 3, 0),
        ];
        let d = dedupe(raw);
        assert_eq!(d.len(), 3);
    }

    #[test]
    fn entries_without_dedup_primary_are_passed_through() {
        let raw = vec![
            entry("x", None, None, false, 1, 1, 0),
            entry("x", None, None, false, 2, 2, 0),
        ];
        let d = dedupe(raw);
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn usage_total_is_input_plus_output_only() {
        let usage = UsageTotals {
            in_tokens: 100,
            out_tokens: 50,
            cache_read_input_tokens: 25,
            cache_creation_input_tokens: 10,
            ..UsageTotals::default()
        };
        assert_eq!(usage.total_tokens(), 150);
    }

    #[test]
    fn buckets_by_agent_model_date() {
        let raw = vec![
            entry("claude", Some("a"), None, false, 10, 5, 3),
            entry("claude", Some("b"), None, false, 20, 7, 4),
        ];
        let recs = bucket(raw);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].in_tokens, 30);
        assert_eq!(recs[0].out_tokens, 12);
        assert_eq!(recs[0].cache_read_input_tokens, 7);
    }

    #[test]
    fn codex_resume_collapses_duplicate_token_count_across_files() {
        // 模拟：同一逻辑 codex session 在 resume 后写到第二个 rollout 文件。
        // session_meta.payload.id 相同，导致 RawEntry 的 dedup_primary（"sid|ts"）相同；
        // 同 token 数相同（dedup_secondary 相同）→ 应被合并为一条。
        // 参考 ccusage `codex_event_key`：(session_id, ts, model, tokens) 元组去重。
        let dup = vec![
            super::parser::RawEntry {
                agent: "codex".to_string(),
                model: "gpt-5".to_string(),
                date: "2026-05-27".to_string(),
                input_tokens: 100,
                output_tokens: 7,
                cache_read_input_tokens: 20,
                cache_creation_input_tokens: 0,
                reasoning_output_tokens: 0,
                dedup_primary: Some("sess-uuid-A|2026-05-27T12:34:56Z".to_string()),
                dedup_secondary: Some("100/20/0/7/0".to_string()),
                is_sidechain: false,
            },
            super::parser::RawEntry {
                agent: "codex".to_string(),
                model: "gpt-5".to_string(),
                date: "2026-05-27".to_string(),
                input_tokens: 100,
                output_tokens: 7,
                cache_read_input_tokens: 20,
                cache_creation_input_tokens: 0,
                reasoning_output_tokens: 0,
                dedup_primary: Some("sess-uuid-A|2026-05-27T12:34:56Z".to_string()),
                dedup_secondary: Some("100/20/0/7/0".to_string()),
                is_sidechain: false,
            },
        ];
        let d = dedupe(dup);
        assert_eq!(d.len(), 1);
        let recs = bucket(d);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].in_tokens, 100);
        assert_eq!(recs[0].out_tokens, 7);
    }
}
