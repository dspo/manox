//! JSON Schema -> TypeScript declaration renderer for the script-visible tool
//! surface.
//!
//! Pure functions: a tool's `parameters_schema` becomes the input type of a
//! `declare const tools` member. Property descriptions render as `//` comments;
//! constraints the type system cannot express render as `@integer`-style tags
//! on the same comment. Local references (`#/$defs/...`, `#/definitions/...`)
//! expand inline with cycle detection; recursive and external references
//! render as `unknown`.
//!
//! Each tool's sample (description + declaration) has a token budget
//! (estimated chars/4). When a sample exceeds the budget it is first re-rendered
//! without the constraint tags, and if it is still too large the whole tool is
//! dropped. Across tools the samples share a total budget: samples are admitted
//! in input order (first come, first served) until the pool is spent, and later
//! tools drop whole. The caller learns which tools were dropped from the result
//! and can point the script at `ALL_TOOLS` instead.

use std::collections::HashSet;

use serde_json::Value;

/// A tool as seen by the renderer.
#[derive(Debug, Clone)]
pub struct DeclaredTool {
    pub name: String,
    /// The identifier scripts use (`tools.<js_name>`); illegal identifier
    /// characters become `_`.
    pub js_name: String,
    pub description: String,
    pub parameters_schema: Value,
}

/// One tool's rendered declaration. `declaration` is `None` for tools dropped
/// by the budget.
#[derive(Debug, Clone)]
pub struct RenderedTool {
    pub name: String,
    pub js_name: String,
    pub description: String,
    pub declaration: Option<String>,
}

/// Rendered declarations for a tool list, in input order.
#[derive(Debug, Clone)]
pub struct RenderedDeclarations {
    pub rendered: Vec<RenderedTool>,
    /// Tools dropped entirely by the budget, in input order.
    pub dropped: Vec<String>,
}

/// Per-tool sample budget, in estimated tokens.
pub const DEFAULT_DECLARATION_BUDGET_TOKENS: usize = 3000;
/// Shared budget across every tool's sample combined, in estimated tokens:
/// the declaration listing must stay bounded no matter how wide the tool
/// surface grows (MCP servers add tools and schema bulk linearly).
pub const DEFAULT_TOTAL_DECLARATION_BUDGET_TOKENS: usize = 3000;
/// Largest rendered input type, in characters, before it degrades (`unknown`
/// at the schema level, tag-drop and tool-drop at the budget level).
const SCHEMA_MAX_CHARS: usize = 16_000;
/// Local `$ref` expansions per rendered schema, so shared definitions cannot
/// blow up the output.
const MAX_REF_EXPANSIONS: usize = 32;
/// JSON Schema keywords rendered as `@keyword value` tags.
const TAG_KEYWORDS: [&str; 10] = [
    "minimum",
    "exclusiveMinimum",
    "maximum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minItems",
];
/// Tag keywords whose values all hold at once, so only the binding one matters.
const LOWER_BOUNDS: [&str; 4] = ["minimum", "exclusiveMinimum", "minLength", "minItems"];
const UPPER_BOUNDS: [&str; 4] = ["maximum", "exclusiveMaximum", "maxLength", "maxItems"];
/// Longest single tag; longer ones (large defaults, long patterns) are left out.
const MAX_TAG_CHARS: usize = 80;

/// Render declarations for a tool list with the default budgets.
pub fn render_declarations(tools: &[DeclaredTool]) -> RenderedDeclarations {
    render_declarations_with_budget(tools, DEFAULT_DECLARATION_BUDGET_TOKENS)
}

/// Render declarations for a tool list with the default total budget. Per
/// tool: render with tags, re-render without tags when the sample exceeds the
/// budget, drop the tool when it still does not fit.
pub fn render_declarations_with_budget(
    tools: &[DeclaredTool],
    budget_tokens: usize,
) -> RenderedDeclarations {
    render_declarations_with_total_budget(
        tools,
        budget_tokens,
        DEFAULT_TOTAL_DECLARATION_BUDGET_TOKENS,
    )
}

