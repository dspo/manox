//! cx stats — Token 用量统计 TUI
//!
//! 扫描各 agent 的本地日志，聚合 (agent, model, date) 维度的 token 用量，
//! 提供 Models / Matrix 两种 TUI 视图。

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use dirs::home_dir;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Axis, Block, Borders, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ──────────────────────────────────────────────────────────
// 常量
// ──────────────────────────────────────────────────────────

const CACHE_VERSION: u32 = 3;

const AGENT_CLAUDE: &str = "claude";
const AGENT_CODEX: &str = "codex";
const AGENT_ZED: &str = "zed";
const AGENT_CX: &str = "cx-agent";

const MATRIX_AGENTS: &[(&str, &str)] = &[
    (AGENT_CLAUDE, "claude code"),
    (AGENT_CODEX, "codex"),
    (AGENT_ZED, "zed agent"),
    (AGENT_CX, "cx agent"),
];

// 折线图调色板（与 Claude /usage 风格相近）
const PALETTE: &[Color] = &[
    Color::Cyan,
    Color::LightYellow,
    Color::LightGreen,
    Color::LightMagenta,
    Color::LightRed,
    Color::LightBlue,
    Color::Yellow,
    Color::Green,
];

type PlotPoint = (f64, f64);
type DatasetData = (String, Vec<PlotPoint>, Color);

// ──────────────────────────────────────────────────────────
// 数据模型
// ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UsageRecord {
    agent: String,
    model: String,
    /// `YYYY-MM-DD`
    date: String,
    in_tokens: u64,
    total_tokens: u64,
    out_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct UsageTotals {
    in_tokens: u64,
    total_tokens: u64,
    out_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
}

impl UsageTotals {
    // Claude Code 的主统计口径按 In + Out 计算，不把缓存 token 算进图表/排序/占比。
    fn total_tokens(self) -> u64 {
        self.in_tokens + self.out_tokens
    }

    fn add_record(&mut self, record: &UsageRecord) {
        self.in_tokens += record.in_tokens;
        self.total_tokens += record.total_tokens;
        self.out_tokens += record.out_tokens;
        self.cache_read_input_tokens += record.cache_read_input_tokens;
        self.cache_creation_input_tokens += record.cache_creation_input_tokens;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    mtime_secs: u64,
    size: u64,
    records: Vec<UsageRecord>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ScanCache {
    version: u32,
    files: HashMap<String, CacheEntry>,
}

impl ScanCache {
    fn new() -> Self {
        Self {
            version: CACHE_VERSION,
            files: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Models,
    Matrix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Period {
    All,
    Last7,
    Last30,
}

impl Period {
    fn label(self) -> &'static str {
        match self {
            Period::All => "All time",
            Period::Last7 => "Last 7 days",
            Period::Last30 => "Last 30 days",
        }
    }

    fn cycle(self) -> Self {
        match self {
            Period::All => Period::Last7,
            Period::Last7 => Period::Last30,
            Period::Last30 => Period::All,
        }
    }

    fn includes(self, date: &str, today: &str) -> bool {
        match self {
            Period::All => true,
            Period::Last7 => days_diff(date, today).is_some_and(|d| d < 7),
            Period::Last30 => days_diff(date, today).is_some_and(|d| d < 30),
        }
    }
}

// ──────────────────────────────────────────────────────────
// 入口
// ──────────────────────────────────────────────────────────

pub fn run_stats() -> Result<()> {
    let today = today_date_string()?;

    let cache_path = cache_path()?;
    let mut cache = load_cache(&cache_path).unwrap_or_else(|_| ScanCache::new());
    if cache.version != CACHE_VERSION {
        cache = ScanCache::new();
    }

    let mut all_records: Vec<UsageRecord> = Vec::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();

    for source in log_sources() {
        if !source.root.exists() {
            continue;
        }
        let files = collect_jsonl_files(&source.root, source.kind);
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

            let records = if let Some(entry) = cache.files.get(&path_key) {
                if entry.mtime_secs == mtime && entry.size == size {
                    entry.records.clone()
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
                    records: records.clone(),
                },
            );

            for r in records {
                all_records.push(r);
            }
        }
    }

    // 清理已删除文件的缓存条目
    cache.files.retain(|k, _| visited.contains(k));
    let _ = save_cache(&cache_path, &cache);

    if std::env::var("CX_STATS_DUMP").ok().as_deref() == Some("1") {
        return dump_records(&all_records, &today);
    }

    run_tui(all_records, today)
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
            format_tokens(usage.in_tokens),
            format_tokens(usage.out_tokens),
            format_tokens(usage.cache_read_input_tokens),
            format_tokens(usage.cache_creation_input_tokens),
            days.len()
        );
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────
// 日志源 & 文件扫描
// ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum SourceKind {
    Claude,
    CodexLike(&'static str, CodexUsageMode), // agent_id
}

#[derive(Debug, Clone, Copy)]
enum CodexUsageMode {
    TotalUsageDelta,
    LastUsageOnly,
}

struct LogSource {
    root: PathBuf,
    kind: SourceKind,
}

fn log_sources() -> Vec<LogSource> {
    let home = match home_dir() {
        Some(h) => h,
        None => return Vec::new(),
    };
    vec![
        LogSource {
            root: home.join(".claude/projects"),
            kind: SourceKind::Claude,
        },
        LogSource {
            root: home.join(".codex/sessions"),
            kind: SourceKind::CodexLike(AGENT_CODEX, CodexUsageMode::TotalUsageDelta),
        },
        LogSource {
            root: home.join("Library/Application Support/Zed/codex/sessions"),
            kind: SourceKind::CodexLike(AGENT_ZED, CodexUsageMode::TotalUsageDelta),
        },
        LogSource {
            root: home.join(".local/share/cx/cx-agent-sessions"),
            kind: SourceKind::CodexLike(AGENT_CX, CodexUsageMode::LastUsageOnly),
        },
    ]
}

/// 递归收集 *.jsonl 文件。
fn collect_jsonl_files(root: &Path, kind: SourceKind) -> Vec<PathBuf> {
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
            if matches!(kind, SourceKind::Claude) && is_claude_subagent_log(&path) {
                continue;
            }
            out.push(path);
        }
    }
    out
}

fn is_claude_subagent_log(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str().to_str() == Some("subagents"))
}

