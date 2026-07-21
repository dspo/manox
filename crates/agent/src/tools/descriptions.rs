//! Bilingual tool descriptions + schema field descriptions.
//!
//! The model-facing prose for every built-in tool — both the top-level
//! `description` and each `description` node inside the schemars-generated
//! `input_schema` — is sourced from two symmetric JSON assets, `en.json` and
//! `zh-CN.json`, selected by the thread's immutable agent language. Structure
//! (field names, types, `required`, enum constraints) stays single-sourced in
//! the `*Input` structs + `schemars`; only the prose is bilingualized here.
//!
//! Key shape: a top-level tool description is keyed by the tool's wire name
//! (e.g. `Read`). A schema-field description is keyed by `<tool>` + the JSON
//! Pointer to the object containing the `description` (e.g.
//! `Read/properties/path`, `AskUserQuestion/properties/questions/items/properties/question`).
//! [`override_schema`] walks the schemars tree and replaces each `description`
//! node it finds with the asset value for that pointer; a missing key leaves
//! the schemars-generated description in place (permissive), so tools whose
//! descriptions are not yet bilingualized (web_explore / team / LSP / MCP, or
//! the template-rendered `Agent` tool) keep their English prose without
//! parity-test churn.
//!
//! Coverage is asserted at startup: the two language files must carry the same
//! key set, and every key must correspond to a real description pointer in the
//! tool it names — so a typo'd key or a field added to an `*Input` struct
//! without a matching asset entry surfaces immediately.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use serde_json::Value;

use crate::language::Language;

const EN_JSON: &str = include_str!("descriptions/en.json");
const ZH_CN_JSON: &str = include_str!("descriptions/zh-CN.json");

fn parse(raw: &str) -> BTreeMap<String, String> {
    serde_json::from_str(raw).expect("tool descriptions JSON parses")
}

static TABLE_EN: OnceLock<BTreeMap<String, String>> = OnceLock::new();
static TABLE_ZH_CN: OnceLock<BTreeMap<String, String>> = OnceLock::new();

/// The full `<key, prose>` map for `lang`. Built once per language from the
/// embedded JSON; `&'static` via `OnceLock` so lookups return `&'static str`.
pub fn table(lang: Language) -> &'static BTreeMap<String, String> {
    match lang {
        Language::En => TABLE_EN.get_or_init(|| parse(EN_JSON)),
        Language::ZhCn => TABLE_ZH_CN.get_or_init(|| parse(ZH_CN_JSON)),
    }
}

/// Top-level tool description for `tool` in `lang`, or `None` if the tool has
/// no asset entry (template-rendered tools like `Agent`, or not-yet-bilingualized
/// tools — the caller falls back to the tool's own `description()`).
pub fn description_for(tool: &str, lang: Language) -> Option<&'static str> {
    table(lang).get(tool).map(|s| s.as_str())
}

/// Replace every `description` node in `schema` with the asset value for
/// `<tool><json-pointer-to-the-containing-object>`. Nodes without an asset
/// entry keep their schemars-generated value. The walk is recursive over
/// objects and arrays; the pointer is accumulated as the tree is descended so
/// each `description` is anchored to its exact location.
pub fn override_schema(mut schema: Value, tool: &str, lang: Language) -> Value {
    let table = table(lang);
    override_inner(&mut schema, "", tool, table);
    schema
}

fn override_inner(value: &mut Value, pointer: &str, tool: &str, table: &BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            // The pointer to THIS object is `pointer`; a `description` field on
            // it lives at `<pointer>/description`, keyed as `<tool><pointer>`.
            let key = format!("{tool}{pointer}");
            if let Some(desc) = table.get(&key)
                && let Some(existing) = map.get_mut("description")
                && existing.is_string()
            {
                *existing = Value::String(desc.clone());
            }
            // Descend into every child, extending the pointer by `/<key>`.
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                let child_ptr = format!("{pointer}/{k}");
                if let Some(child) = map.get_mut(&k) {
                    override_inner(child, &child_ptr, tool, table);
                }
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                let child_ptr = format!("{pointer}/{i}");
                override_inner(item, &child_ptr, tool, table);
            }
        }
        _ => {}
    }
}

/// Collect every `<tool><pointer>` key that corresponds to a `description`
/// node in `schema` — the canonical coverage set for `tool`. Used by the
/// parity test to assert the asset has exactly these keys (no missing, no extra).
pub fn collect_description_pointers(schema: &Value, tool: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    collect_inner(schema, "", tool, &mut out);
    out
}

fn collect_inner(value: &Value, pointer: &str, tool: &str, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if map
                .get("description")
                .map(Value::is_string)
                .unwrap_or(false)
            {
                out.insert(format!("{tool}{pointer}"));
            }
            for (k, child) in map {
                let child_ptr = format!("{pointer}/{k}");
                collect_inner(child, &child_ptr, tool, out);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let child_ptr = format!("{pointer}/{i}");
                collect_inner(item, &child_ptr, tool, out);
            }
        }
        _ => {}
    }
}