/// Render declarations for a tool list with explicit per-tool and total
/// budgets. The per-tool budget is the second-level trim (tag drop, then tool
/// drop) for one oversized sample; the total budget is the shared pool across
/// all samples — admitted in input order, first come first served, and a tool
/// whose sample does not fit the remaining pool drops whole (name-only) while
/// earlier tools keep theirs.
pub fn render_declarations_with_total_budget(
    tools: &[DeclaredTool],
    per_tool_budget_tokens: usize,
    total_budget_tokens: usize,
) -> RenderedDeclarations {
    let budget_chars = per_tool_budget_tokens.saturating_mul(crate::types::CHARS_PER_TOKEN);
    let total_chars = total_budget_tokens.saturating_mul(crate::types::CHARS_PER_TOKEN);
    let mut rendered = Vec::with_capacity(tools.len());
    let mut dropped = Vec::new();
    let mut pool_used = 0usize;
    for tool in tools {
        let tagged = format!(
            "{}(args: {}): Promise<unknown>;",
            tool.js_name,
            schema_to_type(&tool.parameters_schema, true)
        );
        let sample = |declaration: &str| -> String {
            format!(
                "{}\n\ncodemode tool declaration:\n```ts\ndeclare const tools: {{ {} }};\n```",
                tool.description.trim(),
                declaration
            )
        };
        let mut declaration = tagged.clone();
        if sample(&tagged).chars().count() > budget_chars {
            let untagged = format!(
                "{}(args: {}): Promise<unknown>;",
                tool.js_name,
                schema_to_type(&tool.parameters_schema, false)
            );
            if sample(&untagged).chars().count() > budget_chars {
                dropped.push(tool.name.clone());
                continue;
            }
            declaration = untagged;
        }
        let sample_chars = sample(&declaration).chars().count();
        if pool_used + sample_chars > total_chars {
            dropped.push(tool.name.clone());
            continue;
        }
        pool_used += sample_chars;
        rendered.push(RenderedTool {
            name: tool.name.clone(),
            js_name: tool.js_name.clone(),
            description: tool.description.clone(),
            declaration: Some(declaration),
        });
    }
    RenderedDeclarations { rendered, dropped }
}

/// A tool's sample: description plus the rendered declaration, the exact text
/// `describeTool` hands to the script.
pub fn tool_sample(rendered: &RenderedTool) -> String {
    match &rendered.declaration {
        Some(declaration) => format!(
            "{}\n\ncodemode tool declaration:\n```ts\ndeclare const tools: {{ {} }};\n```",
            rendered.description.trim(),
            declaration
        ),
        // Budget-dropped tools still answer describeTool with their description.
        None => rendered.description.trim().to_owned(),
    }
}

#[derive(Clone)]
struct SchemaContext<'a> {
    root: &'a Value,
    /// References being expanded on the current path, to stop at recursive types.
    resolving: HashSet<String>,
    expansions: usize,
    tags: bool,
}

/// Convert a JSON Schema to a TypeScript type expression. A result longer than
/// [`SCHEMA_MAX_CHARS`] is rendered again without the constraint tags and
/// becomes `unknown` if it is still too long.
pub fn schema_to_type(schema: &Value, tags: bool) -> String {
    let render = |tags| {
        let mut ctx = SchemaContext {
            root: schema,
            resolving: HashSet::new(),
            expansions: 0,
            tags,
        };
        to_type(schema, &mut ctx)
    };
    let typed = render(tags);
    if typed.chars().count() <= SCHEMA_MAX_CHARS {
        return typed;
    }
    let untagged = render(false);
    if untagged.chars().count() <= SCHEMA_MAX_CHARS {
        untagged
    } else {
        "unknown".to_owned()
    }
}