// ──────────────────────────────────────────────────────────
// 解析
// ──────────────────────────────────────────────────────────

fn parse_file(path: &Path, kind: SourceKind) -> Vec<UsageRecord> {
    let content = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    match kind {
        SourceKind::Claude => parse_claude_jsonl(&content),
        SourceKind::CodexLike(agent, usage_mode) => {
            let fallback_date = fallback_date_from_path(path);
            parse_codex_jsonl(&content, agent, fallback_date.as_deref(), usage_mode)
        }
    }
}

fn parse_claude_jsonl(content: &str) -> Vec<UsageRecord> {
    let Some(session_date) = claude_session_date(content) else {
        return Vec::new();
    };
    let mut acc: BTreeMap<String, UsageTotals> = BTreeMap::new();

    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let message = match v.get("message") {
            Some(m) => m,
            None => continue,
        };
        let usage = match message.get("usage") {
            Some(u) => u,
            None => continue,
        };
        let model = message
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if model.is_empty() || model == "<synthetic>" {
            continue;
        }

        let input_tokens = u64_field(usage, "input_tokens");
        let cache_read = u64_field(usage, "cache_read_input_tokens");
        let cache_create = u64_field(usage, "cache_creation_input_tokens");
        let out_tokens = u64_field(usage, "output_tokens");
        let total_tokens = input_tokens.saturating_add(out_tokens);
        if input_tokens == 0 && cache_read == 0 && cache_create == 0 && out_tokens == 0 {
            continue;
        }

        let entry = acc.entry(model).or_default();
        entry.in_tokens += input_tokens;
        entry.total_tokens += total_tokens;
        entry.out_tokens += out_tokens;
        entry.cache_read_input_tokens += cache_read;
        entry.cache_creation_input_tokens += cache_create;
    }

    acc.into_iter()
        .map(|(model, totals)| UsageRecord {
            agent: AGENT_CLAUDE.to_string(),
            model,
            date: session_date.clone(),
            in_tokens: totals.in_tokens,
            total_tokens: totals.total_tokens,
            out_tokens: totals.out_tokens,
            cache_read_input_tokens: totals.cache_read_input_tokens,
            cache_creation_input_tokens: totals.cache_creation_input_tokens,
        })
        .collect()
}

fn claude_session_date(content: &str) -> Option<String> {
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(date) = date_field(v.get("timestamp")) {
            return Some(date);
        }
    }
    None
}

