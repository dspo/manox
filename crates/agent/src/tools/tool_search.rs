//! ToolSearch: BM25-lite tool discovery.
//!
//! When `context_optimization.tool_discovery` is `On`, the first turn of a
//! thread sends only [`CORE_TOOLS`] in the schema. `Shadow` computes the same
//! schema-savings metric without changing the provider request.
//!
//! Activation is scoped per-thread via a global activation ledger keyed by
//! thread id. ToolSearch.run() activates discovered tools by name; the turn
//! loop auto-activates any non-core tool the model successfully uses (so
//! auto-activation catches tools the model learns about outside of ToolSearch,
//! e.g. from system prompt references or skill manifests).
//!
//! Core tools are always present; discovered tools are capped at 24 total to
//! bound schema token growth. The catalog is populated at registry-build time
//! and searched with a simple BM25 scorer over English and Chinese descriptions.

use std::sync::{Arc, Mutex};

use gpui::{App, Task};
use schemars::JsonSchema;
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;

const NAME: &str = "ToolSearch";

/// Core tools always present in the schema. Everything else is discoverable.
/// These are the essential tools every turn needs; the rest are lazily
/// activated via ToolSearch or implicit first-use activation.
pub const CORE_TOOLS: &[&str] = &[
    crate::tools::READ,
    crate::tools::BASH,
    crate::tools::EDIT,
    crate::tools::WRITE,
    crate::tools::GREP,
    crate::tools::GLOB,
    crate::tools::ASK_USER_QUESTION,
    crate::tools::UPDATE_PLAN,
    NAME,
];

/// Max activated tools (non-core). Once this cap is hit, the model cannot
/// discover additional tools — it must work with what's already available.
const MAX_ACTIVATED: usize = 24;

// ─── per-thread activation ledger ───────────────────────────────────────

/// Global activation state: thread_id → activated names in first-use order.
static ACTIVATIONS: Mutex<Option<ActivationLedger>> = Mutex::new(None);
static SEARCH_STATS: Mutex<Option<std::collections::HashMap<String, ToolSearchStats>>> =
    Mutex::new(None);

type ActivationLedger = std::collections::HashMap<String, Vec<String>>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolSearchStats {
    pub queries: u64,
    pub hits: u64,
    pub last_query: String,
    pub last_hits: Vec<String>,
}

/// Activate one or more tools for a given thread. Non-core tools only (core
/// tools are always present and don't need activation). Respects the
/// [`MAX_ACTIVATED`] cap.
pub fn activate_tools(thread_id: &str, tool_names: &[String]) {
    let mut guard = ACTIVATIONS.lock().unwrap();
    let ledger = guard.get_or_insert_with(std::collections::HashMap::new);
    let active = ledger.entry(thread_id.to_string()).or_default();
    for name in tool_names {
        if CORE_TOOLS.contains(&name.as_str()) || conditional_core(name) {
            continue;
        }
        if active.len() >= MAX_ACTIVATED {
            break;
        }
        if !active.contains(name) {
            active.push(name.clone());
        }
    }
}

fn conditional_core(name: &str) -> bool {
    (name == crate::tools::SKILL && !crate::skill::global().list().is_empty())
        || (name == crate::tools::CODE
            && matches!(
                crate::settings::context_optimization().code_mode,
                crate::settings::CodeModeToggle::Hybrid
            ))
}

/// Core schemas followed by conditional Skill and then discovered schemas in
/// first-activation order. Existing entries never move between turns.
pub fn schema_order(thread_id: &str) -> Vec<String> {
    let mut order: Vec<String> = CORE_TOOLS.iter().map(|name| (*name).to_string()).collect();
    if matches!(
        crate::settings::context_optimization().code_mode,
        crate::settings::CodeModeToggle::Hybrid
    ) {
        order.push(crate::tools::CODE.to_string());
    }
    if !crate::skill::global().list().is_empty() {
        order.push(crate::tools::SKILL.to_string());
    }
    for name in activated_for(thread_id) {
        if !order.contains(&name) {
            order.push(name);
        }
    }
    order
}

