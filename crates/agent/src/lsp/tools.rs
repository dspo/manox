//! LSP `AgentTool` adapters. Three lifecycle tools surface the harness-driven
//! spawn/readiness model; six code-intel tools route a file path to the right
//! server client by extension and return `path:line:col` or text summaries.
//!
//! Position input is `(path, line, symbol, column?)`: the model normally picks
//! the symbol text off a line and the client resolves the exact column. When a
//! symbol occurs twice on one line, the optional 1-indexed column disambiguates
//! it. Code-intel calls
//! go through `LspRegistry::client_for`, which ensures + bounded-waits
//! (≤5s) and returns an "indexing, retry" error instead of blocking forever.

// ─── tool name constants ────────────────────────────────────────────────────

pub const LSP_STATUS: &str = "LspStatus";
pub const LSP_ENSURE: &str = "LspEnsure";
pub const LSP_WAIT_READY: &str = "LspWaitReady";
pub const GO_TO_DEFINITION: &str = "GoToDefinition";
pub const FIND_REFERENCES: &str = "FindReferences";
pub const HOVER: &str = "Hover";
pub const DOCUMENT_SYMBOLS: &str = "DocumentSymbols";
pub const WORKSPACE_SYMBOLS: &str = "WorkspaceSymbols";
pub const DIAGNOSTICS: &str = "Diagnostics";

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
    /// spec id (`rust-analyzer`). When omitted, reports every supported server.
    #[serde(default)]
    language: Option<String>,
}