fn parse_codex_jsonl(
    content: &str,
    agent: &str,
    fallback_date: Option<&str>,
    usage_mode: CodexUsageMode,
) -> Vec<UsageRecord> {
    let mut current_model: Option<String> = None;
    let mut current_date: Option<String> = None;
    let mut previous_total_usage: Option<(u64, u64, u64, u64)> = None;
    let mut acc: BTreeMap<(String, String), UsageTotals> = BTreeMap::new();

    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let typ = v.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = match v.get("payload") {
            Some(p) => p,
            None => continue,
        };
        let event_date = codex_like_event_date(&v, payload);
        if let Some(date) = &event_date {
            current_date = Some(date.clone());
        }

        if typ == "session_meta" {
            // 部分 session_meta 也带 model 字段，作为兜底
            if let Some(m) = payload.get("model").and_then(Value::as_str) {
                current_model = Some(m.to_string());
            }
            continue;
        }
        if typ == "turn_context" {
            if let Some(m) = payload.get("model").and_then(Value::as_str) {
                current_model = Some(m.to_string());
            }
            continue;
        }
        if typ != "event_msg" {
            continue;
        }
        if payload.get("type").and_then(Value::as_str) != Some("token_count") {
            continue;
        }
        let info = match payload.get("info") {
            Some(i) if !i.is_null() => i,
            _ => continue,
        };
        let total = info
            .get("total_token_usage")
            .filter(|total| !total.is_null());
        let last = info.get("last_token_usage").filter(|last| !last.is_null());
        let (input_tokens, cached_input_tokens, cache_creation_input_tokens, out_tokens) =
            match usage_mode {
                CodexUsageMode::TotalUsageDelta => {
                    let Some(total) = total else {
                        continue;
                    };
                    let total_input_tokens = u64_field(total, "input_tokens");
                    let total_cached_input_tokens = u64_field(total, "cached_input_tokens");
                    let total_cache_creation_input_tokens =
                        u64_field(total, "cache_creation_input_tokens");
                    let total_out_tokens = u64_field(total, "output_tokens");
                    let (prev_input, prev_cached, prev_cache_create, prev_out) =
                        previous_total_usage.unwrap_or((0, 0, 0, 0));
                    previous_total_usage = Some((
                        total_input_tokens,
                        total_cached_input_tokens,
                        total_cache_creation_input_tokens,
                        total_out_tokens,
                    ));
                    (
                        total_input_tokens.saturating_sub(prev_input),
                        total_cached_input_tokens.saturating_sub(prev_cached),
                        total_cache_creation_input_tokens.saturating_sub(prev_cache_create),
                        total_out_tokens.saturating_sub(prev_out),
                    )
                }
                CodexUsageMode::LastUsageOnly => {
                    let Some(last) = last else {
                        continue;
                    };
                    (
                        u64_field(last, "input_tokens"),
                        u64_field(last, "cached_input_tokens"),
                        u64_field(last, "cache_creation_input_tokens"),
                        u64_field(last, "output_tokens"),
                    )
                }
            };
        let total_tokens = input_tokens.saturating_add(out_tokens);
        if input_tokens == 0
            && cached_input_tokens == 0
            && cache_creation_input_tokens == 0
            && out_tokens == 0
        {
            continue;
        }

        let date = event_date
            .or_else(|| current_date.clone())
            .or_else(|| fallback_date.map(str::to_string))
            .unwrap_or_default();
        if date.is_empty() {
            continue;
        }
        let model = current_model
            .clone()
            .unwrap_or_else(|| "unknown".to_string());

        let entry = acc.entry((model, date)).or_default();
        entry.in_tokens += input_tokens;
        entry.total_tokens += total_tokens;
        entry.out_tokens += out_tokens;
        entry.cache_read_input_tokens += cached_input_tokens;
        entry.cache_creation_input_tokens += cache_creation_input_tokens;
    }

    acc.into_iter()
        .map(|((model, date), totals)| UsageRecord {
            agent: agent.to_string(),
            model,
            date,
            in_tokens: totals.in_tokens,
            total_tokens: totals.total_tokens,
            out_tokens: totals.out_tokens,
            cache_read_input_tokens: totals.cache_read_input_tokens,
            cache_creation_input_tokens: totals.cache_creation_input_tokens,
        })
        .collect()
}