fn to_type(schema: &Value, ctx: &mut SchemaContext<'_>) -> String {
    match schema {
        Value::Bool(true) => "unknown".to_owned(),
        Value::Bool(false) => "never".to_owned(),
        Value::Null | Value::Number(_) | Value::String(_) | Value::Array(_) => "unknown".to_owned(),
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref") {
                return expand_ref(reference, map, ctx);
            }
            if let Some(value) = map.get("const") {
                return json_literal(value);
            }
            if let Some(Value::Array(values)) = map.get("enum") {
                let variants: Vec<String> = values.iter().map(json_literal).collect();
                return union(variants);
            }
            let variants_key = if map.contains_key("anyOf") {
                "anyOf"
            } else if map.contains_key("oneOf") {
                "oneOf"
            } else {
                ""
            };
            if let Some(Value::Array(variants)) = map.get(variants_key) {
                return union(variants.iter().map(|v| to_type(v, ctx)).collect());
            }
            if let Some(Value::Array(parts)) = map.get("allOf") {
                let rendered: Vec<String> = parts
                    .iter()
                    .map(|part| to_type(part, ctx))
                    .filter(|part| part != "unknown")
                    .collect();
                if rendered.is_empty() {
                    return "unknown".to_owned();
                }
                return rendered
                    .iter()
                    .map(|part| {
                        if part.contains(" | ") {
                            format!("({part})")
                        } else {
                            part.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" & ");
            }
            match map.get("type").and_then(Value::as_str) {
                Some("string") => return "string".to_owned(),
                Some("number") => return "number".to_owned(),
                Some("integer") => return "number".to_owned(),
                Some("boolean") => return "boolean".to_owned(),
                Some("null") => return "null".to_owned(),
                Some("array") => return array_type(map, ctx),
                Some("object") => return object_type(map, ctx),
                Some(_) => return "unknown".to_owned(),
                None => {}
            }
            if let Some(Value::Array(types)) = map.get("type") {
                let variants = types
                    .iter()
                    .map(|entry| {
                        let mut narrowed = map.clone();
                        narrowed.insert("type".to_owned(), entry.clone());
                        to_type(&Value::Object(narrowed), ctx)
                    })
                    .collect();
                return union(variants);
            }
            if map.contains_key("properties")
                || map.contains_key("additionalProperties")
                || map.contains_key("required")
            {
                return object_type(map, ctx);
            }
            if map.contains_key("items") || map.contains_key("prefixItems") {
                return array_type(map, ctx);
            }
            "unknown".to_owned()
        }
    }
}

/// Expand a local reference (`#`, `#/$defs/Item`) against the root schema.
/// Recursive references on the current path and external pointers render as
/// `unknown`.
fn expand_ref(
    reference: &str,
    _wrapper: &serde_json::Map<String, Value>,
    ctx: &mut SchemaContext<'_>,
) -> String {
    if ctx.resolving.contains(reference) || ctx.expansions >= MAX_REF_EXPANSIONS {
        return "unknown".to_owned();
    }
    let resolved = if reference == "#" {
        Some(ctx.root.clone())
    } else {
        resolve_pointer(reference, ctx.root)
    };
    let Some(target) = resolved else {
        return "unknown".to_owned();
    };
    ctx.expansions += 1;
    ctx.resolving.insert(reference.to_owned());
    let result = to_type(&target, ctx);
    ctx.resolving.remove(reference);
    result
}

/// The subschema a local JSON pointer names in `root`, or `None` for external
/// references and pointers that name nothing. Pointer segments are
/// percent-decoded and JSON-Pointer-unescaped (`~1` is `/`, `~0` is `~`).
fn resolve_pointer(reference: &str, root: &Value) -> Option<Value> {
    let path = reference.strip_prefix("#/")?;
    let mut current = root.clone();
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        let decoded = percent_decode(segment)?;
        let key = decoded.replace("~1", "/").replace("~0", "~");
        let Value::Object(map) = &current else {
            return None;
        };
        current = map.get(&key)?.clone();
    }
    Some(current)
}

fn percent_decode(segment: &str) -> Option<String> {
    if !segment.contains('%') {
        return Some(segment.to_owned());
    }
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            out.push(value);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn json_literal(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "unknown".to_owned())
}

