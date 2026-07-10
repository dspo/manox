//! LSP `AgentTool` adapters. Three lifecycle tools surface the harness-driven
//! spawn/readiness model; six code-intel tools route a file path to the right
//! server client by extension and return `path:line:col` or text summaries.
//!
//! Position input is `(path, line, symbol)`: the model picks the symbol text
//! off a line (as `read_file` showed it) and the client resolves the exact
//! column, freeing the model from counting UTF-16 offsets. Code-intel calls
//! go through `LspRegistry::client_for`, which ensures + bounded-waits
//! (≤5s) and returns an "indexing, retry" error instead of blocking forever.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, AppContext, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;
use crate::tools::{bridge_tokio, resolve_path, schema, truncate_output};

/// Output byte cap for code-intel results (paths + summaries).
const OUTPUT_CAP: usize = 8192;

// ─── language alias resolution ──────────────────────────────────────────────

/// Map a free-form language id to a spec id. Accepts the spec id itself
/// (`rust-analyzer`) or common language aliases (`rust`, `go`, `python`,
/// `typescript`/`ts`). Returns `None` for an unknown alias.
fn resolve_language(input: &str) -> Option<&'static str> {
    let id = match input.trim().to_ascii_lowercase().as_str() {
        "rust" | "rust-analyzer" | "rust_analyzer" => "rust-analyzer",
        "go" | "golang" | "gopls" => "gopls",
        "python" | "py" | "pyright" => "pyright",
        "typescript" | "ts" | "typescript-language-server" => "typescript-language-server",
        other => return lsp::spec::spec_for_id(other).map(|s| s.id),
    };
    Some(id)
}

// ─── lifecycle tools ────────────────────────────────────────────────────────

pub struct LspStatusTool;

#[derive(Deserialize, JsonSchema)]
struct LspStatusInput {
    /// Optional language id (`rust`, `go`, `python`, `typescript`) or server
    /// spec id (`rust-analyzer`). When omitted, reports every detected server.
    #[serde(default)]
    language: Option<String>,
}

impl AgentTool for LspStatusTool {
    fn name(&self) -> &str {
        "lsp_status"
    }
    fn description(&self) -> &str {
        "Report the status of language servers (rust-analyzer/gopls/pyright/typescript-language-server) for the current project. \
         Returns `not_started` (installed, not spawned yet — the default), `starting`, `ready`, `failed`, or `not_installed` per server. \
         Cheap: does not spawn a server. Optional `language` filters to one. \
         Use this before code-intel tools to decide whether to call lsp_ensure/lsp_wait_ready."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<LspStatusInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<LspStatusInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let cwd = ctx.cwd().to_path_buf();
        bridge_tokio(cx, async move {
            let Some(reg) = lsp::registry::try_global() else {
                return Ok("LSP not initialized".to_string());
            };
            let mut all = reg.statuses_for(&cwd).await;
            if let Some(lang) = parsed.language {
                let Some(want) = resolve_language(&lang) else {
                    return Ok(format!("unknown language `{lang}`"));
                };
                all.retain(|(id, _)| *id == want);
            }
            if all.is_empty() {
                return Ok("No LSP servers detected on PATH (install rust-analyzer/gopls/pyright/typescript-language-server to enable code-intel)".to_string());
            }
            let body = all
                .into_iter()
                .map(|(id, s)| format!("{id}: {s:?}"))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(body)
        })
    }
}

pub struct LspEnsureTool;

#[derive(Deserialize, JsonSchema)]
struct LspEnsureInput {
    /// Language id (`rust`/`go`/`python`/`typescript`) or server spec id.
    language: String,
}

impl AgentTool for LspEnsureTool {
    fn name(&self) -> &str {
        "lsp_ensure"
    }
    fn description(&self) -> &str {
        "Ensure a language server is spawned for the current project. Non-blocking: kicks off spawn+initialize in the background and returns `starting` immediately, or `ready`/`failed`/`not_installed`. \
         Idempotent — call repeatedly. Pair with lsp_wait_ready when you need to block until ready. \
         Code-intel tools (go_to_definition etc.) also ensure implicitly, so calling this is optional but lets you warm a server before first use."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<LspEnsureInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<LspEnsureInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let cwd = ctx.cwd().to_path_buf();
        bridge_tokio(cx, async move {
            let Some(reg) = lsp::registry::try_global() else {
                return Ok("LSP not initialized".to_string());
            };
            let Some(spec_id) = resolve_language(&parsed.language) else {
                return Ok(format!("unknown language `{}`", parsed.language));
            };
            if !reg.is_available(spec_id) {
                return Ok(format!("{spec_id}: not_installed"));
            }
            let status = reg.ensure(spec_id, cwd).await?;
            Ok(format!("{spec_id}: {status:?}"))
        })
    }
}