fn u64_field(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn fallback_date_from_path(path: &Path) -> Option<String> {
    let mut cursor = path.parent();
    while let Some(dir) = cursor {
        match dir.file_name().and_then(|s| s.to_str()) {
            Some(name) if parse_ymd(name).is_some() => return Some(name.to_string()),
            _ => {}
        }
        cursor = dir.parent();
    }
    None
}

fn codex_like_event_date(v: &Value, payload: &Value) -> Option<String> {
    date_field(v.get("timestamp"))
        .or_else(|| date_field(payload.get("timestamp")))
        .or_else(|| date_field(payload.get("at")))
        .or_else(|| date_field(payload.get("started_at")))
        .or_else(|| date_field(payload.get("info").and_then(|info| info.get("at"))))
        .or_else(|| date_field(payload.get("info").and_then(|info| info.get("timestamp"))))
        .or_else(|| {
            date_field(
                payload
                    .get("info")
                    .and_then(|info| info.get("last_token_usage"))
                    .and_then(|last| last.get("at")),
            )
        })
        .or_else(|| {
            date_field(
                payload
                    .get("info")
                    .and_then(|info| info.get("last_token_usage"))
                    .and_then(|last| last.get("timestamp")),
            )
        })
}

fn date_field(value: Option<&Value>) -> Option<String> {
    let date = date_from_iso(value.and_then(Value::as_str).unwrap_or_default());
    if date.is_empty() { None } else { Some(date) }
}

// ──────────────────────────────────────────────────────────
// 缓存
// ──────────────────────────────────────────────────────────

fn cache_path() -> Result<PathBuf> {
    let home = home_dir().context("无法解析用户主目录")?;
    Ok(home.join(".local/share/cx/stats-cache.json"))
}

fn load_cache(path: &Path) -> Result<ScanCache> {
    let bytes = fs::read(path)?;
    let cache: ScanCache = serde_json::from_slice(&bytes)?;
    Ok(cache)
}

fn save_cache(path: &Path, cache: &ScanCache) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(cache)?;
    fs::write(path, json)?;
    Ok(())
}

// ──────────────────────────────────────────────────────────
// 日期工具（不引入 chrono）
// ──────────────────────────────────────────────────────────

fn today_date_string() -> Result<String> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 Unix Epoch")?
        .as_secs() as i64;
    Ok(unix_to_date(secs))
}

fn date_from_iso(s: &str) -> String {
    if s.len() >= 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' {
        s[..10].to_string()
    } else {
        String::new()
    }
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
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp.wrapping_sub(9) }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", year, m, d)
}

/// "YYYY-MM-DD" → unix 秒（当日 00:00 UTC）。

fn parse_ymd(s: &str) -> Option<(i64, u32, u32)> {
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

fn date_offset(s: &str, days: i64) -> Result<String> {
    let (y, m, d) = parse_ymd(s).context("无法解析日期")?;
    let new_days = days_from_civil(y, m, d) + days;
    Ok(unix_to_date(new_days * 86_400))
}

fn days_diff(date: &str, today: &str) -> Option<i64> {
    let (y1, m1, d1) = parse_ymd(today)?;
    let (y2, m2, d2) = parse_ymd(date)?;
    Some(days_from_civil(y1, m1, d1) - days_from_civil(y2, m2, d2))
}

// ──────────────────────────────────────────────────────────
// TUI 状态
// ──────────────────────────────────────────────────────────

struct StatsApp {
    records: Vec<UsageRecord>,
    today: String,
    view: View,
    period: Period,
    models_scroll: usize,
    matrix_scroll: usize,
}

impl StatsApp {
    fn new(records: Vec<UsageRecord>, today: String) -> Self {
        Self {
            records,
            today,
            view: View::Models,
            period: Period::Last7,
            models_scroll: 0,
            matrix_scroll: 0,
        }
    }

    /// 当前 period 内的记录。
    fn period_records(&self) -> Vec<&UsageRecord> {
        self.records
            .iter()
            .filter(|r| self.period.includes(&r.date, &self.today))
            .collect()
    }
}

// ──────────────────────────────────────────────────────────
// TUI 主循环
// ──────────────────────────────────────────────────────────

fn run_tui(records: Vec<UsageRecord>, today: String) -> Result<()> {
    enable_raw_mode().context("启用 raw mode 失败")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("进入 alt screen 失败")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("初始化 terminal 失败")?;

    let result = event_loop(&mut terminal, records, today);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    records: Vec<UsageRecord>,
    today: String,
) -> Result<()> {
    let mut app = StatsApp::new(records, today);
    loop {
        terminal.draw(|f| draw(f, &mut app))?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Tab | KeyCode::BackTab => {
                app.view = match app.view {
                    View::Models => View::Matrix,
                    View::Matrix => View::Models,
                };
            }
            KeyCode::Char('1') => app.period = Period::Last7,
            KeyCode::Char('2') => app.period = Period::Last30,
            KeyCode::Char('3') => app.period = Period::All,
            KeyCode::Char('r') => app.period = app.period.cycle(),
            KeyCode::Down | KeyCode::Char('j') => match app.view {
                View::Models => app.models_scroll = app.models_scroll.saturating_add(1),
                View::Matrix => app.matrix_scroll = app.matrix_scroll.saturating_add(1),
            },
            KeyCode::Up | KeyCode::Char('k') => match app.view {
                View::Models => app.models_scroll = app.models_scroll.saturating_sub(1),
                View::Matrix => app.matrix_scroll = app.matrix_scroll.saturating_sub(1),
            },
            _ => {}
        }
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────
// 渲染
// ──────────────────────────────────────────────────────────

fn draw(f: &mut ratatui::Frame, app: &mut StatsApp) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header tabs
            Constraint::Min(0),    // body
            Constraint::Length(1), // footer hint
        ])
        .split(area);

    draw_header(f, chunks[0], app);
    match app.view {
        View::Models => draw_models_view(f, chunks[1], app),
        View::Matrix => draw_matrix_view(f, chunks[1], app),
    }
    draw_footer(f, chunks[2], app);
}