fn union(variants: Vec<String>) -> String {
    let mut unique: Vec<String> = Vec::new();
    for variant in variants {
        if !unique.contains(&variant) {
            unique.push(variant);
        }
    }
    if unique.iter().any(|variant| variant == "unknown") {
        return "unknown".to_owned();
    }
    if unique.is_empty() {
        return "never".to_owned();
    }
    unique.join(" | ")
}

fn array_type(map: &serde_json::Map<String, Value>, ctx: &mut SchemaContext<'_>) -> String {
    if let Some(items) = map.get("items")
        && !items.is_array()
    {
        return format!("Array<{}>", to_type(items, ctx));
    }
    let tuple_items = match map.get("prefixItems").or_else(|| map.get("items")) {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    if !tuple_items.is_empty() {
        let members: Vec<String> = tuple_items.iter().map(|item| to_type(item, ctx)).collect();
        return format!("[{}]", members.join(", "));
    }
    "unknown[]".to_owned()
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    chars.all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '$')
}

fn object_type(map: &serde_json::Map<String, Value>, ctx: &mut SchemaContext<'_>) -> String {
    let properties = match map.get("properties") {
        Some(Value::Object(properties)) => properties.clone(),
        _ => serde_json::Map::new(),
    };
    let required: HashSet<&str> = match map.get("required") {
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
        _ => HashSet::new(),
    };
    let mut names: Vec<&String> = properties.keys().collect();
    names.sort();
    let members: Vec<String> = names
        .iter()
        .map(|name| {
            let optional = if required.contains(name.as_str()) {
                ""
            } else {
                "?"
            };
            format!(
                "{}{}: {};",
                property_key(name),
                optional,
                to_type(&properties[*name], ctx)
            )
        })
        .collect();
    let mut extras = Vec::new();
    match map.get("additionalProperties") {
        Some(Value::Bool(false)) => {}
        Some(additional) => {
            let value_type = if additional == &Value::Bool(true) {
                "unknown".to_owned()
            } else {
                to_type(additional, ctx)
            };
            extras.push(format!("[key: string]: {value_type};"));
        }
        None if names.is_empty() => extras.push("[key: string]: unknown;".to_owned()),
        None => {}
    }

    let mut comments: Vec<Vec<String>> = Vec::new();
    for name in &names {
        let mut lines = description_lines(&properties[*name]);
        if ctx.tags {
            let tags = constraint_tags(&properties[*name]);
            if !tags.is_empty() {
                lines.push(tags);
            }
        }
        comments.push(lines);
    }

    if comments.iter().all(|lines| lines.is_empty()) {
        let mut all = members.clone();
        all.extend(extras.clone());
        if all.is_empty() {
            return "{}".to_owned();
        }
        return format!("{{ {} }}", all.join(" "));
    }

    let mut lines = vec!["{".to_owned()];
    for (index, comment) in comments.iter().enumerate() {
        for line in comment {
            lines.push(format!("  // {line}"));
        }
        lines.push(format!("  {}", members[index]));
    }
    for extra in &extras {
        lines.push(format!("  {extra}"));
    }
    lines.push("}".to_owned());
    lines.join("\n")
}

fn property_key(name: &str) -> String {
    if is_identifier(name) {
        name.to_owned()
    } else {
        json_literal(&Value::String(name.to_owned()))
    }
}