/// Return the set of activated tool names for `thread_id`, excluding core
/// tools (which are always present).
pub fn activated_for(thread_id: &str) -> Vec<String> {
    let guard = ACTIVATIONS.lock().unwrap();
    let Some(ledger) = guard.as_ref() else {
        return Vec::new();
    };
    ledger.get(thread_id).cloned().unwrap_or_default()
}

pub fn search_stats_for(thread_id: &str) -> ToolSearchStats {
    SEARCH_STATS
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|stats| stats.get(thread_id))
        .cloned()
        .unwrap_or_default()
}

fn record_search(thread_id: &str, query: &str, hits: &[String]) {
    let mut guard = SEARCH_STATS.lock().unwrap();
    let stats = guard
        .get_or_insert_with(std::collections::HashMap::new)
        .entry(thread_id.to_string())
        .or_default();
    stats.queries = stats.queries.saturating_add(1);
    stats.hits = stats.hits.saturating_add(hits.len() as u64);
    stats.last_query = query.to_string();
    stats.last_hits = hits.to_vec();
}

/// Drop activation state for a thread when it shuts down. Idempotent.
pub fn drop_activations(thread_id: &str) {
    let mut guard = ACTIVATIONS.lock().unwrap();
    if let Some(ledger) = guard.as_mut() {
        ledger.remove(thread_id);
    }
    if let Some(stats) = SEARCH_STATS.lock().unwrap().as_mut() {
        stats.remove(thread_id);
    }
}

/// Whether tool discovery is actively filtering the schema. True only when
/// the settings toggle is `On`. `Shadow` collects projection metrics but does
/// not alter the model-facing tool set, preserving the rollout safety
/// contract.
pub fn is_active() -> bool {
    matches!(
        crate::settings::context_optimization().tool_discovery,
        crate::settings::Toggle::On
    )
}

// ─── catalog ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolEntry {
    pub name: String,
    pub description: String,
    read_only: bool,
    search_text: String,
}

/// Register ToolSearch with a catalog snapshot of this exact registry. This
/// avoids a process-global first-registry-wins bug for MCP/LSP/plugin tools.
pub fn register(registry: &mut crate::tool::ToolRegistry) {
    if registry.get(NAME).is_some() {
        return;
    }
    let catalog = registry
        .iter()
        .map(|tool| {
            let en = crate::tools::descriptions::description_for(
                tool.name(),
                crate::language::Language::En,
            )
            .unwrap_or_else(|| tool.description());
            let zh = crate::tools::descriptions::description_for(
                tool.name(),
                crate::language::Language::ZhCn,
            )
            .unwrap_or("");
            ToolEntry {
                name: tool.name().to_string(),
                description: en.to_string(),
                read_only: tool.is_read_only(),
                search_text: format!("{} {en} {zh}", tool.name()),
            }
        })
        .collect();
    registry.register(Arc::new(ToolSearchTool {
        catalog: Arc::new(catalog),
    }));
}

pub struct ToolSearchTool {
    catalog: Arc<Vec<ToolEntry>>,
}

impl AgentTool for ToolSearchTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "Search for tools by describing what you want to do. Returns matching \
         tool names and their descriptions. Discovered tools become available \
         in subsequent turns."
    }

    fn input_schema(&self) -> serde_json::Value {
        crate::tools::schema::<ToolSearchInput>()
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        context: &dyn crate::tool::ToolContext,
        _cx: &mut App,
    ) -> Task<Result<String, String>> {
        let query: ToolSearchInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Task::ready(Err(format!("invalid input: {e}"))),
        };
        let mut results = search(&query.query, &self.catalog);
        filter_for_plan_mode(&mut results, context.plan_mode());
        let thread_id = context.thread_id().to_string();

        // Activate discovered tools so they appear in the schema on the next
        // turn. The model sees the ToolSearch result text (tool names +
        // descriptions) this turn, then can call them next turn when the
        // schema includes them.
        let discovered: Vec<String> = results.iter().map(|r| r.name.clone()).collect();
        record_search(&thread_id, &query.query, &discovered);
        if !discovered.is_empty() {
            activate_tools(&thread_id, &discovered);
        }

        let out = if results.is_empty() {
            "No matching tools found. Try a broader query or describe the \
             capability you need."
                .into()
        } else {
            let mut s = String::from("Matching tools (available next turn):\n\n");
            for r in &results {
                s.push_str(&format!("- **{}**: {}\n", r.name, r.description));
            }
            s
        };
        Task::ready(Ok(out))
    }
}