fn draw_header(f: &mut ratatui::Frame, area: Rect, app: &StatsApp) {
    let active_style = Style::default()
        .fg(Color::Black)
        .bg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let inactive_style = Style::default().fg(Color::Gray);

    let models_span = if app.view == View::Models {
        Span::styled(" Models ", active_style)
    } else {
        Span::styled(" Models ", inactive_style)
    };
    let matrix_span = if app.view == View::Matrix {
        Span::styled(" Matrix ", active_style)
    } else {
        Span::styled(" Matrix ", inactive_style)
    };

    let title = Line::from(vec![
        Span::styled(
            " cx stats ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("· Token Usage Dashboard   "),
        models_span,
        Span::raw(" "),
        matrix_span,
    ]);

    let block = Block::default().borders(Borders::BOTTOM);
    let p = Paragraph::new(title).block(block);
    f.render_widget(p, area);
}

fn draw_footer(f: &mut ratatui::Frame, area: Rect, app: &StatsApp) {
    let period_hint = match app.period {
        Period::Last7 => "[1] 7d  2 30d  3 All",
        Period::Last30 => "1 7d  [2] 30d  3 All",
        Period::All => "1 7d  2 30d  [3] All",
    };
    let text = format!("[Tab] toggle view   {period_hint}   r cycle dates   ↑↓ scroll   q quit");
    let p = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(p, area);
}

// ──────────────────────────────────────────────────────────
// Models 视图
// ──────────────────────────────────────────────────────────

fn draw_models_view(f: &mut ratatui::Frame, area: Rect, app: &mut StatsApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),   // chart
            Constraint::Length(1), // period switch row
            Constraint::Min(0),    // model list
        ])
        .split(area);

    draw_tokens_per_day_chart(f, chunks[0], app);
    draw_period_switch(f, chunks[1], app);
    draw_model_list(f, chunks[2], app);
}

