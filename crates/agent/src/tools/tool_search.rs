//! ToolSearch: BM25-lite tool discovery.
//!
//! When `context_optimization.tool_discovery` is not `Off`, this tool lets the
//! model search the full tool catalog by natural-language query. The catalog is
//! populated at registry-build time and searched with a simple BM25 scorer.

use std::sync::{Arc, OnceLock};

use gpui::{App, Task};
use schemars::JsonSchema;
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;

const NAME: &str = "ToolSearch";

/// Core tools always present in the schema; everything else is discoverable.
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

/// Static catalog of discoverable tools. Populated at startup; ToolSearch
/// queries it without needing access to the thread's registry.
static CATALOG: OnceLock<Vec<ToolEntry>> = OnceLock::new();

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
/// registry. Call this once after `main_registry_with_policy` assembles the
/// full tool set, so the catalog is populated before the first turn.
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
         tool names and their descriptions so you can use them in subsequent calls."
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
        _context: &dyn crate::tool::ToolContext,
        _cx: &mut App,
    ) -> Task<Result<String, String>> {
        let query: ToolSearchInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Task::ready(Err(format!("invalid input: {e}"))),
        };
        let results = search(&query.query);
        let out = if results.is_empty() {
            "No matching tools found. Try a broader query or describe the \
             capability you need."
                .into()
        } else {
            let mut s = String::from("Matching tools (you may use these now):\n\n");
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
    query: String,
}

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
        // search handles both unset and set catalog — it must not panic
        // regardless of test ordering.
        let results = search("read file");
        // Catalog may or may not be seeded from other tests; the contract is
        // only that search never panics.
        let _ = results;
    }
}
