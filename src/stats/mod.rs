//! cx stats — Token 用量统计 TUI
//!
//! 扫描各 agent 的本地日志，将 per-message 明细存入 cx.db，
//! 按 (agent, model, date) 聚合展示。
//!
//! 增量更新：变化的源文件 DELETE+INSERT，未变化的跳过。
//! 跨文件去重（codex/copilot）在 db insert 时处理。
//! 聚合用 SQL GROUP BY 实时计算，不需要单独的聚合表。

mod aggregate;
mod date;
mod db;
mod format;
#[cfg(feature = "image-output")]
mod image;
mod parser;
mod svg;
mod tui;
mod types;
mod view;

use anyhow::Result;
#[cfg(not(feature = "image-output"))]
use anyhow::anyhow;
use dirs::home_dir;
use ratatui::style::Color;
use std::collections::BTreeSet;
use std::fs;
use std::fs::Metadata;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use parser::{SourceKind, parse_file, parse_file_from_offset};
use types::UsageRecord;

/// 统计输出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Svg,
    Png,
    Jpg,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "svg" => Some(Self::Svg),
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpg),
            _ => None,
        }
    }
    #[cfg(feature = "image-output")]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Svg => "svg",
            Self::Png => "png",
            Self::Jpg => "jpg",
        }
    }
}

/// 统计视图选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsView {
    Overview,
    Race,
}

impl StatsView {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "overview" => Some(Self::Overview),
            "race" => Some(Self::Race),
            _ => None,
        }
    }
}

/// CLI 可指定的时间段（映射到内部 Period enum）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsPeriod {
    Today,
    Yesterday,
    Last7Days,
    Last30Days,
    All,
}

impl StatsPeriod {
    /// 从 CLI 简称解析：today / yda / 7d / 30d / all。
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "today" => Some(Self::Today),
            "yda" | "yesterday" => Some(Self::Yesterday),
            "7d" | "last-7-days" => Some(Self::Last7Days),
            "30d" | "last-30-days" | "mo" => Some(Self::Last30Days),
            "all" => Some(Self::All),
            _ => None,
        }
    }
    fn to_period(self) -> types::Period {
        match self {
            Self::Today => types::Period::Today,
            Self::Yesterday => types::Period::Lastday,
            Self::Last7Days => types::Period::Last7,
            Self::Last30Days => types::Period::Last30,
            Self::All => types::Period::All,
        }
    }
}

/// `run_stats()` 的配置参数。
#[derive(Debug, Clone)]
pub struct StatsOutputConfig {
    /// 输出格式；None 表示交互式 TUI。
    pub output_format: Option<OutputFormat>,
    /// 视图选择。
    pub view: StatsView,
    /// 时间段；None 表示默认 Last7。
    pub period: Option<StatsPeriod>,
}

impl Default for StatsOutputConfig {
    fn default() -> Self {
        Self {
            output_format: None,
            view: StatsView::Overview,
            period: None,
        }
    }
}

pub(crate) const AGENT_CLAUDE: &str = "claude";
pub(crate) const AGENT_CODEX: &str = "codex";
pub(crate) const AGENT_ZED: &str = "zed";
pub(crate) const AGENT_COPILOT: &str = "copilot";
pub(crate) const AGENT_OMP: &str = "omp";
pub(crate) const AGENT_MIMO: &str = "mimo";
pub(crate) const AGENT_REMORA: &str = "remora";