fn draw_tokens_per_day_chart(f: &mut ratatui::Frame, area: Rect, app: &StatsApp) {
    let records = app.period_records();
    if records.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "  No data in selected period.",
            Style::default().fg(Color::DarkGray),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Tokens per Day "),
        );
        f.render_widget(p, area);
        return;
    }

    // 折线图：仅显示累计占比达到 80% 的头部模型（按总 token 排序）。
    let totals = totals_by_model(&records);
    let top_models: Vec<String> = top_models_covering(&totals, 0.80);

    // 找出 period 的日期范围
    let mut min_date = app.today.clone();
    let mut max_date = "0000-00-00".to_string();
    for r in &records {
        if r.date < min_date {
            min_date = r.date.clone();
        }
        if r.date > max_date {
            max_date = r.date.clone();
        }
    }
    if max_date == "0000-00-00" {
        return;
    }

    let day_count = (days_diff(&min_date, &max_date).unwrap_or(0).max(0) + 1) as usize;
    let day_count = day_count.max(1);

    // 每个模型每天的 token 数（In + Out）
    let mut series: HashMap<String, Vec<f64>> = HashMap::new();
    for m in &top_models {
        series.insert(m.clone(), vec![0.0; day_count]);
    }
    for r in &records {
        let idx = days_diff(&min_date, &r.date).unwrap_or(0).max(0) as usize;
        if idx >= day_count {
            continue;
        }
        let tokens = r.total_tokens as f64;
        if let Some(v) = series.get_mut(&r.model) {
            v[idx] += tokens;
        }
    }

    let mut max_y: f64 = 1.0;
    let mut datasets_data: Vec<DatasetData> = Vec::new();
    for (idx, model) in top_models.iter().enumerate() {
        let color = PALETTE[idx % PALETTE.len()];
        let pts: Vec<PlotPoint> = series
            .get(model)
            .map(|v| {
                v.iter()
                    .enumerate()
                    .map(|(i, &y)| {
                        if y > max_y {
                            max_y = y;
                        }
                        (i as f64, y)
                    })
                    .collect()
            })
            .unwrap_or_default();
        datasets_data.push((model.clone(), pts, color));
    }

    let datasets: Vec<Dataset> = datasets_data
        .iter()
        .map(|(name, data, color)| {
            Dataset::default()
                .name(name.clone())
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(*color))
                .data(data)
        })
        .collect();

    let x_labels: Vec<Span> = if day_count <= 1 {
        vec![Span::raw(min_date.clone())]
    } else {
        let mid_idx = day_count / 2;
        let mid_date = date_offset(&min_date, mid_idx as i64).unwrap_or_else(|_| String::new());
        vec![
            Span::styled(short_date(&min_date), Style::default().fg(Color::DarkGray)),
            Span::styled(short_date(&mid_date), Style::default().fg(Color::DarkGray)),
            Span::styled(short_date(&max_date), Style::default().fg(Color::DarkGray)),
        ]
    };

    let y_max_label = format_tokens(max_y as u64);
    let y_mid_label = format_tokens((max_y / 2.0) as u64);

    let chart = Chart::new(datasets)
        .block(Block::default().borders(Borders::ALL).title(format!(
            " Tokens per Day (in + out) · {} ",
            app.period.label()
        )))
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, (day_count.saturating_sub(1)) as f64])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, max_y * 1.05])
                .labels(vec![
                    Span::styled("0", Style::default().fg(Color::DarkGray)),
                    Span::styled(y_mid_label, Style::default().fg(Color::DarkGray)),
                    Span::styled(y_max_label, Style::default().fg(Color::DarkGray)),
                ]),
        );

    f.render_widget(chart, area);
}

fn draw_period_switch(f: &mut ratatui::Frame, area: Rect, app: &StatsApp) {
    let mut spans: Vec<Span> = Vec::new();
    for (i, p) in [Period::Last7, Period::Last30, Period::All]
        .iter()
        .enumerate()
    {
        if i > 0 {
            spans.push(Span::raw(" · "));
        }
        let style = if app.period == *p {
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(p.label().to_string(), style));
    }
    let p = Paragraph::new(Line::from(spans));
    f.render_widget(p, area);
}