fn filter_for_plan_mode(results: &mut Vec<ToolEntry>, plan_mode: bool) {
    if plan_mode {
        results.retain(|tool| tool.read_only);
    }
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
struct ToolSearchInput {
    /// Natural-language description of what you want to do (e.g. "search the
    /// web", "run a terminal command in the background").
    query: String,
}

// ─── BM25 search ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct CatalogDoc {
    name: String,
    description: String,
    read_only: bool,
    term_count: usize,
    tf: std::collections::HashMap<String, usize>,
}

impl CatalogDoc {
    fn term_freq(&self, token: &str) -> usize {
        self.tf.get(token).copied().unwrap_or(0)
    }
}

fn search(query: &str, catalog: &[ToolEntry]) -> Vec<ToolEntry> {
    let tokens = tokenize(query);
    if tokens.is_empty() {
        return Vec::new();
    }
    let docs: Vec<CatalogDoc> = catalog
        .iter()
        .filter(|e| !CORE_TOOLS.contains(&e.name.as_str()))
        .map(|e| {
            let doc_tokens = tokenize(&e.search_text);
            let term_count = doc_tokens.len();
            let mut tf: std::collections::HashMap<String, usize> = Default::default();
            for t in &doc_tokens {
                *tf.entry(t.clone()).or_default() += 1;
            }
            CatalogDoc {
                name: e.name.clone(),
                description: e.description.clone(),
                read_only: e.read_only,
                term_count,
                tf,
            }
        })
        .collect();

    let n_docs = docs.len() as f64;
    let avg_dl = docs.iter().map(|d| d.term_count as f64).sum::<f64>() / n_docs.max(1.0);
    let k1 = 1.2;
    let b = 0.75;

    let mut scored: Vec<(f64, &CatalogDoc)> = docs
        .iter()
        .map(|doc| {
            let dl = doc.term_count as f64;
            let mut score = 0.0;
            for t in &tokens {
                let tf = doc.term_freq(t) as f64;
                if tf == 0.0 {
                    continue;
                }
                let df = docs.iter().filter(|d| d.term_freq(t) > 0).count() as f64;
                let idf = ((n_docs - df + 0.5) / (df + 0.5)).ln_1p();
                let numerator = tf * (k1 + 1.0);
                let denominator = tf + k1 * (1.0 - b + b * dl / avg_dl.max(1.0));
                score += idf * numerator / denominator.max(1e-9);
            }
            (score, doc)
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.retain(|(s, _)| *s > 0.0);
    scored.truncate(10);

    scored
        .into_iter()
        .map(|(_, doc)| ToolEntry {
            name: doc.name.clone(),
            description: doc.description.clone(),
            read_only: doc.read_only,
            search_text: String::new(),
        })
        .collect()
}

fn tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let lower = s.to_lowercase();
    for segment in lower.split(|c: char| !c.is_alphanumeric()) {
        if segment.is_empty() {
            continue;
        }
        let chars: Vec<char> = segment.chars().collect();
        if chars.iter().any(|c| is_cjk(*c)) {
            for width in [1usize, 2] {
                for window in chars.windows(width) {
                    tokens.push(window.iter().collect());
                }
            }
        } else if segment.len() >= 2 {
            tokens.push(segment.to_string());
        }
    }
    tokens
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_splits_and_filters() {
        let tokens = tokenize("Read files from disk");
        assert!(tokens.contains(&"read".into()));
        assert!(tokens.contains(&"files".into()));
    }

    #[test]
    fn empty_query_returns_nothing() {
        let results = search("", &[]);
        assert!(results.is_empty());
    }

    #[test]
    fn search_does_not_panic_without_catalog() {
        let results = search("read file", &[]);
        let _ = results;
    }

    #[test]
    fn core_tool_not_in_search_results() {
        let catalog = vec![
            ToolEntry {
                name: "Read".into(),
                description: "Read a file".into(),
                read_only: true,
                search_text: "Read Read a file 读取文件".into(),
            },
            ToolEntry {
                name: "WebFetch".into(),
                description: "Fetch a URL".into(),
                read_only: true,
                search_text: "WebFetch Fetch a URL 获取网页".into(),
            },
        ];
        let results = search("read file", &catalog);
        // "Read" is a core tool — should not appear in results.
        assert!(
            !results.iter().any(|r| r.name == "Read"),
            "core tools excluded from search"
        );
    }

    #[test]
    fn chinese_query_matches_bilingual_catalog() {
        let catalog = vec![ToolEntry {
            name: "WebFetch".into(),
            description: "Fetch a URL".into(),
            read_only: true,
            search_text: "WebFetch Fetch a URL 获取网页内容".into(),
        }];
        let results = search("获取网页", &catalog);
        assert_eq!(results.first().map(|r| r.name.as_str()), Some("WebFetch"));
    }

    #[test]
    fn plan_mode_filters_write_tools_before_activation() {
        let mut results = vec![
            ToolEntry {
                name: "WebFetch".into(),
                description: "Fetch a URL".into(),
                read_only: true,
                search_text: "WebFetch Fetch a URL".into(),
            },
            ToolEntry {
                name: "Write".into(),
                description: "Write a file".into(),
                read_only: false,
                search_text: "Write Write a file".into(),
            },
        ];

        filter_for_plan_mode(&mut results, true);

        assert_eq!(
            results
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["WebFetch"]
        );
    }

    #[test]
    fn activation_preserves_first_seen_order() {
        let tid = "test_order";
        activate_tools(tid, &["WebFetch".into(), "Agent".into(), "List".into()]);
        assert_eq!(activated_for(tid), ["WebFetch", "Agent", "List"]);
        activate_tools(tid, &["Agent".into(), "Monitor".into()]);
        assert_eq!(activated_for(tid), ["WebFetch", "Agent", "List", "Monitor"]);
        drop_activations(tid);
    }

    #[test]
    fn search_stats_record_queries_hits_and_last_result() {
        let tid = "test_search_stats";
        record_search(tid, "web", &["WebFetch".into(), "BrowserOpen".into()]);
        record_search(tid, "agent", &["Agent".into()]);
        assert_eq!(
            search_stats_for(tid),
            ToolSearchStats {
                queries: 2,
                hits: 3,
                last_query: "agent".into(),
                last_hits: vec!["Agent".into()],
            }
        );
        drop_activations(tid);
        assert_eq!(search_stats_for(tid), ToolSearchStats::default());
    }

    #[test]
    fn activation_capped_at_max() {
        // Activate more than MAX_ACTIVATED and ensure cap holds.
        let tid = "test_cap";
        let many: Vec<String> = (0..50).map(|i| format!("Tool_{i}")).collect();
        activate_tools(tid, &many);
        let active = activated_for(tid);
        assert!(
            active.len() <= MAX_ACTIVATED,
            "activation cap: {} <= {MAX_ACTIVATED}",
            active.len()
        );
        drop_activations(tid);
    }

    #[test]
    fn core_tools_not_in_activation_set() {
        let tid = "test_core";
        activate_tools(tid, &["Read".into(), "Bash".into(), "WebFetch".into()]);
        let active = activated_for(tid);
        assert!(
            !active.contains(&"Read".to_string()),
            "core tool not activated"
        );
        assert!(
            !active.contains(&"Bash".to_string()),
            "core tool not activated"
        );
        assert!(
            active.contains(&"WebFetch".to_string()),
            "non-core tool activated"
        );
        drop_activations(tid);
    }
}