impl AgentTool for LspStatusTool {
    fn name(&self) -> &str {
        LSP_STATUS
    }
    fn description(&self) -> &str {
        "Report the status of language servers (rust-analyzer/gopls/pyright/typescript-language-server) for the current project. \
         Returns `not_started` (installed, not spawned yet — the default), `starting`, `ready`, `failed`, or an actionable missing/broken probe detail per server. \
         Cheap: does not spawn a server. Optional `language` filters to one. \
         Use this before code-intel tools to decide whether to call LspEnsure/LspWaitReady."
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
                all.retain(|report| report.id == want);
            }
            if all.is_empty() {
                return Ok("No matching LSP server is configured".to_string());
            }
            let body = all
                .into_iter()
                .map(|report| match report.detail {
                    Some(detail) => format!("{}: {:?} ({detail})", report.id, report.status),
                    None => format!("{}: {:?}", report.id, report.status),
                })
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
        LSP_ENSURE
    }
    fn description(&self) -> &str {
        "Ensure a language server is spawned for the current project. Non-blocking: kicks off spawn+initialize in the background and returns `starting` immediately, or `ready`/`failed`/`not_installed`. \
         Idempotent — call repeatedly. Pair with LspWaitReady when you need to block until ready. \
         Code-intel tools (GoToDefinition etc.) also ensure implicitly, so calling this is optional but lets you warm a server before first use."
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
            match reg.availability(spec_id) {
                Some(lsp::registry::ServerAvailability::Available) => {}
                Some(lsp::registry::ServerAvailability::Broken(reason)) => {
                    return Ok(format!("{spec_id}: broken ({reason})"));
                }
                Some(lsp::registry::ServerAvailability::NotInstalled) | None => {
                    return Ok(format!("{spec_id}: not_installed"));
                }
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
        LSP_WAIT_READY
    }
    fn description(&self) -> &str {
        "Block until a language server is ready (or the timeout elapses). Returns `ready`/`starting`(timed out, still indexing)/`failed`/`not_installed`. \
         This is the explicit 'I decide to wait' entry point — LspEnsure kicks off spawn non-blocking, then LspWaitReady blocks on it. \
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
            match reg.availability(spec_id) {
                Some(lsp::registry::ServerAvailability::Available) => {}
                Some(lsp::registry::ServerAvailability::Broken(reason)) => {
                    return Ok(format!("{spec_id}: broken ({reason})"));
                }
                Some(lsp::registry::ServerAvailability::NotInstalled) | None => {
                    return Ok(format!("{spec_id}: not_installed"));
                }
            }
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
    let client = reg.client_for_path(&abs, cwd).await?;
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
    /// Optional 1-indexed character column. Required only when the symbol text
    /// occurs more than once on the selected line.
    #[serde(default)]
    column: Option<u32>,
}

impl AgentTool for GoToDefinitionTool {
    fn name(&self) -> &str {
        GO_TO_DEFINITION
    }
    fn description(&self) -> &str {
        "Find where a symbol is defined (LSP textDocument/definition). Input: path + 1-indexed line + the symbol text on that line; pass optional 1-indexed column only when the symbol occurs twice on the line. \
         Returns `path:line:col` triples (feed straight to read_file). Routes by file extension: .rs→rust-analyzer, .go→gopls, .py→pyright, .ts/.tsx/...→typescript-language-server. \
         The server must be installed and warmed (LspEnsure/LspWaitReady); returns an 'indexing, retry' note if not ready yet."
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
                .go_to_definition(&abs, parsed.line, &parsed.symbol, parsed.column)
                .await?;
            Ok(render_locs(
                locs,
                "No definitions found (server may still be indexing; call LspStatus or retry shortly).",
                "narrow the symbol or call DocumentSymbols first",
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
    #[serde(default)]
    column: Option<u32>,
    /// Include the declaration site among references. Default true.
    #[serde(default = "default_true")]
    include_declaration: bool,
}

fn default_true() -> bool {
    true
}

impl AgentTool for FindReferencesTool {
    fn name(&self) -> &str {
        FIND_REFERENCES
    }
    fn description(&self) -> &str {
        "Find all references to a symbol (LSP textDocument/references). Semantically accurate — unlike grep, won't be swamped by same-name identifiers in other scopes. \
         Input: path + 1-indexed line + symbol text, plus optional column for same-line ambiguity. Returns `path:line:col` triples. include_declaration defaults true. Same routing/ready rules as GoToDefinition."
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
                    parsed.column,
                    parsed.include_declaration,
                )
                .await?;
            Ok(render_locs(
                locs,
                "No references found (server may still be indexing; call LspStatus or retry shortly).",
                "narrow the symbol or call DocumentSymbols first",
            ))
        })
    }
}

pub struct HoverTool;

impl AgentTool for HoverTool {
    fn name(&self) -> &str {
        HOVER
    }
    fn description(&self) -> &str {
        "Get hover info for a symbol (LSP textDocument/hover): type signature, doc comments. \
         Input: path + 1-indexed line + symbol text, plus optional column for same-line ambiguity. Returns markdown/plaintext, or 'no hover' if the server has nothing. Same routing/ready rules as GoToDefinition."
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
            match client
                .hover(&abs, parsed.line, &parsed.symbol, parsed.column)
                .await?
            {
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
        DOCUMENT_SYMBOLS
    }
    fn description(&self) -> &str {
        "List the symbol tree of a file (LSP textDocument/documentSymbol): functions, structs, methods, etc. with kind and 1-indexed line. \
         Input: path only. Use before GoToDefinition/FindReferences to pick exact symbol names and lines."
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
                    "No symbols (server may still be indexing; call LspStatus or retry shortly)."
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
        WORKSPACE_SYMBOLS
    }
    fn description(&self) -> &str {
        "Search symbols across the whole workspace (LSP workspace/symbol). Input: query string. \
         Returns `path:line:col name` triples. Which server answers depends on the servers warmed for the current project — call LspEnsure for the language(s) you care about first. \
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
                return Ok("No symbols matched (warm the servers with LspEnsure, or indexing may be incomplete).".to_string());
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
        DIAGNOSTICS
    }
    fn description(&self) -> &str {
        "Report LSP diagnostics (errors/warnings) for a file or the whole project. A file-scoped call synchronizes the current on-disk content and waits briefly for a fresh publication. \
         It explicitly reports `diagnostics unknown` on timeout and never presents missing data as a clean file. Input: optional path; omit it only to inspect the cached project-wide snapshot."
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
                let report = client
                    .diagnostics_for(&abs, std::time::Duration::from_secs(2))
                    .await?;
                return Ok(render_diagnostics_report(&abs, report));
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
                return Ok("No cached diagnostics (call LspEnsure to warm servers, then re-run; indexing may be incomplete).".to_string());
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

fn render_diagnostics_report(path: &Path, report: lsp::DiagnosticsReport) -> String {
    if !report.fresh {
        return format!(
            "{}: diagnostics unknown (no fresh publication for the current file content)",
            path.display()
        );
    }
    render_diagnostics(path, &report.diagnostics)
}

/// Fast post-edit feedback used by the harness after successful `Edit`/`Write`
/// calls. Unsupported files are ignored; supported-but-unavailable servers are
/// reported so the model can fall back to compiler/test verification.
pub(crate) async fn diagnostics_for_paths(
    paths: Vec<PathBuf>,
    cwd: PathBuf,
) -> anyhow::Result<Option<String>> {
    let Some(reg) = lsp::registry::try_global() else {
        return Ok(None);
    };
    const POST_EDIT_FILE_CAP: usize = 4;
    let paths: Vec<_> = paths
        .into_iter()
        .filter(|path| reg.routed_spec_for_path(path).is_some())
        .collect();
    let total = paths.len();
    let mut bodies = Vec::new();
    for path in paths.into_iter().take(POST_EDIT_FILE_CAP) {
        match reg.client_for_path(&path, &cwd).await {
            Ok(client) => {
                let report = client
                    .diagnostics_for(&path, std::time::Duration::from_secs(1))
                    .await?;
                bodies.push(render_diagnostics_report(&path, report));
            }
            Err(error) => bodies.push(format!(
                "{}: diagnostics unavailable ({error})",
                path.display()
            )),
        }
    }
    if total > POST_EDIT_FILE_CAP {
        bodies.push(format!(
            "{} additional changed files were not checked automatically; call Diagnostics with each path or run the project checker",
            total - POST_EDIT_FILE_CAP
        ));
    }
    if bodies.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!(
            "LSP post-edit diagnostics:\n{}",
            bodies.join("\n")
        )))
    }
}