fn draw_model_list(f: &mut ratatui::Frame, area: Rect, app: &mut StatsApp) {
    let records = app.period_records();
    let totals = totals_by_model(&records);
    let total_all: u64 = totals.values().map(|usage| usage.total_tokens()).sum();

    if totals.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "  No models in selected period.",
            Style::default().fg(Color::DarkGray),
        )))
        .block(Block::default().borders(Borders::ALL).title(" Models "));
        f.render_widget(p, area);
        return;
    }

    let mut sorted: Vec<(String, UsageTotals)> = totals.into_iter().collect();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1.total_tokens()));

    let visible = area.height.saturating_sub(2) as usize;
    let max_scroll = sorted.len().saturating_sub(visible.max(1));
    if app.models_scroll > max_scroll {
        app.models_scroll = max_scroll;
    }

    let rows: Vec<Row> = sorted
        .iter()
        .enumerate()
        .skip(app.models_scroll)
        .take(visible)
        .map(|(idx, (model, usage))| {
            let pct = if total_all > 0 {
                usage.total_tokens() as f64 * 100.0 / total_all as f64
            } else {
                0.0
            };
            let dot_color = PALETTE[idx % PALETTE.len()];
            Row::new(vec![
                Cell::from(Span::styled("●", Style::default().fg(dot_color))),
                Cell::from(Span::styled(
                    model.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Cell::from(Span::styled(
                    format!("{:.1}%", pct),
                    Style::default().fg(Color::DarkGray),
                )),
                Cell::from(format!("In: {}", format_tokens(usage.in_tokens))),
                Cell::from(format!("Out: {}", format_tokens(usage.out_tokens))),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(2),
        Constraint::Length(28),
        Constraint::Length(8),
        Constraint::Length(26),
        Constraint::Length(16),
    ];

    let shown = sorted.len().saturating_sub(app.models_scroll).min(visible);
    let title = format!(" Models · {} of {} ", shown, sorted.len());
    let table = Table::new(rows, widths).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(table, area);
}

// ──────────────────────────────────────────────────────────
// Matrix 视图
// ──────────────────────────────────────────────────────────

fn draw_matrix_view(f: &mut ratatui::Frame, area: Rect, app: &mut StatsApp) {
    let records = app.period_records();
    let cells = totals_by_agent_model(&records);

    // 按 model 总用量（across all agents）从高到低排序；零用量或无统计的不显示。
    let mut model_totals: HashMap<String, u64> = HashMap::new();
    for ((_, model), usage) in &cells {
        *model_totals.entry(model.clone()).or_insert(0) += usage.total_tokens();
    }
    let mut models: Vec<(String, u64)> = model_totals
        .into_iter()
        .filter(|(_, total)| *total > 0)
        .collect();
    models.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let models: Vec<String> = models.into_iter().map(|(m, _)| m).collect();

    let visible = (area.height.saturating_sub(4) as usize / 2).max(1);
    let max_scroll = models.len().saturating_sub(visible.max(1));
    if app.matrix_scroll > max_scroll {
        app.matrix_scroll = max_scroll;
    }

    let header_cells: Vec<Cell> = std::iter::once(Cell::from(Span::styled(
        "model",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )))
    .chain(MATRIX_AGENTS.iter().map(|(_, label)| {
        Cell::from(Span::styled(
            *label,
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ))
    }))
    .collect();
    let header = Row::new(header_cells).style(Style::default().bg(Color::Reset));

    let rows: Vec<Row> = models
        .iter()
        .skip(app.matrix_scroll)
        .take(visible)
        .map(|model| {
            let mut row_cells: Vec<Cell> = Vec::with_capacity(MATRIX_AGENTS.len() + 1);
            row_cells.push(Cell::from(Span::styled(
                model.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            for (agent, _) in MATRIX_AGENTS {
                let cell = match cells.get(&(agent.to_string(), model.clone())) {
                    Some(usage) => Text::from(vec![
                        Line::from(format!("In: {}", format_tokens(usage.in_tokens))),
                        Line::from(Span::styled(
                            format!("Out: {}", format_tokens(usage.out_tokens)),
                            Style::default().fg(Color::DarkGray),
                        )),
                    ]),
                    None => Text::from(vec![
                        Line::from(Span::styled("—", Style::default().fg(Color::DarkGray))),
                        Line::from(""),
                    ]),
                };
                row_cells.push(Cell::from(cell));
            }
            Row::new(row_cells).height(2)
        })
        .collect();

    let widths = [
        Constraint::Length(28),
        Constraint::Length(24),
        Constraint::Length(24),
        Constraint::Length(24),
        Constraint::Length(24),
    ];

    let title = format!(
        " Agent × Model · {} · In/Out ({}/{}) ",
        app.period.label(),
        models.len().min(app.matrix_scroll + visible),
        models.len()
    );
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(table, area);
}

// ──────────────────────────────────────────────────────────
// 聚合工具
// ──────────────────────────────────────────────────────────

fn totals_by_model(records: &[&UsageRecord]) -> HashMap<String, UsageTotals> {
    let mut map: HashMap<String, UsageTotals> = HashMap::new();
    for r in records {
        let entry = map.entry(r.model.clone()).or_default();
        entry.add_record(r);
    }
    map
}

fn totals_by_agent_model(records: &[&UsageRecord]) -> HashMap<(String, String), UsageTotals> {
    let mut map: HashMap<(String, String), UsageTotals> = HashMap::new();
    for r in records {
        let entry = map.entry((r.agent.clone(), r.model.clone())).or_default();
        entry.add_record(r);
    }
    map
}

/// 按总用量降序取头部模型，直到累计占比 ≥ `ratio`。
/// 至少返回 1 个非空模型（如有），避免折线图为空。
fn top_models_covering(totals: &HashMap<String, UsageTotals>, ratio: f64) -> Vec<String> {
    let mut v: Vec<(String, u64)> = totals
        .iter()
        .map(|(k, usage)| (k.clone(), usage.total_tokens()))
        .filter(|(_, t)| *t > 0)
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let grand_total: u64 = v.iter().map(|(_, t)| *t).sum();
    if grand_total == 0 {
        return Vec::new();
    }
    let threshold = (grand_total as f64 * ratio).ceil() as u64;

    let mut acc: u64 = 0;
    let mut out: Vec<String> = Vec::new();
    for (model, total) in v {
        out.push(model);
        acc = acc.saturating_add(total);
        if acc >= threshold {
            break;
        }
    }
    out
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}b", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn short_date(s: &str) -> String {
    // YYYY-MM-DD → "MMM DD"
    if let Some((_, m, d)) = parse_ymd(s) {
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        return format!("{} {:02}", MONTHS[(m as usize - 1).min(11)], d);
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_like_parser_accepts_payload_timestamp_and_cache_tokens() {
        let content = concat!(
            r#"{"type":"turn_context","payload":{"model":"qwen3.6-plus"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","at":"2026-05-27T12:34:56Z","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"cache_creation_input_tokens":5,"output_tokens":7},"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"cache_creation_input_tokens":5,"output_tokens":7}}}}"#,
            "\n"
        );

        let records =
            parse_codex_jsonl(content, AGENT_CODEX, None, CodexUsageMode::TotalUsageDelta);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].agent, AGENT_CODEX);
        assert_eq!(records[0].model, "qwen3.6-plus");
        assert_eq!(records[0].date, "2026-05-27");
        assert_eq!(records[0].in_tokens, 100);
        assert_eq!(records[0].total_tokens, 107);
        assert_eq!(records[0].out_tokens, 7);
        assert_eq!(records[0].cache_read_input_tokens, 20);
        assert_eq!(records[0].cache_creation_input_tokens, 5);
    }

    #[test]
    fn codex_like_parser_uses_session_or_path_date_for_legacy_cx_agent_rollout() {
        let content = concat!(
            r#"{"type":"session_meta","payload":{"session_id":"cx-agent-abc","agent":"cx-agent","model":"qwen3.6-plus","started_at":"2026-05-28T09:00:00Z"}}"#,
            "\n",
            r#"{"type":"turn_context","payload":{"model":"qwen3.6-plus"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":11,"output_tokens":13}}}}"#,
            "\n"
        );

        let records = parse_codex_jsonl(
            content,
            AGENT_CX,
            Some("2026-05-27"),
            CodexUsageMode::LastUsageOnly,
        );

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].date, "2026-05-28");
        assert_eq!(records[0].in_tokens, 11);
        assert_eq!(records[0].total_tokens, 24);
        assert_eq!(records[0].out_tokens, 13);
    }

    #[test]
    fn codex_like_parser_uses_total_usage_deltas_to_avoid_duplicate_snapshots() {
        let content = concat!(
            r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","at":"2026-05-27T12:34:56Z","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":7},"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":7}}}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","at":"2026-05-27T12:35:56Z","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":7},"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":7}}}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","at":"2026-05-27T12:36:56Z","info":{"total_token_usage":{"input_tokens":150,"cached_input_tokens":30,"output_tokens":17},"last_token_usage":{"input_tokens":50,"cached_input_tokens":10,"output_tokens":10}}}}"#,
            "\n"
        );

        let records =
            parse_codex_jsonl(content, AGENT_CODEX, None, CodexUsageMode::TotalUsageDelta);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].in_tokens, 150);
        assert_eq!(records[0].out_tokens, 17);
        assert_eq!(records[0].cache_read_input_tokens, 30);
        assert_eq!(records[0].total_tokens, 167);
    }

    #[test]
    fn claude_parser_buckets_to_session_start_and_keeps_duplicate_message_ids() {
        let content = concat!(
            r#"{"type":"user","timestamp":"2026-05-26T23:59:59Z","message":{"role":"user","content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-05-27T12:34:56Z","message":{"id":"msg-1","model":"claude-opus-4-7","usage":{"input_tokens":100,"cache_read_input_tokens":20,"cache_creation_input_tokens":5,"output_tokens":7}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-05-27T12:35:56Z","message":{"id":"msg-1","model":"claude-opus-4-7","usage":{"input_tokens":999,"cache_read_input_tokens":999,"cache_creation_input_tokens":999,"output_tokens":999}}}"#,
            "\n"
        );

        let records = parse_claude_jsonl(content);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].agent, AGENT_CLAUDE);
        assert_eq!(records[0].model, "claude-opus-4-7");
        assert_eq!(records[0].date, "2026-05-26");
        assert_eq!(records[0].in_tokens, 1099);
        assert_eq!(records[0].total_tokens, 2105);
        assert_eq!(records[0].out_tokens, 1006);
        assert_eq!(records[0].cache_read_input_tokens, 1019);
        assert_eq!(records[0].cache_creation_input_tokens, 1004);
    }

    #[test]
    fn fallback_date_from_path_reads_parent_day_directory() {
        let path = Path::new("/logs/cx-agent-sessions/2026-05-29/session.jsonl");
        assert_eq!(fallback_date_from_path(path).as_deref(), Some("2026-05-29"));
    }

    #[test]
    fn claude_subagent_logs_are_skipped() {
        let path = Path::new("/logs/projects/foo/session/subagents/agent-1.jsonl");
        assert!(is_claude_subagent_log(path));
    }
}