pub(crate) const MATRIX_AGENTS: &[(&str, &str)] = &[
    (AGENT_CLAUDE, "Claude Code"),
    (AGENT_CODEX, "Codex"),
    (AGENT_ZED, "Zed Agent"),
    (AGENT_OMP, "OMP"),
    (AGENT_COPILOT, "Copilot"),
    (AGENT_MIMO, "Mimo"),
    (AGENT_REMORA, "Remora"),
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

struct CurrentFileState {
    mtime_secs: u64,
    size: u64,
    file_id: Option<String>,
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
        LogSource {
            root: home.join(".omp/agent/sessions"),
            extra_file: None,
            kind: SourceKind::OmpSession,
        },
        LogSource {
            root: home.join(".local/share/mimocode"),
            extra_file: None,
            kind: SourceKind::MimoSession,
        },
        LogSource {
            root: home.join(".remora/projects"),
            extra_file: None,
            kind: SourceKind::RemoraSession,
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

fn current_file_state(meta: &Metadata) -> CurrentFileState {
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    CurrentFileState {
        mtime_secs,
        size: meta.len(),
        file_id: current_file_id(meta),
    }
}

#[cfg(unix)]
fn current_file_id(meta: &Metadata) -> Option<String> {
    Some(format!("{}:{}", meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn current_file_id(_meta: &Metadata) -> Option<String> {
    None
}

/// 单次会话的 token 用量统计。
pub(crate) struct SessionTokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

impl SessionTokens {
    /// 返回总 token 数（input + output，不含 cache）。
    pub fn total(&self) -> u64 {
        self.input + self.output
    }
}

/// 查找指定 agent 在 `since` 之后修改的最新日志文件并汇总 token 用量。
///
/// 用于 agent 退出后在退出摘要中显示本次会话的 token 消耗。
/// 如果找不到匹配的日志文件或解析失败，返回 `None`（静默降级）。
pub(crate) fn count_recent_session_tokens(
    agent_id: &str,
    since: std::time::SystemTime,
) -> Option<SessionTokens> {
    let since_secs = since
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let source = log_sources().into_iter().find(|s| match agent_id {
        "claude" => matches!(s.kind, SourceKind::Claude),
        "codex" | "Codex.app" => {
            matches!(s.kind, SourceKind::CodexLike(a) if a == AGENT_CODEX)
        }
        "copilot" => {
            matches!(s.kind, SourceKind::Copilot(a) if a == AGENT_COPILOT)
        }
        _ => false,
    })?;

    // 收集日志文件，筛选 session 开始后修改的
    let mut files = collect_jsonl_files(&source.root);
    if let Some(ref extra) = source.extra_file {
        files.push(extra.clone());
    }

    let mut recent: Vec<_> = files
        .into_iter()
        .filter_map(|path| {
            let meta = fs::metadata(&path).ok()?;
            let mtime_secs = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if mtime_secs >= since_secs {
                Some((path, mtime_secs))
            } else {
                None
            }
        })
        .collect();

    if recent.is_empty() {
        return None;
    }

    // 按 mtime 降序排列，取最新的文件
    recent.sort_by(|a, b| b.1.cmp(&a.1));
    let newest_path = &recent[0].0;

    // 解析日志文件
    let result = parse_file(newest_path, source.kind).ok()?;

    let mut tokens = SessionTokens {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_creation: 0,
    };
    for entry in &result.entries {
        tokens.input += entry.input_tokens;
        tokens.output += entry.output_tokens;
        tokens.cache_read += entry.cache_read_input_tokens;
        tokens.cache_creation += entry.cache_creation_input_tokens;
    }

    Some(tokens)
}

/// 将 token 数格式化为紧凑的人类可读表示。
///
/// - `0` → `"0"`
/// - `1234` → `"1.2k"`
/// - `123456` → `"123k"`
/// - `3123000` → `"3m123k"`
pub(crate) fn format_tokens_compact(n: u64) -> String {
    if n == 0 {
        return "0".into();
    }
    if n < 1_000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        let k = n / 1_000;
        let rem = (n % 1_000) / 100;
        if k >= 10 || rem == 0 {
            format!("{k}k")
        } else {
            format!("{k}.{rem}k")
        }
    } else {
        let m = n / 1_000_000;
        let k = (n % 1_000_000) / 1_000;
        if k == 0 {
            format!("{m}m")
        } else {
            format!("{m}m{k}k")
        }
    }
}

pub fn run_stats(config: StatsOutputConfig) -> Result<()> {
    let today = date::today_date_string()?;

    let db_path = db::db_path()?;
    let conn = db::open_db(&db_path)?;
    db::init_schema(&conn)?;

    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut active_source_roots: Vec<PathBuf> = Vec::new();
    let mut active_extra_files: Vec<PathBuf> = Vec::new();

    for source in log_sources() {
        active_source_roots.push(source.root.clone());
        if let Some(ref extra) = source.extra_file {
            active_extra_files.push(extra.clone());
        }

        let mut files = if matches!(source.kind, SourceKind::MimoSession) {
            let db_path = source.root.join("mimocode.db");
            if db_path.is_file() {
                vec![db_path]
            } else {
                Vec::new()
            }
        } else if source.root.exists() {
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
            let current = current_file_state(&meta);
            let cached = db::load_scan_state(&conn, &path_key);

            if cached.as_ref().is_some_and(|state| {
                state.mtime_secs == current.mtime_secs && state.size == current.size
            }) {
                continue;
            }

            if source.kind.supports_append_scan() {
                if let Some(state) = cached.as_ref() {
                    let can_append = state.file_id == current.file_id
                        && current.size >= state.size
                        && current.size >= state.parsed_upto_bytes;
                    if can_append && current.size > state.parsed_upto_bytes {
                        let parsed = match parse_file_from_offset(
                            &path,
                            source.kind,
                            state.parsed_upto_bytes,
                        ) {
                            Ok(parsed) => parsed,
                            Err(e) => {
                                eprintln!("cx: 解析日志失败 ({}): {e:#}", path.display());
                                continue;
                            }
                        };
                        let new_state = db::ScanState {
                            mtime_secs: current.mtime_secs,
                            size: current.size,
                            parsed_upto_bytes: state
                                .parsed_upto_bytes
                                .saturating_add(parsed.consumed_bytes),
                            file_id: current.file_id.clone(),
                        };
                        db::append_file_messages(&conn, &parsed.entries, &path_key, &new_state)?;
                        continue;
                    }
                }
            }

            let parsed = match parse_file(&path, source.kind) {
                Ok(parsed) => parsed,
                Err(e) => {
                    eprintln!("cx: 解析日志失败 ({}): {e:#}", path.display());
                    continue;
                }
            };
            let new_state = db::ScanState {
                mtime_secs: current.mtime_secs,
                size: current.size,
                parsed_upto_bytes: parsed.consumed_bytes,
                file_id: current.file_id.clone(),
            };
            db::replace_file_messages(&conn, &parsed.entries, &path_key, &new_state)?;
        }
    }

    let roots_refs: Vec<&Path> = active_source_roots.iter().map(|p| p.as_path()).collect();
    let extras_refs: Vec<&Path> = active_extra_files.iter().map(|p| p.as_path()).collect();
    if let Err(e) = db::cleanup_stale_entries(&conn, &roots_refs, &extras_refs) {
        eprintln!("cx: 清理过期缓存失败: {e:#}");
    }

    let records = db::load_aggregated(&conn)?;

    if std::env::var("CX_STATS_DUMP").ok().as_deref() == Some("1") {
        return dump_records(&records, &today);
    }

    let period = config
        .period
        .map(|p| p.to_period())
        .unwrap_or(types::Period::Last7);

    match config.output_format {
        None => tui::run_tui(records, today),
        Some(format) => {
            let svg_content = svg::render_to_string(&records, &today, period, config.view)?;
            match format {
                OutputFormat::Svg => {
                    println!("{svg_content}");
                    Ok(())
                }
                OutputFormat::Png | OutputFormat::Jpg => {
                    #[cfg(feature = "image-output")]
                    {
                        let ext = format.extension();
                        let filename = format!("cx-stats.{ext}");
                        let path = std::path::PathBuf::from(&filename);
                        image::render_to_image(&svg_content, format, &path)?;
                        println!("Written to {}", path.display());
                        Ok(())
                    }
                    #[cfg(not(feature = "image-output"))]
                    {
                        Err(anyhow!(
                            "PNG/JPG output requires the `image-output` feature. \
                             Rebuild with: cargo build --features image-output"
                        ))?
                    }
                }
            }
        }
    }
}

/// 将模型名称归一化为统一的命名风格。
///
/// Anthropic 的 Claude 模型存在两种版本号分隔风格：
/// - 点号风格（如 API 返回）：`claude-opus-4.7`
/// - 连字符风格（如 Anthropic 官方文档）：`claude-opus-4-7`
///
/// 在统计中这两种写法会被视为两个不同模型，导致聚合拆分。
/// 此函数将 `claude-*` 模型名中版本号部分的点号替换为连字符，
/// 使之统一为 `claude-opus-4-7` 风格。非 Claude 模型保持原样（如 `gpt-5.4`）。
pub(crate) fn normalize_model_name(model: &str) -> String {
    // 先剥离 provider 前缀（取最后一个 '/' 之后的部分）
    let base_model = model
        .rsplit_once('/')
        .and_then(|(_, suffix)| (!suffix.is_empty()).then_some(suffix))
        .unwrap_or(model);

    // 剥离尾部上下文窗口变体后缀，如 glm-5.2[1m] / gpt-4o[3m]
    // 这类后缀只是同模型的上下文长度变体，统计与展示上应与原模型归并
    let base_model = strip_context_variant_suffix(base_model);

    // 仅对 claude- 前缀的模型做归一化：版本号中的 "." → "-"
    if base_model.starts_with("claude-") {
        let rest = &base_model[7..]; // "opus-4.7" 或 "sonnet-4-20250514" 等
        let mut result = String::with_capacity(base_model.len());
        result.push_str("claude-");
        let mut seen_first_hyphen = false;
        for ch in rest.chars() {
            if ch == '-' && !seen_first_hyphen {
                seen_first_hyphen = true;
                result.push(ch);
            } else if ch == '.' && seen_first_hyphen {
                result.push('-');
            } else {
                result.push(ch);
            }
        }
        result
    } else {
        base_model.to_string()
    }
}

/// 剥离尾部上下文窗口变体后缀，如 `[1m]` / `[3m]`。
/// 正则与 probe 模块的 `resolve_api_model_id` 相同（`\[\d+m\]$`，大小写不敏感），
/// 但当后缀是字符串全部内容时（如 `[1m]`）保留原样，避免产生空模型名。
fn strip_context_variant_suffix(model: &str) -> &str {
    use regex::Regex;
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)\[\d+m\]$").unwrap());
    match re.find(model) {
        Some(m) if m.start() > 0 => &model[..m.start()],
        _ => model,
    }
}

fn dump_records(records: &[UsageRecord], today: &str) -> Result<()> {
    use std::collections::{BTreeMap, BTreeSet};
    use types::UsageTotals;

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

    #[test]
    fn normalize_claude_model_dot_to_hyphen() {
        assert_eq!(normalize_model_name("claude-opus-4.7"), "claude-opus-4-7");
        assert_eq!(
            normalize_model_name("claude-sonnet-4.5"),
            "claude-sonnet-4-5"
        );
        assert_eq!(normalize_model_name("claude-haiku-4.5"), "claude-haiku-4-5");
    }
    #[test]
    fn normalize_claude_model_already_hyphen() {
        assert_eq!(normalize_model_name("claude-opus-4-7"), "claude-opus-4-7");
        assert_eq!(
            normalize_model_name("claude-sonnet-4-20250514"),
            "claude-sonnet-4-20250514"
        );
    }
    #[test]
    fn normalize_non_claude_model_unchanged() {
        assert_eq!(normalize_model_name("gpt-5.4"), "gpt-5.4");
        assert_eq!(normalize_model_name("qwen3.6-plus"), "qwen3.6-plus");
        assert_eq!(normalize_model_name("mimo-v2.5-pro"), "mimo-v2.5-pro");
    }

    #[test]
    fn normalize_model_name_strips_provider_prefix() {
        assert_eq!(normalize_model_name("MiniMax/MiniMax-M2.7"), "MiniMax-M2.7");
        assert_eq!(
            normalize_model_name("Anthropic/claude-opus-4.7"),
            "claude-opus-4-7"
        );
    }

    #[test]
    fn normalize_model_name_strips_context_variant_suffix() {
        // 上下文窗口变体后缀应当被剥离，与原模型归并
        assert_eq!(normalize_model_name("glm-5.2[1m]"), "glm-5.2");
        assert_eq!(normalize_model_name("gpt-4o[3m]"), "gpt-4o");
        assert_eq!(normalize_model_name("gpt-5.4[1m]"), "gpt-5.4");
        // 大小写不敏感
        assert_eq!(normalize_model_name("glm-5.2[1M]"), "glm-5.2");
    }

    #[test]
    fn normalize_model_name_strips_variant_after_provider_prefix() {
        assert_eq!(normalize_model_name("OpenAI/gpt-4o[1m]"), "gpt-4o");
        assert_eq!(
            normalize_model_name("Anthropic/claude-opus-4.7[1m]"),
            "claude-opus-4-7"
        );
    }

    #[test]
    fn normalize_model_name_keeps_unrelated_brackets() {
        // 非变体后缀的括号不应被剥离
        assert_eq!(normalize_model_name("model[1mm]"), "model[1mm]");
        assert_eq!(normalize_model_name("[1m]"), "[1m]");
        assert_eq!(normalize_model_name("model"), "model");
        // 空串不应崩溃
        assert_eq!(normalize_model_name(""), "");
    }
}