pub struct LspWaitReadyTool;

#[derive(Deserialize, JsonSchema)]
struct LspWaitReadyInput {
    /// Language id or server spec id.
    language: String,
    /// Seconds to wait. Default 10. The harness chooses how long to block;
    /// pass a small value to poll, a larger one to wait out indexing.
    #[serde(default = "default_wait_secs")]
    timeout_secs: u64,
}

fn default_wait_secs() -> u64 {
    10
}

impl AgentTool for LspWaitReadyTool {
    fn name(&self) -> &str {
        "lsp_wait_ready"
    }
    fn description(&self) -> &str {
        "Block until a language server is ready (or the timeout elapses). Returns `ready`/`starting`(timed out, still indexing)/`failed`/`not_installed`. \
         This is the explicit 'I decide to wait' entry point — lsp_ensure kicks off spawn non-blocking, then lsp_wait_ready blocks on it. \
         Call before a burst of code-intel calls if you want them to hit a warm server."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<LspWaitReadyInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<LspWaitReadyInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let cwd = ctx.cwd().to_path_buf();
        bridge_tokio(cx, async move {
            let Some(reg) = lsp::registry::try_global() else {
                return Ok("LSP not initialized".to_string());
            };
            let Some(spec_id) = resolve_language(&parsed.language) else {
                return Ok(format!("unknown language `{}`", parsed.language));
            };
            let status = reg
                .wait_ready(
                    spec_id,
                    cwd,
                    std::time::Duration::from_secs(parsed.timeout_secs),
                )
                .await?;
            Ok(format!("{spec_id}: {status:?}"))
        })
    }
}

// ─── code-intel tools ───────────────────────────────────────────────────────

/// Resolve a code-intel target: route the path's extension to a server and
/// ensure its client is ready (bounded wait). Returns the client and the
/// absolute path. Errors carry actionable "install / retry" guidance.
async fn client_for_path(path: &str, cwd: &Path) -> anyhow::Result<(Arc<lsp::LspClient>, PathBuf)> {
    let Some(reg) = lsp::registry::try_global() else {
        return Err(anyhow::anyhow!("LSP not initialized"));
    };
    let abs = resolve_path(path, cwd);
    let Some(spec) = reg.spec_for_path(&abs) else {
        let ext = abs
            .extension()
            .map(|e| e.to_string_lossy())
            .unwrap_or_default();
        return Err(anyhow::anyhow!(
            "no LSP server handles `.{ext}` — not a routed language (rust/go/python/typescript), or its server isn't on PATH"
        ));
    };
    let client = reg.client_for(spec.id, cwd.to_path_buf()).await?;
    Ok((client, abs))
}

/// Format a `(path, line, col)` triple as `path:line:col`, the shape
/// `read_file` accepts directly.
fn fmt_loc(path: &Path, line: u32, col: u32) -> String {
    format!("{}:{line}:{col}", path.display())
}

fn render_locs(locs: Vec<(PathBuf, u32, u32)>, empty_hint: &str, hint: &str) -> String {
    if locs.is_empty() {
        return empty_hint.to_string();
    }
    let body = locs
        .iter()
        .map(|(p, l, c)| fmt_loc(p, *l, *c))
        .collect::<Vec<_>>()
        .join("\n");
    truncate_output(&body, OUTPUT_CAP).render(hint)
}

pub struct GoToDefinitionTool;

#[derive(Deserialize, JsonSchema)]
struct PositionInput {
    /// File path, relative to cwd or absolute.
    path: String,
    /// 1-indexed line number (as shown by read_file).
    line: u32,
    /// The symbol text on that line whose definition to jump to. Picked off
    /// the line as read_file showed it; the client resolves the exact column.
    symbol: String,
}

impl AgentTool for GoToDefinitionTool {
    fn name(&self) -> &str {
        "go_to_definition"
    }
    fn description(&self) -> &str {
        "Find where a symbol is defined (LSP textDocument/definition). Input: path + 1-indexed line + the symbol text on that line. \
         Returns `path:line:col` triples (feed straight to read_file). Routes by file extension: .rs→rust-analyzer, .go→gopls, .py→pyright, .ts/.tsx/...→typescript-language-server. \
         The server must be installed and warmed (lsp_ensure/lsp_wait_ready); returns an 'indexing, retry' note if not ready yet."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<PositionInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<PositionInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let cwd = ctx.cwd().to_path_buf();
        bridge_tokio(cx, async move {
            let (client, abs) = client_for_path(&parsed.path, &cwd).await?;
            let locs = client
                .go_to_definition(&abs, parsed.line, &parsed.symbol)
                .await?;
            Ok(render_locs(
                locs,
                "No definitions found (server may still be indexing; call lsp_status or retry shortly).",
                "narrow the symbol or call document_symbols first",
            ))
        })
    }
}

