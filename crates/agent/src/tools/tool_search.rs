//! ToolSearch: BM25-lite tool discovery.
//!
//! When `context_optimization.tool_discovery` is not `Off`, the first turn of a
//! thread sends only [`CORE_TOOLS`] in the schema. The model uses ToolSearch to
//! discover additional tools, which are activated for subsequent turns.
//!
//! Activation is scoped per-thread via a global activation ledger keyed by
//! thread id. ToolSearch.run() activates discovered tools by name; the turn
//! loop auto-activates any non-core tool the model successfully uses (so
//! auto-activation catches tools the model learns about outside of ToolSearch,
//! e.g. from system prompt references or skill manifests).
//!
//! Core tools are always present; discovered tools are capped at 24 total to
//! bound schema token growth. The catalog is populated at registry-build time
//! and searched with a simple BM25 scorer. Only English descriptions are
//! indexed (the retrieval corpus is monolingual).

use std::collections::HashSet;
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

/// Global activation state: thread_id → set of activated (non-core) tool names.
static ACTIVATIONS: Mutex<Option<ActivationLedger>> = Mutex::new(None);

type ActivationLedger = std::collections::HashMap<String, HashSet<String>>;

/// Activate one or more tools for a given thread. Non-core tools only (core
/// tools are always present and don't need activation). Respects the
/// [`MAX_ACTIVATED`] cap.
pub fn activate_tools(thread_id: &str, tool_names: &[String]) {
    let mut guard = ACTIVATIONS.lock().unwrap();
    let ledger = guard.get_or_insert_with(std::collections::HashMap::new);
    let set = ledger.entry(thread_id.to_string()).or_default();
    for name in tool_names {
        if CORE_TOOLS.contains(&name.as_str()) {
            continue;
        }
        if set.len() >= MAX_ACTIVATED {
            break;
        }
        set.insert(name.clone());
    }
}

/// Return the set of activated tool names for `thread_id`, excluding core
/// tools (which are always present).
pub fn activated_for(thread_id: &str) -> Vec<String> {
    let guard = ACTIVATIONS.lock().unwrap();
    let Some(ledger) = guard.as_ref() else {
        return Vec::new();
    };
    ledger
        .get(thread_id)
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default()
}

/// Drop activation state for a thread when it shuts down. Idempotent.
pub fn drop_activations(thread_id: &str) {
    let mut guard = ACTIVATIONS.lock().unwrap();
    if let Some(ledger) = guard.as_mut() {
        ledger.remove(thread_id);
    }
}

/// Whether tool discovery is active for the current process. True when the
/// settings toggle is not Off.
pub fn is_active() -> bool {
    !matches!(
        crate::settings::context_optimization().tool_discovery,
        crate::settings::Toggle::Off
    )
}

// ─── catalog ───────────────────────────────────────────────────────────

/// Static catalog of discoverable tools. Populated at startup; ToolSearch
/// queries it without needing access to the thread's registry.
static CATALOG: std::sync::OnceLock<Vec<ToolEntry>> = std::sync::OnceLock::new();

#[derive(Debug, Clone)]
pub struct ToolEntry {
    pub name: String,
    pub description: String,
}

/// Seed the catalog from the full set of tool names and descriptions. Called
/// once after the default registry is built.
pub fn seed_catalog(entries: Vec<ToolEntry>) {
    let _ = CATALOG.set(entries);
}

/// Build catalog entries from the English descriptions of every tool in the
/// registry. Call once after `main_registry_with_policy` assembles the full
/// tool set, so the catalog is populated before the first turn.
pub fn seed_catalog_from_registry(registry: &crate::tool::ToolRegistry) {
    let entries: Vec<ToolEntry> = registry
        .iter()
        .map(|t| ToolEntry {
            name: t.name().to_string(),
            description: t.description().to_string(),
        })
        .collect();
    let _ = CATALOG.set(entries);
}

/// Register this tool in the given registry if not already present.
pub fn register(registry: &mut crate::tool::ToolRegistry) {
    if registry.get(NAME).is_some() {
        return;
    }
    registry.register(Arc::new(ToolSearchTool));
}

pub struct ToolSearchTool;

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
        let results = search(&query.query);
        let thread_id = context.thread_id().to_string();

        // Activate discovered tools so they appear in the schema on the next
        // turn. The model sees the ToolSearch result text (tool names +
        // descriptions) this turn, then can call them next turn when the
        // schema includes them.
        let discovered: Vec<String> = results.iter().map(|r| r.name.clone()).collect();
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
    term_count: usize,
    tf: std::collections::HashMap<String, usize>,
}

impl CatalogDoc {
    fn term_freq(&self, token: &str) -> usize {
        self.tf.get(token).copied().unwrap_or(0)
    }
}

fn search(query: &str) -> Vec<ToolEntry> {
    let tokens = tokenize(query);
    if tokens.is_empty() {
        return Vec::new();
    }
    let catalog = CATALOG.get();
    let docs: Vec<CatalogDoc> = match catalog {
        Some(entries) => entries
            .iter()
            .filter(|e| !CORE_TOOLS.contains(&e.name.as_str()))
            .map(|e| {
                let text = format!("{} {}", e.name, e.description);
                let doc_tokens = tokenize(&text);
                let term_count = doc_tokens.len();
                let mut tf: std::collections::HashMap<String, usize> = Default::default();
                for t in &doc_tokens {
                    *tf.entry(t.clone()).or_default() += 1;
                }
                CatalogDoc {
                    name: e.name.clone(),
                    description: e.description.clone(),
                    term_count,
                    tf,
                }
            })
            .collect(),
        None => return Vec::new(),
    };

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
        })
        .collect()
}

fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_string())
        .collect()
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
        let results = search("");
        assert!(results.is_empty());
    }

    #[test]
    fn search_does_not_panic_without_catalog() {
        let results = search("read file");
        let _ = results;
    }

    #[test]
    fn core_tool_not_in_search_results() {
        // Seed a minimal catalog and verify core tools are excluded.
        seed_catalog(vec![
            ToolEntry {
                name: "Read".into(),
                description: "Read a file".into(),
            },
            ToolEntry {
                name: "WebFetch".into(),
                description: "Fetch a URL".into(),
            },
        ]);
        let results = search("read file");
        // "Read" is a core tool — should not appear in results.
        assert!(
            !results.iter().any(|r| r.name == "Read"),
            "core tools excluded from search"
        );
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