/// The 15 built-in tools whose descriptions + schema field descriptions are
/// bilingualized here. Excludes `Agent` (its description is template-rendered
/// and already bilingual via `tools/agent_tool.tera.md`) and the dynamic
/// web_explore / team / LSP / MCP tool sets (disclosed partial coverage; the
/// permissive `override_schema` fallback keeps their schemars English prose).
pub const BILINGUAL_TOOLS: &[(&str, &str)] = &[
    ("Read", "read_file::ReadFileInput"),
    ("Write", "write_file::WriteFileInput"),
    ("Edit", "edit_file::EditFileInput"),
    ("Bash", "bash::BashInput"),
    ("Grep", "grep::GrepInput"),
    ("Glob", "glob::GlobInput"),
    ("List", "list_directory::ListDirectoryInput"),
    ("WebFetch", "web_fetch::WebFetchInput"),
    ("SelfInfo", "self_info::SelfInfoInput"),
    ("Skill", "skill::SkillInput"),
    ("Monitor", "monitor::MonitorInput"),
    ("EnterWorktree", "worktree::EnterWorktreeInput"),
    ("ExitWorktree", "worktree::ExitWorktreeInput"),
    ("BashOutput", "bash_output::BashOutputInput"),
    ("AskUserQuestion", "ask_user::AskUserQuestionInput"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn en_and_zh_keys_are_symmetric() {
        let en: BTreeSet<&str> = table(Language::En).keys().map(|s| s.as_str()).collect();
        let zh: BTreeSet<&str> = table(Language::ZhCn).keys().map(|s| s.as_str()).collect();
        let en_only: Vec<_> = en.difference(&zh).collect();
        let zh_only: Vec<_> = zh.difference(&en).collect();
        assert!(en_only.is_empty(), "keys only in en.json: {en_only:?}");
        assert!(zh_only.is_empty(), "keys only in zh-CN.json: {zh_only:?}");
    }

    #[test]
    fn asset_keys_match_real_schema_pointers() {
        // For each bilingual tool, the set of `description` pointers in its
        // schemars schema must equal the set of asset keys prefixed with the
        // tool name — no missing coverage, no orphan keys.
        for (tool, _input) in BILINGUAL_TOOLS {
            let schema = super::super::schema_for_tool(tool);
            let pointers = collect_description_pointers(&schema, tool);
            let en_keys: BTreeSet<String> = table(Language::En)
                .keys()
                .filter(|k| k.starts_with(&format!("{tool}/")))
                .cloned()
                .collect();
            let missing: Vec<_> = pointers.difference(&en_keys).collect();
            let extra: Vec<_> = en_keys.difference(&pointers).collect();
            assert!(
                missing.is_empty(),
                "{tool}: schema pointers missing from asset: {missing:?}"
            );
            assert!(
                extra.is_empty(),
                "{tool}: asset keys with no schema pointer: {extra:?}"
            );
            // Each tool also has a top-level key (the description() prose).
            assert!(
                table(Language::En).contains_key(*tool),
                "{tool}: top-level description key missing from en.json"
            );
            assert!(
                table(Language::ZhCn).contains_key(*tool),
                "{tool}: top-level description key missing from zh-CN.json"
            );
        }
    }

    /// Walk `value` and collect `(pointer, description)` pairs — the pointer
    /// is the JSON Pointer to the object carrying each `description` node, the
    /// same shape `override_schema` keys on. Used to prove the override walk is
    /// total: after a zh-CN override, every description node must hold a value
    /// sourced from the zh asset (no node left at its schemars English fallback).
    fn collect_descriptions(value: &Value, pointer: &str, out: &mut Vec<(String, String)>) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(desc)) = map.get("description") {
                    out.push((pointer.to_string(), desc.clone()));
                }
                for (k, child) in map {
                    let child_ptr = format!("{pointer}/{k}");
                    collect_descriptions(child, &child_ptr, out);
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    let child_ptr = format!("{pointer}/{i}");
                    collect_descriptions(item, &child_ptr, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn override_schema_replaces_every_present_description() {
        // The zh-CN override must differ from the English one for every
        // bilingualized tool whose schema carries field descriptions — a
        // translation that accidentally reverts to the English value (en == zh
        // for some field) would silently pass the pointer-parity test while
        // shipping English to Chinese threads. Tools with no field descriptions
        // (e.g. SelfInfo, whose prose lives entirely in the top-level
        // description handled by `description_for`) legitimately override to a
        // no-op, so they are skipped here.
        for (tool, _input) in BILINGUAL_TOOLS {
            let schema = super::super::schema_for_tool(tool);
            if collect_description_pointers(&schema, tool).is_empty() {
                continue;
            }
            let en = override_schema(schema.clone(), tool, Language::En);
            let zh = override_schema(schema, tool, Language::ZhCn);
            assert_ne!(en, zh, "{tool}: zh-CN override produced no change vs en");
        }

        // Totality: every description node in the zh-overridden schema must hold
        // the value the zh asset assigns to that pointer — the walk may not skip
        // any node or leave a schemars fallback in place.
        let tool = "Read";
        let schema = super::super::schema_for_tool(tool);
        let zh = override_schema(schema, tool, Language::ZhCn);
        let table = table(Language::ZhCn);
        let mut nodes = Vec::new();
        collect_descriptions(&zh, "", &mut nodes);
        assert!(
            !nodes.is_empty(),
            "Read schema carries no description nodes"
        );
        for (pointer, desc) in &nodes {
            let expected = table
                .get(&format!("{tool}{pointer}"))
                .expect("zh asset key for an existing description pointer");
            assert_eq!(
                desc, expected,
                "Read:{pointer}: zh override did not apply the asset value"
            );
        }
    }
}