pub struct FindReferencesTool;

#[derive(Deserialize, JsonSchema)]
struct ReferencesInput {
    path: String,
    line: u32,
    symbol: String,
    /// Include the declaration site among references. Default true.
    #[serde(default = "default_true")]
    include_declaration: bool,
}

fn default_true() -> bool {
    true
}

impl AgentTool for FindReferencesTool {
    fn name(&self) -> &str {
        "find_references"
    }
    fn description(&self) -> &str {
        "Find all references to a symbol (LSP textDocument/references). Semantically accurate — unlike grep, won't be swamped by same-name identifiers in other scopes. \
         Input: path + 1-indexed line + symbol text. Returns `path:line:col` triples. include_declaration defaults true. Same routing/ready rules as go_to_definition."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<ReferencesInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<ReferencesInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let cwd = ctx.cwd().to_path_buf();
        bridge_tokio(cx, async move {
            let (client, abs) = client_for_path(&parsed.path, &cwd).await?;
            let locs = client
                .find_references(
                    &abs,
                    parsed.line,
                    &parsed.symbol,
                    parsed.include_declaration,
                )
                .await?;
            Ok(render_locs(
                locs,
                "No references found (server may still be indexing; call lsp_status or retry shortly).",
                "narrow the symbol or call document_symbols first",
            ))
        })
    }
}

pub struct HoverTool;

impl AgentTool for HoverTool {
    fn name(&self) -> &str {
        "hover"
    }
    fn description(&self) -> &str {
        "Get hover info for a symbol (LSP textDocument/hover): type signature, doc comments. \
         Input: path + 1-indexed line + symbol text. Returns markdown/plaintext, or 'no hover' if the server has nothing. Same routing/ready rules as go_to_definition."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<PositionInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<PositionInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let cwd = ctx.cwd().to_path_buf();
        bridge_tokio(cx, async move {
            let (client, abs) = client_for_path(&parsed.path, &cwd).await?;
            match client.hover(&abs, parsed.line, &parsed.symbol).await? {
                Some(text) => Ok(truncate_output(&text, OUTPUT_CAP).render("narrow the symbol")),
                None => Ok("No hover information available".to_string()),
            }
        })
    }
}

pub struct DocumentSymbolsTool;

#[derive(Deserialize, JsonSchema)]
struct PathInput {
    path: String,
}

impl AgentTool for DocumentSymbolsTool {
    fn name(&self) -> &str {
        "document_symbols"
    }
    fn description(&self) -> &str {
        "List the symbol tree of a file (LSP textDocument/documentSymbol): functions, structs, methods, etc. with kind and 1-indexed line. \
         Input: path only. Use before go_to_definition/find_references to pick exact symbol names and lines."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<PathInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<PathInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let cwd = ctx.cwd().to_path_buf();
        bridge_tokio(cx, async move {
            let (client, abs) = client_for_path(&parsed.path, &cwd).await?;
            let syms = client.document_symbols(&abs).await?;
            if syms.is_empty() {
                return Ok(
                    "No symbols (server may still be indexing; call lsp_status or retry shortly)."
                        .to_string(),
                );
            }
            let body = syms
                .into_iter()
                .map(|(name, kind, line)| format!("{kind} {name} :{line}"))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(truncate_output(&body, OUTPUT_CAP)
                .render("scope to a region or use a narrower file"))
        })
    }
}

pub struct WorkspaceSymbolsTool;

#[derive(Deserialize, JsonSchema)]
struct QueryInput {
    /// Free-text symbol query (prefix/fuzzy depending on server).
    query: String,
}