fn description_lines(schema: &Value) -> Vec<String> {
    let Some(description) = schema.get("description").and_then(Value::as_str) else {
        return Vec::new();
    };
    description
        .trim()
        .split(['\r', '\n', '\u{2028}', '\u{2029}'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Constraints of a property that its TypeScript type cannot express, as tags:
/// `@integer @minimum 1 @default 20`. Empty when there are none.
fn constraint_tags(schema: &Value) -> String {
    let Some(property) = schema.as_object() else {
        return String::new();
    };
    // Optional fields such as Pydantic's `int | None` keep their constraints in
    // the non-null branch; the branch's constraints apply alongside the
    // property's own.
    let variants = property
        .get("anyOf")
        .or_else(|| property.get("oneOf"))
        .and_then(Value::as_array);
    let branch = variants.and_then(|variants| {
        let non_null: Vec<&Value> = variants
            .iter()
            .filter(|variant| variant.get("type").and_then(Value::as_str) != Some("null"))
            .collect();
        if non_null.len() == 1 {
            non_null[0].as_object()
        } else {
            None
        }
    });

    let is_integer = |schema: Option<&serde_json::Map<String, Value>>| -> bool {
        match schema.and_then(|schema| schema.get("type")) {
            Some(Value::String(ty)) => ty == "integer",
            Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some("integer")),
            _ => false,
        }
    };
    let mut tags: Vec<String> = Vec::new();
    if is_integer(Some(property)) || is_integer(branch) {
        tags.push("@integer".to_owned());
    }
    // Draft-04 and OpenAPI 3.0 make `minimum`/`maximum` exclusive with a
    // boolean `exclusiveMinimum`/`exclusiveMaximum`; remap so the tag keeps the
    // exclusive-ness.
    let exclusive_minimum = property.get("exclusiveMinimum").and_then(Value::as_bool) == Some(true);
    let exclusive_maximum = property.get("exclusiveMaximum").and_then(Value::as_bool) == Some(true);
    let mut exclusive_tags: Vec<String> = Vec::new();
    if exclusive_minimum && let Some(value) = property.get("minimum").and_then(Value::as_f64) {
        exclusive_tags.push(format!("@exclusiveMinimum {}", format_number(value)));
    }
    if exclusive_maximum && let Some(value) = property.get("maximum").and_then(Value::as_f64) {
        exclusive_tags.push(format!("@exclusiveMaximum {}", format_number(value)));
    }
    for keyword in TAG_KEYWORDS {
        if (keyword == "minimum" && exclusive_minimum)
            || (keyword == "maximum" && exclusive_maximum)
        {
            continue;
        }
        let value_of = |schema: Option<&serde_json::Map<String, Value>>| -> Vec<Value> {
            match schema.and_then(|schema| schema.get(keyword)) {
                Some(value @ Value::Number(_)) | Some(value @ Value::String(_)) => {
                    vec![value.clone()]
                }
                _ => Vec::new(),
            }
        };
        let mut values = value_of(Some(property));
        values.extend(value_of(branch));
        if values.is_empty() {
            continue;
        }
        if LOWER_BOUNDS.contains(&keyword) {
            let binding = values
                .iter()
                .filter_map(Value::as_f64)
                .fold(f64::NEG_INFINITY, f64::max);
            tags.push(format!("@{keyword} {}", format_number(binding)));
        } else if UPPER_BOUNDS.contains(&keyword) {
            let binding = values
                .iter()
                .filter_map(Value::as_f64)
                .fold(f64::INFINITY, f64::min);
            tags.push(format!("@{keyword} {}", format_number(binding)));
        } else {
            let mut seen = HashSet::new();
            for value in values {
                if seen.insert(json_literal(&value)) {
                    tags.push(format!("@{keyword} {}", json_literal(&value)));
                }
            }
        }
    }
    tags.extend(exclusive_tags);
    let has_unique = |schema: Option<&serde_json::Map<String, Value>>| {
        schema
            .and_then(|schema| schema.get("uniqueItems"))
            .and_then(Value::as_bool)
            == Some(true)
    };
    if has_unique(Some(property)) || has_unique(branch) {
        tags.push("@uniqueItems".to_owned());
    }
    // Annotations, unlike constraints, come from the property itself.
    match property.get("default") {
        Some(value @ Value::Number(_))
        | Some(value @ Value::String(_))
        | Some(value @ Value::Bool(_)) => {
            let tag = format!("@default {}", json_literal(value));
            if tag.chars().count() <= MAX_TAG_CHARS {
                tags.push(tag);
            }
        }
        _ => {}
    }
    if property.get("deprecated").and_then(Value::as_bool) == Some(true) {
        tags.push("@deprecated".to_owned());
    }
    tags.retain(|tag| tag.chars().count() <= MAX_TAG_CHARS);
    tags.join(" ")
}

fn format_number(value: f64) -> String {
    if value == f64::fract(value) && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema_type(schema: Value) -> String {
        schema_to_type(&schema, true)
    }

    #[test]
    fn primitives_and_containers() {
        // A bare JSON string is not a type: non-object schemas match anything.
        assert_eq!(schema_type(json!("string")), "unknown");
        assert_eq!(schema_type(json!({"type": "string"})), "string");
        assert_eq!(schema_type(json!({"type": "integer"})), "number");
        assert_eq!(
            schema_type(json!({"type": "array", "items": {"type": "string"}})),
            "Array<string>"
        );
        assert_eq!(
            schema_type(
                json!({"type": "array", "prefixItems": [{"type": "string"}, {"type": "number"}]})
            ),
            "[string, number]"
        );
        assert_eq!(schema_type(json!({"const": "fixed"})), "\"fixed\"");
        assert_eq!(
            schema_type(json!({"enum": ["a", 1, null]})),
            "\"a\" | 1 | null"
        );
        assert_eq!(
            schema_type(json!({"anyOf": [{"type": "string"}, {"type": "number"}]})),
            "string | number"
        );
        assert_eq!(schema_type(json!({})), "unknown");
    }

    #[test]
    fn object_with_descriptions_and_tags_renders_comments() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to read.\nSecond line."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20, "description": "Rows"},
                "flag": {"type": "boolean"}
            },
            "required": ["path"]
        });
        // Properties render sorted by name; `path` is required, the rest optional.
        assert_eq!(
            schema_type(schema),
            "{\n  flag?: boolean;\n  // Rows\n  // @integer @minimum 1 @maximum 100 @default 20\n  limit?: number;\n  // File to read.\n  // Second line.\n  path: string;\n}"
        );
    }

    #[test]
    fn object_without_comments_stays_on_one_line() {
        let schema = json!({
            "type": "object",
            "properties": {"b": {"type": "number"}, "a": {"type": "string"}},
            "required": ["a", "b"]
        });
        assert_eq!(schema_type(schema), "{ a: string; b: number; }");
    }

    #[test]
    fn additional_properties_and_non_identifier_keys() {
        let schema = json!({
            "type": "object",
            "properties": {"weird key": {"type": "string"}},
            "additionalProperties": {"type": "number"}
        });
        assert_eq!(
            schema_type(schema),
            "{ \"weird key\"?: string; [key: string]: number; }"
        );
        assert_eq!(
            schema_type(json!({"type": "object"})),
            "{ [key: string]: unknown; }"
        );
    }

    #[test]
    fn local_refs_expand_and_recursion_yields_unknown() {
        let schema = json!({
            "type": "object",
            "$defs": {"Item": {"type": "object", "properties": {"child": {"$ref": "#/$defs/Item"}, "name": {"type": "string"}}, "required": ["name"]}},
            "properties": {"root": {"$ref": "#/$defs/Item"}},
            "required": ["root"]
        });
        // The self-reference one level down renders as `unknown` (recursive).
        assert_eq!(
            schema_type(schema),
            "{ root: { child?: unknown; name: string; }; }"
        );
        // External references name nothing here.
        assert_eq!(
            schema_type(json!({"$ref": "https://example.com/schema.json"})),
            "unknown"
        );
        assert_eq!(schema_type(json!({"$ref": "#/missing"})), "unknown");
    }

    #[test]
    fn draft04_exclusive_bounds_render_as_exclusive_tags() {
        let schema = json!({
            "type": "object",
            "properties": {
                "n": {"type": "number", "minimum": 0, "exclusiveMinimum": true, "maximum": 10, "exclusiveMaximum": true}
            }
        });
        assert_eq!(
            schema_type(schema),
            "{\n  // @exclusiveMinimum 0 @exclusiveMaximum 10\n  n?: number;\n}"
        );
    }

    fn tool(name: &str, schema: Value, description: &str) -> DeclaredTool {
        DeclaredTool {
            name: name.to_owned(),
            js_name: name.to_owned(),
            description: description.to_owned(),
            parameters_schema: schema,
        }
    }

    #[test]
    fn budget_first_drops_tags_then_whole_tools() {
        let big_description = "d".repeat(120);
        // With tags the sample exceeds 60 tokens (240 chars); without tags it fits.
        let tagged_heavy = json!({
            "type": "object",
            "properties": {
                "a": {"type": "string", "minimum": 1, "maxLength": 5, "pattern": "aaaa", "format": "uuid", "default": "x"}
            }
        });
        // Even untagged the sample is too large: the whole tool is dropped.
        let too_large = json!({
            "type": "object",
            "properties": {
                "a": {"type": "string", "description": "word ".repeat(60).trim().to_owned()}
            }
        });
        let tools = vec![
            tool("small", json!({"type": "string"}), "fits"),
            tool("heavy", tagged_heavy, &big_description),
            tool("huge", too_large, "always over"),
        ];
        let rendered = render_declarations_with_budget(&tools, 60);
        assert_eq!(rendered.dropped, vec!["huge".to_owned()]);
        assert_eq!(rendered.rendered.len(), 2);
        // `heavy` kept but without tags.
        let heavy = &rendered.rendered[1];
        let declaration = heavy.declaration.clone().expect("heavy rendered");
        assert!(
            declaration.contains("args: {"),
            "untagged object: {declaration}"
        );
        assert!(!declaration.contains('@'), "tags dropped: {declaration}");
        assert!(tool_sample(&rendered.rendered[0]).contains("declare const tools"));
    }

    #[test]
    fn sample_format_matches_describe_contract() {
        let rendered = RenderedTool {
            name: "read".into(),
            js_name: "read".into(),
            description: "Read a file.".into(),
            declaration: Some("read(args: { path: string; }): Promise<unknown>;".into()),
        };
        let sample = tool_sample(&rendered);
        assert_eq!(
            sample,
            "Read a file.\n\ncodemode tool declaration:\n```ts\ndeclare const tools: { read(args: { path: string; }): Promise<unknown>; };\n```"
        );
    }

    /// The shared total budget admits samples in input order (first come,
    /// first served) and drops whole tools once the pool is spent: shrinking
    /// the pool drops the LATER tools, never the earlier ones. Each sample
    /// here is ~118 chars, so a 400-char pool (100 est-tokens) admits three
    /// of the four.
    #[test]
    fn total_budget_admits_in_order_and_drops_later_tools() {
        let description = "x".repeat(20);
        let tools: Vec<DeclaredTool> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| tool(name, json!({"type": "string"}), &description))
            .collect();
        fn names(rendered: &RenderedDeclarations) -> Vec<&str> {
            rendered.rendered.iter().map(|t| t.name.as_str()).collect()
        }
        let rendered = render_declarations_with_total_budget(&tools, 60, 100);
        assert_eq!(names(&rendered), vec!["a", "b", "c"]);
        assert_eq!(rendered.dropped, vec!["d".to_owned()]);

        // A tighter pool keeps only the first tool.
        let rendered = render_declarations_with_total_budget(&tools, 60, 40);
        assert_eq!(names(&rendered), vec!["a"]);
        assert_eq!(
            rendered.dropped,
            vec!["b".to_owned(), "c".to_owned(), "d".to_owned()]
        );

        // A per-tool-dropped tool consumes nothing from the pool: the rest
        // share it undiminished.
        let oversized = tool(
            "huge",
            json!({"type": "object", "properties": {"d": {"type": "string", "description": "y".repeat(1000)}}}),
            &description,
        );
        let mut mixed = vec![oversized];
        mixed.extend(tools);
        let rendered = render_declarations_with_total_budget(&mixed, 60, 100);
        assert_eq!(names(&rendered), vec!["a", "b", "c"]);
        assert_eq!(
            rendered.dropped,
            vec!["huge".to_owned(), "d".to_owned()],
            "drops stay in input order"
        );
    }
}