impl AgentTool for WorkspaceSymbolsTool {
    fn name(&self) -> &str {
        "workspace_symbols"
    }
    fn description(&self) -> &str {
        "Search symbols across the whole workspace (LSP workspace/symbol). Input: query string. \
         Returns `path:line:col name` triples. Which server answers depends on the servers warmed for the current project — call lsp_ensure for the language(s) you care about first. \
         Note: a server only returns symbols it has indexed; an empty result may mean indexing isn't done."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<QueryInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<QueryInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let cwd = ctx.cwd().to_path_buf();
        bridge_tokio(cx, async move {
            let Some(reg) = lsp::registry::try_global() else {
                return Err(anyhow::anyhow!("LSP not initialized"));
            };
            // Query every available server concurrently; a workspace symbol call
            // without a file path can't route by extension, so fan out and merge.
            let specs: Vec<&'static lsp::spec::LspServerSpec> = reg.available_specs().to_vec();
            let mut handles = Vec::new();
            for spec in specs {
                let query = parsed.query.clone();
                let cwd = cwd.clone();
                handles.push(tokio::spawn(async move {
                    let client = reg.client_for(spec.id, cwd).await?;
                    client.workspace_symbols(&query).await
                }));
            }
            let mut all: Vec<(PathBuf, String, u32, u32)> = Vec::new();
            for h in handles {
                match h.await {
                    Ok(Ok(syms)) => all.extend(syms),
                    Ok(Err(e)) => tracing::warn!("workspace_symbols from a server failed: {e}"),
                    Err(e) => tracing::warn!("workspace_symbols task panicked: {e}"),
                }
            }
            if all.is_empty() {
                return Ok("No symbols matched (warm the servers with lsp_ensure, or indexing may be incomplete).".to_string());
            }
            all.sort_by_key(|(_, _, line, _)| *line);
            let body = all
                .into_iter()
                .map(|(p, name, line, col)| format!("{}:{}:{} {name}", p.display(), line, col))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(truncate_output(&body, OUTPUT_CAP).render("narrow the query"))
        })
    }
}

pub struct DiagnosticsTool;

#[derive(Deserialize, JsonSchema)]
struct DiagnosticsInput {
    /// File path. When omitted, reports every file with cached diagnostics.
    #[serde(default)]
    path: Option<String>,
}

impl AgentTool for DiagnosticsTool {
    fn name(&self) -> &str {
        "diagnostics"
    }
    fn description(&self) -> &str {
        "Report LSP diagnostics (errors/warnings) for a file or the whole project. These are what the language server pushes as it indexes; \
         an empty result may mean no problems OR none pushed yet (check lsp_status). Input: optional path. \
         NOTE: diagnostics reflect on-disk state (the server watches files); they may be stale during initial indexing. Re-run after edits settle."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<DiagnosticsInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<DiagnosticsInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let cwd = ctx.cwd().to_path_buf();
        bridge_tokio(cx, async move {
            if let Some(path) = parsed.path {
                let (client, abs) = client_for_path(&path, &cwd).await?;
                let diags = client.cached_diagnostics(&abs);
                return Ok(render_diagnostics(&abs, &diags));
            }
            // No path: dump cached diagnostics across every warmed server.
            let Some(reg) = lsp::registry::try_global() else {
                return Err(anyhow::anyhow!("LSP not initialized"));
            };
            let specs: Vec<&'static lsp::spec::LspServerSpec> = reg.available_specs().to_vec();
            let mut bodies = Vec::new();
            for spec in specs {
                let cwd = cwd.clone();
                let client = match reg.client_for(spec.id, cwd).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("diagnostics for {} unavailable: {e}", spec.id);
                        continue;
                    }
                };
                for path in client.cached_diagnostic_files() {
                    let diags = client.cached_diagnostics(&path);
                    if !diags.is_empty() {
                        bodies.push(render_diagnostics(&path, &diags));
                    }
                }
            }
            if bodies.is_empty() {
                return Ok("No cached diagnostics (call lsp_ensure to warm servers, then re-run; indexing may be incomplete).".to_string());
            }
            Ok(truncate_output(&bodies.join("\n\n"), OUTPUT_CAP)
                .render("scope to a single file via the `path` argument"))
        })
    }
}

/// Render a file's diagnostics as `path:line:col [severity] message`.
fn render_diagnostics(path: &Path, diags: &[lsp_types::Diagnostic]) -> String {
    if diags.is_empty() {
        return format!("{}: no diagnostics", path.display());
    }
    let body = diags
        .iter()
        .map(|d| {
            let sev = match d.severity {
                Some(lsp_types::DiagnosticSeverity::ERROR) => "error",
                Some(lsp_types::DiagnosticSeverity::WARNING) => "warning",
                Some(lsp_types::DiagnosticSeverity::INFORMATION) => "info",
                Some(lsp_types::DiagnosticSeverity::HINT) => "hint",
                _ => "diagnostic",
            };
            let line = d.range.start.line + 1;
            let col = d.range.start.character + 1;
            let msg = d.message.trim();
            format!("{}:{line}:{col} [{sev}] {msg}", path.display())
        })
        .collect::<Vec<_>>()
        .join("\n");
    truncate_output(&body, OUTPUT_CAP).render("scope to a single file via the `path` argument")
}
