//! Multi-level `CLAUDE.md` instruction loading, compatible with Claude Code's
//! memory hierarchy.
//!
//! Every turn, [`load`] collects instruction files from broadest scope to most
//! specific and [`render_eager`] packs them into a single model-facing block:
//!
//! 1. the managed-policy file (platform path, exempt from `excludes`),
//! 2. `~/.claude/CLAUDE.md` and unconditional `~/.claude/rules/**/*.md`,
//! 3. for every directory from the filesystem root down to the thread's cwd:
//!    `CLAUDE.md`, `.claude/CLAUDE.md`, unconditional `.claude/rules/**/*.md`,
//!    then `CLAUDE.local.md`.
//!
//! Rules carrying a `paths` frontmatter glob list are not eager; they land in
//! [`InstructionSet::scoped`] for the read-triggered lazy layer. All files are
//! concatenated (nothing overrides anything), HTML comments outside code
//! fences are stripped, and `@path` imports are expanded in place (relative to
//! the importing file, `~/` supported, four hops deep, cycle-safe). Imports
//! resolving outside both the cwd subtree and the `~/.claude` subtree are
//! "external": their paths are reported via [`InstructionSet::external_imports`]
//! and their content is withheld unless [`LoadContext::allow_external`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Deepest `@import` chain followed: a top-level file expands its imports
/// (depth 0 → 1), and expansion stops once an import would be resolved at
/// depth [`MAX_IMPORT_DEPTH`].
const MAX_IMPORT_DEPTH: usize = 4;

/// The managed-policy file for this platform, when the platform has one.
/// Existence is a load-time concern; this only names the canonical location
/// Claude Code deploys to.
pub fn managed_policy_path() -> Option<PathBuf> {
    match std::env::consts::OS {
        "macos" => Some(PathBuf::from(
            "/Library/Application Support/ClaudeCode/CLAUDE.md",
        )),
        "linux" => Some(PathBuf::from("/etc/claude-code/CLAUDE.md")),
        _ => None,
    }
}

/// Where an instruction file sits in the load hierarchy. Render order follows
/// discovery order; the kind only feeds the `scope` tag attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    ManagedPolicy,
    UserGlobal,
    UserRule,
    Project,
    ProjectLocal,
    ProjectRule,
}

impl SourceKind {
    /// The `scope` attribute written into the rendered `<instructions>` tag.
    fn scope(self) -> &'static str {
        match self {
            Self::ManagedPolicy => "managed-policy",
            Self::UserGlobal => "user",
            Self::UserRule => "user-rule",
            Self::Project => "project",
            Self::ProjectLocal => "project-local",
            Self::ProjectRule => "project-rule",
        }
    }
}

/// One loaded instruction file: absolute canonical path plus its expanded,
/// comment-stripped content.
#[derive(Debug, Clone)]
pub struct InstructionSource {
    pub path: PathBuf,
    pub kind: SourceKind,
    pub content: String,
}

/// A rule whose `paths` frontmatter scopes it to matching files. Content is
/// expanded and stripped exactly like eager sources; only the trigger differs
/// (a matching `read_file`, handled by the lazy layer).
#[derive(Debug, Clone)]
pub struct ScopedRule {
    pub path: PathBuf,
    /// User rule or project rule — feeds the `scope` tag attribute when the
    /// rule is injected by the lazy layer.
    pub kind: SourceKind,
    /// The directory owning the `.claude/rules` tree; `patterns` match paths
    /// relative to this base.
    pub base_dir: PathBuf,
    /// Glob patterns that survived globset compilation (invalid ones are
    /// dropped with a warning, per-pattern).
    pub patterns: Vec<String>,
    pub content: String,
}

/// The result of one [`load`] sweep.
#[derive(Debug, Default)]
pub struct InstructionSet {
    /// Eager sources in render order (broadest → most specific).
    pub eager: Vec<InstructionSource>,
    /// Path-scoped rules awaiting a matching read.
    pub scoped: Vec<ScopedRule>,
    /// External imports encountered during expansion (deduped, sorted), for
    /// the approval preflight.
    pub external_imports: Vec<PathBuf>,
}

/// Everything [`load`] needs that a test must be able to substitute.
#[derive(Debug, Clone, Default)]
pub struct LoadContext {
    /// The user's home directory (`~/.claude` and `~/` imports anchor here).
    pub home: Option<PathBuf>,
    /// The managed-policy file; loaded first and exempt from `excludes`.
    pub managed: Option<PathBuf>,
    /// Glob patterns matched against canonical absolute paths; matching
    /// instruction files are skipped.
    pub excludes: Vec<String>,
    /// Whether external imports (outside the cwd and `~/.claude` subtrees)
    /// may be expanded.
    pub allow_external: bool,
}

/// Collect every instruction file visible from `cwd`, in render order.
///
/// Missing files and unreadable entries are skipped silently (absence is the
/// common case); malformed content (bad frontmatter, bad globs, missing
/// imports) degrades to warnings so one bad file never blocks a session.
pub fn load(cwd: &Path, ctx: &LoadContext) -> InstructionSet {
    let cwd = canonicalize_best_effort(cwd);
    let home = ctx.home.as_ref().map(|h| canonicalize_best_effort(h));
    let excludes = ExcludeSet::compile(&ctx.excludes);
    let mut out = InstructionSet::default();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut externals: Vec<PathBuf> = Vec::new();

    // 1. Managed policy: first in order, never excludable.
    if let Some(managed) = &ctx.managed {
        push_source(
            &mut out,
            &mut seen,
            managed,
            SourceKind::ManagedPolicy,
            true,
            &cwd,
            &home,
            ctx,
            &excludes,
            &mut externals,
        );
    }
    // 2. User level: global file, then user rules (before any project file).
    if let Some(home_dir) = home.as_deref() {
        let claude_dir = home_dir.join(".claude");
        push_source(
            &mut out,
            &mut seen,
            &claude_dir.join("CLAUDE.md"),
            SourceKind::UserGlobal,
            false,
            &cwd,
            &home,
            ctx,
            &excludes,
            &mut externals,
        );
        load_rules(
            &mut out,
            &mut seen,
            &claude_dir.join("rules"),
            home_dir,
            SourceKind::UserRule,
            &cwd,
            &home,
            ctx,
            &excludes,
            &mut externals,
        );
    }
    // 3. Ancestor chain, filesystem root → cwd, so the instruction closest to
    //    the launch directory reads last.
    let chain: Vec<PathBuf> = cwd
        .ancestors()
        .map(Path::to_path_buf)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    for dir in &chain {
        for (path, kind) in [
            (dir.join("CLAUDE.md"), SourceKind::Project),
            (dir.join(".claude").join("CLAUDE.md"), SourceKind::Project),
        ] {
            push_source(
                &mut out,
                &mut seen,
                &path,
                kind,
                false,
                &cwd,
                &home,
                ctx,
                &excludes,
                &mut externals,
            );
        }
        load_rules(
            &mut out,
            &mut seen,
            &dir.join(".claude").join("rules"),
            dir,
            SourceKind::ProjectRule,
            &cwd,
            &home,
            ctx,
            &excludes,
            &mut externals,
        );
        push_source(
            &mut out,
            &mut seen,
            &dir.join("CLAUDE.local.md"),
            SourceKind::ProjectLocal,
            false,
            &cwd,
            &home,
            ctx,
            &excludes,
            &mut externals,
        );
    }

    externals.sort();
    externals.dedup();
    out.external_imports = externals;
    out
}

/// Discover instruction files triggered by reading `read_path` — the lazy
/// half of the hierarchy:
///
/// - nested `CLAUDE.md` / `.claude/CLAUDE.md` / `CLAUDE.local.md` in every
///   directory strictly between `cwd` and the file's directory (inclusive),
///   top-down, same within-directory order as the eager sweep;
/// - `scoped` rules whose globs match the read path relativized to the rule's
///   `base_dir`.
///
/// Paths in `already` (and duplicates within one call) are skipped; the
/// caller folds the returned paths into its injected set. Reads outside the
/// `cwd` subtree only match scoped rules whose `base_dir` contains them.
pub fn discover_for_read(
    read_path: &Path,
    cwd: &Path,
    scoped: &[ScopedRule],
    already: &HashSet<PathBuf>,
    ctx: &LoadContext,
) -> Vec<InstructionSource> {
    let cwd = canonicalize_best_effort(cwd);
    let home = ctx.home.as_ref().map(|h| canonicalize_best_effort(h));
    let excludes = ExcludeSet::compile(&ctx.excludes);
    let read_path = canonicalize_best_effort(read_path);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    // Lazy-discovered files' own external imports stay withheld here; the
    // approval preflight reports only eager-chain externals (PR3's scope).
    let mut externals = Vec::new();

    // Nested chain, cwd-exclusive → the read file's directory, top-down.
    if let Ok(rel) = read_path.strip_prefix(&cwd) {
        let mut dir = cwd.clone();
        for comp in rel
            .parent()
            .map(|p| p.components().collect::<Vec<_>>())
            .unwrap_or_default()
        {
            dir.push(comp);
            for (name, kind) in [
                ("CLAUDE.md", SourceKind::Project),
                (".claude/CLAUDE.md", SourceKind::Project),
                ("CLAUDE.local.md", SourceKind::ProjectLocal),
            ] {
                let Ok(canon) = dir.join(name).canonicalize() else {
                    continue;
                };
                if !canon.is_file()
                    || already.contains(&canon)
                    || !seen.insert(canon.clone())
                    || excludes.is_excluded(&canon)
                {
                    continue;
                }
                let raw = match std::fs::read_to_string(&canon) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(path = %canon.display(), error = %e, "nested instruction file unreadable");
                        continue;
                    }
                };
                let content = prepare(&raw, &canon, &cwd, &home, ctx, &mut externals);
                out.push(InstructionSource {
                    path: canon,
                    kind,
                    content,
                });
            }
        }
    }

    // Path-scoped rules whose glob matches the read path (relative to the
    // rule's base directory). `scoped` order (user before project) is
    // preserved so rendering stays deterministic.
    for rule in scoped {
        if already.contains(&rule.path) || seen.contains(&rule.path) {
            continue;
        }
        let Ok(rel) = read_path.strip_prefix(&rule.base_dir) else {
            continue;
        };
        let matched = rule.patterns.iter().any(|p| {
            globset::Glob::new(p)
                .map(|g| g.compile_matcher().is_match(rel))
                .unwrap_or(false)
        });
        if matched {
            seen.insert(rule.path.clone());
            out.push(InstructionSource {
                path: rule.path.clone(),
                kind: rule.kind,
                content: rule.content.clone(),
            });
        }
    }
    out
}

/// Render lazy discoveries as a `<system-reminder>` appended to the triggering
/// `read_file` tool result. `None` when empty (no reminder attached).
pub fn render_lazy(
    sources: &[InstructionSource],
    lang: crate::language::Language,
) -> Option<String> {
    if sources.is_empty() {
        return None;
    }
    let data = crate::prompt::InstructionsPromptData {
        sources: sources
            .iter()
            .map(|s| crate::prompt::InstructionSourcePromptData {
                scope: s.kind.scope(),
                path: s.path.display().to_string(),
                content: s.content.trim_end().to_string(),
            })
            .collect(),
    };
    Some(
        crate::prompt::render(
            crate::prompt::PromptTemplate::WrapperInstructionsLazy,
            lang,
            &data,
        )
        .expect("instructions lazy template render"),
    )
}

/// Render the eager block via the `wrapper/instructions_eager` template.
/// `None` when nothing loaded, so the caller can omit the message entirely
/// and keep the request prefix byte-identical to a no-instructions session.
pub fn render_eager(set: &InstructionSet, lang: crate::language::Language) -> Option<String> {
    if set.eager.is_empty() {
        return None;
    }
    let data = crate::prompt::InstructionsPromptData {
        sources: set
            .eager
            .iter()
            .map(|s| crate::prompt::InstructionSourcePromptData {
                scope: s.kind.scope(),
                path: s.path.display().to_string(),
                content: s.content.trim_end().to_string(),
            })
            .collect(),
    };
    Some(
        crate::prompt::render(
            crate::prompt::PromptTemplate::WrapperInstructionsEager,
            lang,
            &data,
        )
        .expect("instructions eager template render"),
    )
}

/// Read one instruction file and append it to `out.eager`. Canonicalization
/// doubles as the existence check; `seen` dedups files reachable through more
/// than one path (e.g. cwd inside `~/.claude`).
#[allow(clippy::too_many_arguments)]
fn push_source(
    out: &mut InstructionSet,
    seen: &mut HashSet<PathBuf>,
    path: &Path,
    kind: SourceKind,
    exempt: bool,
    cwd: &Path,
    home: &Option<PathBuf>,
    ctx: &LoadContext,
    excludes: &ExcludeSet,
    externals: &mut Vec<PathBuf>,
) {
    let Ok(canon) = path.canonicalize() else {
        return;
    };
    if !canon.is_file() || !seen.insert(canon.clone()) {
        return;
    }
    // Everything except the managed-policy file is subject to excludes; the
    // managed file is deployed precisely so users cannot skip it.
    if !exempt && excludes.is_excluded(&canon) {
        tracing::info!(path = %canon.display(), "instruction file excluded by claude_md_excludes");
        return;
    }
    let raw = match std::fs::read_to_string(&canon) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(path = %canon.display(), error = %e, "instruction file unreadable");
            return;
        }
    };
    let content = prepare(&raw, &canon, cwd, home, ctx, externals);
    out.eager.push(InstructionSource {
        path: canon,
        kind,
        content,
    });
}

/// Comment-strip + import-expand pipeline shared by eager files and rules.
fn prepare(
    raw: &str,
    file: &Path,
    cwd: &Path,
    home: &Option<PathBuf>,
    ctx: &LoadContext,
    externals: &mut Vec<PathBuf>,
) -> String {
    let stripped = strip_html_comments(raw);
    let mut visited = HashSet::from([file.to_path_buf()]);
    expand_imports(&stripped, file, 0, &mut visited, cwd, home, ctx, externals)
}

/// Scan a `rules` directory recursively for `.md` files (sorted for stable
/// output). Unconditional files append to `eager`; files with a `paths`
/// frontmatter list append to `scoped` under `base_dir`.
#[allow(clippy::too_many_arguments)]
fn load_rules(
    out: &mut InstructionSet,
    seen: &mut HashSet<PathBuf>,
    rules_dir: &Path,
    base_dir: &Path,
    kind: SourceKind,
    cwd: &Path,
    home: &Option<PathBuf>,
    ctx: &LoadContext,
    excludes: &ExcludeSet,
    externals: &mut Vec<PathBuf>,
) {
    let Ok(canon_dir) = rules_dir.canonicalize() else {
        return;
    };
    if !canon_dir.is_dir() {
        return;
    }
    let mut files = Vec::new();
    collect_markdown(&canon_dir, &mut HashSet::new(), &mut files);
    files.sort();
    for path in files {
        if !seen.insert(path.clone()) || excludes.is_excluded(&path) {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "rule file unreadable");
                continue;
            }
        };
        let (front, body) = crate::frontmatter::split(&raw);
        // No frontmatter (or one without `paths`) means the rule is
        // unconditional; malformed YAML degrades to unconditional rather than
        // dropping the rule on the floor.
        let patterns: Vec<String> = if front.trim().is_empty() {
            Vec::new()
        } else {
            serde_yaml::from_str::<RuleFront>(front)
                .map(|f| f.paths.unwrap_or_default())
                .unwrap_or_else(|e| {
                    tracing::warn!(path = %path.display(), error = %e, "rule frontmatter unparsable; treating as unconditional");
                    Vec::new()
                })
        };
        let content = prepare(&body, &path, cwd, home, ctx, externals);
        let patterns: Vec<String> = patterns
            .into_iter()
            .filter(|p| match globset::Glob::new(p) {
                Ok(_) => true,
                Err(e) => {
                    tracing::warn!(path = %path.display(), pattern = %p, error = %e, "invalid rule glob dropped");
                    false
                }
            })
            .collect();
        if patterns.is_empty() {
            out.eager.push(InstructionSource {
                path,
                kind,
                content,
            });
        } else {
            out.scoped.push(ScopedRule {
                path,
                kind,
                base_dir: base_dir.to_path_buf(),
                patterns,
                content,
            });
        }
    }
}

#[derive(serde::Deserialize)]
struct RuleFront {
    paths: Option<Vec<String>>,
}

/// Recursively collect `.md` files under `dir`, following directory symlinks
/// with a canonical-path visited set so circular links terminate.
fn collect_markdown(dir: &Path, visited: &mut HashSet<PathBuf>, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `Path::is_dir`/`is_file` follow symlinks, which is what we want for
        // linked-in shared rule trees; the canonical visited set breaks loops.
        if path.is_dir() {
            if let Ok(canon) = path.canonicalize()
                && visited.insert(canon)
            {
                collect_markdown(&path, visited, out);
            }
        } else if path.is_file()
            && path.extension().is_some_and(|e| e == "md")
            && let Ok(canon) = path.canonicalize()
        {
            out.push(canon);
        }
    }
}

/// Glob matcher for `claude_md_excludes`. An empty or all-invalid pattern
/// list excludes nothing.
struct ExcludeSet {
    set: Option<globset::GlobSet>,
}

impl ExcludeSet {
    fn compile(patterns: &[String]) -> Self {
        let mut builder = globset::GlobSetBuilder::new();
        let mut any = false;
        for p in patterns {
            match globset::Glob::new(p) {
                Ok(g) => {
                    builder.add(g);
                    any = true;
                }
                Err(e) => {
                    tracing::warn!(pattern = %p, error = %e, "invalid claude_md_excludes glob dropped");
                }
            }
        }
        Self {
            set: any.then(|| builder.build()).transpose().ok().flatten(),
        }
    }

    fn is_excluded(&self, path: &Path) -> bool {
        self.set.as_ref().is_some_and(|s| s.is_match(path))
    }
}

/// Strip block-level `<!-- ... -->` comments outside fenced code blocks.
/// A comment may span lines and several may share one line; fences are not
/// recognized while a comment is open.
fn strip_html_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut fence: Option<(u8, usize)> = None;
    let mut in_comment = false;
    for line in input.split_inclusive('\n') {
        if !in_comment {
            if let Some(marker) = fence_marker(line) {
                match fence {
                    None => fence = Some(marker),
                    Some((ch, len)) if ch == marker.0 && marker.1 >= len => fence = None,
                    _ => {}
                }
                out.push_str(line);
                continue;
            }
            if fence.is_some() {
                out.push_str(line);
                continue;
            }
        }
        let mut rest = line;
        while !rest.is_empty() {
            if in_comment {
                match rest.find("-->") {
                    Some(i) => {
                        in_comment = false;
                        rest = &rest[i + 3..];
                    }
                    None => rest = "",
                }
            } else {
                match rest.find("<!--") {
                    Some(i) => {
                        out.push_str(&rest[..i]);
                        in_comment = true;
                        rest = &rest[i + 4..];
                    }
                    None => {
                        out.push_str(rest);
                        rest = "";
                    }
                }
            }
        }
    }
    out
}

/// The fence opener/closer of a markdown line: `(byte, run length)` for a
/// leading run of 3+ backticks or tildes, else `None`.
fn fence_marker(line: &str) -> Option<(u8, usize)> {
    let bytes = line.trim_start().as_bytes();
    let ch = *bytes.first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let len = bytes.iter().take_while(|&&b| b == ch).count();
    (len >= 3).then_some((ch, len))
}

/// Expand `@path` imports outside fenced code blocks and inline code spans.
/// Each successfully resolved import is replaced by its (recursively
/// expanded) content; anything that cannot be expanded — missing file, cycle,
/// depth limit, withheld external — stays as the literal `@token` text.
#[allow(clippy::too_many_arguments)]
fn expand_imports(
    input: &str,
    file: &Path,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    cwd: &Path,
    home: &Option<PathBuf>,
    ctx: &LoadContext,
    externals: &mut Vec<PathBuf>,
) -> String {
    let file_dir = file.parent().unwrap_or(Path::new("/"));
    let mut out = String::with_capacity(input.len());
    let mut fence: Option<(u8, usize)> = None;
    for line in input.split_inclusive('\n') {
        if let Some(marker) = fence_marker(line) {
            match fence {
                None => fence = Some(marker),
                Some((ch, len)) if ch == marker.0 && marker.1 >= len => fence = None,
                _ => {}
            }
            out.push_str(line);
            continue;
        }
        if fence.is_some() {
            out.push_str(line);
            continue;
        }
        expand_line_imports(
            line, file_dir, depth, visited, cwd, home, ctx, externals, &mut out,
        );
    }
    out
}

/// One non-fence line of import expansion. Inline code spans (backtick runs)
/// are copied verbatim; an `@` token is only honored at the start of the line
/// or after whitespace, so prose like `user@host` is never parsed.
#[allow(clippy::too_many_arguments)]
fn expand_line_imports(
    line: &str,
    file_dir: &Path,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    cwd: &Path,
    home: &Option<PathBuf>,
    ctx: &LoadContext,
    externals: &mut Vec<PathBuf>,
    out: &mut String,
) {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut prev_ws = true;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'`' {
            let run_len = bytes[i..].iter().take_while(|&&c| c == b'`').count();
            let after = i + run_len;
            match find_backtick_run(&bytes[after..], run_len) {
                Some(rel) => {
                    let end = after + rel + run_len;
                    out.push_str(&line[i..end]);
                    prev_ws = false;
                    i = end;
                }
                None => {
                    out.push_str(&line[i..]);
                    return;
                }
            }
        } else if b == b'@' && prev_ws {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
                end += 1;
            }
            let token = &line[start..end];
            match resolve_and_expand(token, file_dir, depth, visited, cwd, home, ctx, externals) {
                Some(content) => out.push_str(&content),
                None => {
                    out.push('@');
                    out.push_str(token);
                }
            }
            prev_ws = false;
            i = end;
        } else {
            // `b` is ASCII or a UTF-8 lead byte; push the whole char.
            let ch_len = line[i..].chars().next().map(char::len_utf8).unwrap_or(1);
            out.push_str(&line[i..i + ch_len]);
            prev_ws = b.is_ascii_whitespace();
            i += ch_len;
        }
    }
}

/// Find a run of exactly `run_len` backticks in `bytes`, returning its offset.
fn find_backtick_run(bytes: &[u8], run_len: usize) -> Option<usize> {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let len = bytes[i..].iter().take_while(|&&c| c == b'`').count();
            if len == run_len {
                return Some(i);
            }
            i += len;
        } else {
            i += 1;
        }
    }
    None
}

/// Resolve one `@token` to expanded content. `None` keeps the literal text.
#[allow(clippy::too_many_arguments)]
fn resolve_and_expand(
    token: &str,
    file_dir: &Path,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    cwd: &Path,
    home: &Option<PathBuf>,
    ctx: &LoadContext,
    externals: &mut Vec<PathBuf>,
) -> Option<String> {
    if token.is_empty() {
        return None;
    }
    if depth >= MAX_IMPORT_DEPTH {
        tracing::warn!(import = %token, "@import beyond max depth {MAX_IMPORT_DEPTH}; left literal");
        return None;
    }
    let candidate = if let Some(rest) = token.strip_prefix("~/") {
        home.as_ref()?.join(rest)
    } else {
        let p = Path::new(token);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            file_dir.join(p)
        }
    };
    let canon = candidate.canonicalize().ok()?;
    if !canon.is_file() {
        return None;
    }
    if !visited.insert(canon.clone()) {
        tracing::warn!(path = %canon.display(), "@import cycle or duplicate; left literal");
        return None;
    }
    let under_cwd = canon.starts_with(cwd);
    // The home-wide subtree is NOT trusted — a repo-authored `@~/.ssh/id_rsa`
    // would otherwise ride the home allowance into the context. Only
    // `~/.claude` (the documented cross-project sharing location) is internal.
    let under_claude_home = home
        .as_ref()
        .is_some_and(|h| canon.starts_with(h.join(".claude")));
    if !under_cwd && !under_claude_home {
        externals.push(canon.clone());
        if !ctx.allow_external {
            return None;
        }
    }
    let raw = std::fs::read_to_string(&canon)
        .map_err(|e| {
            tracing::warn!(path = %canon.display(), error = %e, "@import target unreadable");
        })
        .ok()?;
    let stripped = strip_html_comments(&raw);
    Some(expand_imports(
        &stripped,
        &canon,
        depth + 1,
        visited,
        cwd,
        home,
        ctx,
        externals,
    ))
}

/// Canonicalize, falling back to the input when the path does not resolve
/// (e.g. a cwd that no longer exists) so callers still get a stable base.
fn canonicalize_best_effort(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique temp directory per test, removed on drop. Canonicalized up
    /// front so assertions match the loader's canonical paths on macOS
    /// (where the temp dir sits behind a `/var` → `/private/var` symlink).
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "manox_claude_md_{name}_{}_{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir.canonicalize().unwrap())
        }

        fn write(&self, rel: &str, content: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
            p.canonicalize().unwrap()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ctx_with(home: Option<PathBuf>, managed: Option<PathBuf>) -> LoadContext {
        LoadContext {
            home,
            managed,
            excludes: Vec::new(),
            allow_external: false,
        }
    }

    fn eager_paths(set: &InstructionSet) -> Vec<&Path> {
        set.eager.iter().map(|s| s.path.as_path()).collect()
    }

    #[test]
    fn ancestor_chain_orders_root_to_cwd() {
        let t = TestDir::new("chain");
        let a = t.write("a/CLAUDE.md", "level a");
        let b = t.write("a/b/CLAUDE.md", "level b");
        let c = t.write("a/b/c/CLAUDE.md", "level c");
        let set = load(&t.0.join("a/b/c"), &ctx_with(None, None));
        assert_eq!(
            eager_paths(&set),
            vec![a.as_path(), b.as_path(), c.as_path()]
        );
        assert_eq!(set.eager[0].content.trim(), "level a");
        assert_eq!(set.eager[2].content.trim(), "level c");
    }

    #[test]
    fn within_directory_order_is_shared_dotclaude_rules_then_local() {
        let t = TestDir::new("dirorder");
        let shared = t.write("proj/CLAUDE.md", "shared");
        let dotted = t.write("proj/.claude/CLAUDE.md", "dotclaude");
        let rule = t.write("proj/.claude/rules/style.md", "rule");
        let local = t.write("proj/CLAUDE.local.md", "local");
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        assert_eq!(
            eager_paths(&set),
            vec![
                shared.as_path(),
                dotted.as_path(),
                rule.as_path(),
                local.as_path()
            ]
        );
        assert_eq!(set.eager[2].kind, SourceKind::ProjectRule);
        assert_eq!(set.eager[3].kind, SourceKind::ProjectLocal);
    }

    #[test]
    fn managed_first_then_user_then_project() {
        let t = TestDir::new("layers");
        let managed = t.write("managed/CLAUDE.md", "managed");
        let home = TestDir::new("home");
        let user = home.write(".claude/CLAUDE.md", "user");
        let user_rule = home.write(".claude/rules/prefs.md", "user rule");
        let proj = t.write("proj/CLAUDE.md", "project");
        let set = load(
            &t.0.join("proj"),
            &ctx_with(Some(home.0.clone()), Some(managed.clone())),
        );
        assert_eq!(
            eager_paths(&set),
            vec![
                managed.as_path(),
                user.as_path(),
                user_rule.as_path(),
                proj.as_path()
            ]
        );
        assert_eq!(set.eager[0].kind, SourceKind::ManagedPolicy);
        assert_eq!(set.eager[1].kind, SourceKind::UserGlobal);
        assert_eq!(set.eager[2].kind, SourceKind::UserRule);
    }

    #[test]
    fn same_file_via_two_paths_loads_once() {
        // cwd inside ~/.claude: the user-global file is also an ancestor-chain
        // hit and must appear exactly once.
        let home = TestDir::new("dedup");
        let user = home.write(".claude/CLAUDE.md", "user");
        let set = load(
            &home.0.join(".claude"),
            &ctx_with(Some(home.0.clone()), None),
        );
        assert_eq!(eager_paths(&set), vec![user.as_path()]);
    }

    #[test]
    fn import_expands_relative_to_importing_file() {
        let t = TestDir::new("importrel");
        t.write("proj/docs/guide.md", "guide body");
        // The importing file is an ancestor-chain CLAUDE.md above cwd;
        // `proj/docs/guide.md` resolves against the file's own directory (the
        // tree root), not against cwd — and lands back inside the cwd subtree.
        let top = t.write("CLAUDE.md", "before\n@proj/docs/guide.md\nafter");
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        assert_eq!(eager_paths(&set), vec![top.as_path()]);
        let content = &set.eager[0].content;
        assert!(content.contains("before\nguide body\nafter"), "{content}");
    }

    #[test]
    fn import_supports_home_tilde() {
        let home = TestDir::new("home");
        home.write(".claude/shared/prefs.md", "home prefs");
        let t = TestDir::new("tilde");
        let top = t.write("proj/CLAUDE.md", "@~/.claude/shared/prefs.md");
        let set = load(&t.0.join("proj"), &ctx_with(Some(home.0.clone()), None));
        assert_eq!(eager_paths(&set), vec![top.as_path()]);
        assert!(set.eager[0].content.contains("home prefs"));
    }

    #[test]
    fn import_chain_depth_boundary_exact() {
        // f1 ← f2 ← f3 ← f4 ← top: four hops from the top file, all included;
        // f5 would be a fifth hop and stays literal.
        let t = TestDir::new("depthx");
        t.write("proj/f5.md", "FIVE");
        t.write("proj/f4.md", "FOUR\n@f5.md");
        t.write("proj/f3.md", "THREE\n@f4.md");
        t.write("proj/f2.md", "TWO\n@f3.md");
        t.write("proj/f1.md", "ONE\n@f2.md");
        t.write("proj/CLAUDE.md", "TOP\n@f1.md");
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        let content = &set.eager[0].content;
        for expected in ["ONE", "TWO", "THREE", "FOUR"] {
            assert!(content.contains(expected), "missing {expected}: {content}");
        }
        assert!(
            !content.contains("FIVE"),
            "fifth hop must stay literal: {content}"
        );
        assert!(content.contains("@f5.md"), "literal token kept: {content}");
    }

    #[test]
    fn import_cycle_terminates() {
        let t = TestDir::new("cycle");
        t.write("proj/a.md", "A\n@b.md");
        t.write("proj/b.md", "B\n@a.md");
        t.write("proj/CLAUDE.md", "TOP\n@a.md");
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        let content = &set.eager[0].content;
        assert!(content.contains('A'), "{content}");
        assert!(content.contains('B'), "{content}");
        // The second encounter of a.md stays literal rather than looping.
        assert!(content.contains("@a.md"), "{content}");
    }

    #[test]
    fn import_ignores_code_fences_and_spans() {
        let t = TestDir::new("codeskip");
        t.write("proj/real.md", "REAL");
        let top = t.write(
            "proj/CLAUDE.md",
            "pre\n```\n@real.md in fence\n```\n`@real.md in span`\n@real.md\npost",
        );
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        let content = &set.eager[0].content;
        assert!(content.contains("@real.md in fence"), "{content}");
        assert!(content.contains("`@real.md in span`"), "{content}");
        assert_eq!(content.matches("REAL").count(), 1, "{content}");
        assert_eq!(eager_paths(&set), vec![top.as_path()]);
    }

    #[test]
    fn import_missing_file_stays_literal() {
        let t = TestDir::new("missing");
        t.write("proj/CLAUDE.md", "keep @nope/missing.md literal");
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        assert!(set.eager[0].content.contains("@nope/missing.md"));
    }

    #[test]
    fn html_comments_stripped_outside_fences_only() {
        let t = TestDir::new("comments");
        t.write(
            "proj/CLAUDE.md",
            "alpha <!-- gone --> beta\n<!--\nmulti-line\ngone\n-->\n```\n<!-- kept -->\n```\nomega",
        );
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        let content = &set.eager[0].content;
        assert!(content.contains("alpha  beta"), "{content}");
        assert!(!content.contains("multi-line"), "{content}");
        assert!(content.contains("<!-- kept -->"), "{content}");
        assert!(content.contains("omega"), "{content}");
    }

    #[test]
    fn rules_without_paths_are_eager_rules_with_paths_are_scoped() {
        let t = TestDir::new("rules");
        let eager = t.write("proj/.claude/rules/style.md", "always on");
        let scoped = t.write(
            "proj/.claude/rules/rust.md",
            "---\npaths:\n  - \"src/**/*.rs\"\n---\nrust only",
        );
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        assert_eq!(eager_paths(&set), vec![eager.as_path()]);
        assert_eq!(set.scoped.len(), 1);
        assert_eq!(set.scoped[0].path, scoped);
        assert_eq!(set.scoped[0].base_dir, t.0.join("proj"));
        assert_eq!(set.scoped[0].patterns, vec!["src/**/*.rs".to_string()]);
        assert!(set.scoped[0].content.contains("rust only"));
    }

    #[test]
    fn rules_discovered_recursively_and_sorted() {
        let t = TestDir::new("rulesrec");
        let b = t.write("proj/.claude/rules/b.md", "b");
        let a = t.write("proj/.claude/rules/sub/a.md", "a");
        t.write("proj/.claude/rules/ignore.txt", "not markdown");
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        assert_eq!(eager_paths(&set), vec![b.as_path(), a.as_path()]);
    }

    #[test]
    fn invalid_rule_glob_dropped_others_kept() {
        let t = TestDir::new("badglob");
        t.write(
            "proj/.claude/rules/x.md",
            "---\npaths:\n  - \"photos [2024/**\"\n  - \"src/**\"\n---\nbody",
        );
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        assert_eq!(set.scoped.len(), 1);
        assert_eq!(set.scoped[0].patterns, vec!["src/**".to_string()]);
    }

    #[test]
    fn excludes_skip_matching_files_but_never_managed() {
        let t = TestDir::new("excludes");
        let managed = t.write("managed/CLAUDE.md", "managed");
        let excluded = t.0.join("proj/CLAUDE.md");
        t.write("proj/CLAUDE.md", "excluded");
        let kept = t.write("proj/CLAUDE.local.md", "kept");
        let ctx = LoadContext {
            home: None,
            managed: Some(managed.clone()),
            excludes: vec![excluded.display().to_string()],
            allow_external: false,
        };
        let set = load(&t.0.join("proj"), &ctx);
        assert_eq!(
            eager_paths(&set),
            vec![managed.as_path(), kept.as_path()],
            "managed exempt, project file excluded, local kept"
        );
    }

    #[test]
    fn external_imports_reported_and_withheld_until_allowed() {
        let t = TestDir::new("external");
        let outside = TestDir::new("outside");
        let secret = outside.write("secret.md", "SECRET");
        t.write("proj/CLAUDE.md", &format!("@{}", secret.display()));

        let denied = load(&t.0.join("proj"), &ctx_with(None, None));
        assert_eq!(denied.external_imports, vec![secret.clone()]);
        assert!(
            denied.eager[0]
                .content
                .contains(&format!("@{}", secret.display()))
        );
        assert!(!denied.eager[0].content.contains("SECRET"));

        let home = TestDir::new("home2");
        let mut ctx = ctx_with(Some(home.0.clone()), None);
        ctx.allow_external = true;
        let allowed = load(&t.0.join("proj"), &ctx);
        assert_eq!(allowed.external_imports, vec![secret.clone()]);
        assert!(allowed.eager[0].content.contains("SECRET"));
    }

    #[test]
    fn relative_import_outside_cwd_is_external() {
        let t = TestDir::new("relescape");
        let shared = t.write("shared.md", "SHARED");
        t.write("proj/CLAUDE.md", "@../shared.md");
        // cwd = `proj`: `../shared.md` lands at the tree root, outside the cwd
        // subtree and outside `~/.claude` → external, withheld.
        let denied = load(&t.0.join("proj"), &ctx_with(None, None));
        assert_eq!(denied.external_imports, vec![shared.clone()]);
        assert!(!denied.eager[0].content.contains("SHARED"));
        // The same `../` shape with a target back inside cwd stays internal.
        let t2 = TestDir::new("relinternal");
        t2.write("proj/shared.md", "INSIDE");
        t2.write("proj/CLAUDE.md", "@../proj/shared.md");
        let internal = load(&t2.0.join("proj"), &ctx_with(None, None));
        assert!(internal.external_imports.is_empty());
        assert!(internal.eager[0].content.contains("INSIDE"));
    }

    #[test]
    fn import_under_claude_home_is_not_external_but_home_is() {
        let home = TestDir::new("home3");
        let shared = home.write(".claude/shared/x.md", "SHARED");
        let t = TestDir::new("underhome");
        let top = t.write("proj/CLAUDE.md", &format!("@{}", shared.display()));
        let set = load(&t.0.join("proj"), &ctx_with(Some(home.0.clone()), None));
        assert!(set.external_imports.is_empty());
        assert!(set.eager[0].content.contains("SHARED"));
        assert_eq!(eager_paths(&set), vec![top.as_path()]);

        // Elsewhere under home (not `~/.claude`) is external — the
        // `@~/.ssh/id_rsa` exfiltration shape stays behind the approval gate.
        let dotfile = home.write(".ssh/id_rsa", "KEY");
        t.write("proj/CLAUDE.local.md", &format!("@{}", dotfile.display()));
        let set = load(&t.0.join("proj"), &ctx_with(Some(home.0.clone()), None));
        assert_eq!(set.external_imports, vec![dotfile.clone()]);
        assert!(!set.eager[1].content.contains("KEY"));
    }

    #[test]
    fn render_eager_none_when_empty() {
        let t = TestDir::new("empty");
        let set = load(&t.0, &ctx_with(None, None));
        assert!(render_eager(&set, crate::language::Language::En).is_none());
    }

    #[test]
    fn render_eager_tags_each_source_in_order() {
        let t = TestDir::new("render");
        t.write("proj/CLAUDE.md", "shared body");
        t.write("proj/CLAUDE.local.md", "local body");
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        let text = render_eager(&set, crate::language::Language::En).expect("non-empty render");
        let shared_ix = text.find("shared body").unwrap();
        let local_ix = text.find("local body").unwrap();
        assert!(shared_ix < local_ix, "{text}");
        assert!(text.contains("scope=\"project\""), "{text}");
        assert!(text.contains("scope=\"project-local\""), "{text}");
        assert!(text.contains("<instructions"), "{text}");
        assert!(text.contains("</instructions>"), "{text}");
        // Stable across renders of the same set (prefix-cache requirement).
        assert_eq!(
            text,
            render_eager(&set, crate::language::Language::En).unwrap()
        );
    }

    #[test]
    fn brace_expansion_glob_is_valid() {
        let t = TestDir::new("brace");
        t.write(
            "proj/.claude/rules/web.md",
            "---\npaths:\n  - \"src/**/*.{ts,tsx}\"\n---\nweb rules",
        );
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        assert_eq!(set.scoped.len(), 1);
        assert_eq!(
            set.scoped[0].patterns,
            vec!["src/**/*.{ts,tsx}".to_string()]
        );
    }

    #[test]
    fn discover_nested_chain_top_down() {
        let t = TestDir::new("lazychain");
        let a = t.write("proj/sub/CLAUDE.md", "sub rule");
        let b = t.write("proj/sub/deep/CLAUDE.md", "deep rule");
        let local = t.write("proj/sub/deep/CLAUDE.local.md", "deep local");
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        let found = discover_for_read(
            &t.0.join("proj/sub/deep/f.rs"),
            &t.0.join("proj"),
            &set.scoped,
            &HashSet::new(),
            &ctx_with(None, None),
        );
        let paths: Vec<&Path> = found.iter().map(|s| s.path.as_path()).collect();
        assert_eq!(paths, vec![a.as_path(), b.as_path(), local.as_path()]);
        assert_eq!(found[0].kind, SourceKind::Project);
        assert_eq!(found[2].kind, SourceKind::ProjectLocal);
        assert!(found[0].content.contains("sub rule"));
        assert!(found[2].content.contains("deep local"));
    }

    #[test]
    fn discover_skips_already_injected_and_files_beside_cwd() {
        let t = TestDir::new("lazydedup");
        let sub = t.write("proj/sub/CLAUDE.md", "sub rule");
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        // Already injected → nothing returned.
        let already = HashSet::from([sub.clone()]);
        let found = discover_for_read(
            &t.0.join("proj/sub/f.rs"),
            &t.0.join("proj"),
            &set.scoped,
            &already,
            &ctx_with(None, None),
        );
        assert!(found.is_empty());
        // Reading a file directly in cwd discovers nothing (no dirs between).
        let found = discover_for_read(
            &t.0.join("proj/f.rs"),
            &t.0.join("proj"),
            &set.scoped,
            &HashSet::new(),
            &ctx_with(None, None),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn discover_matches_scoped_rule_by_glob() {
        let t = TestDir::new("lazyscoped");
        let rule = t.write(
            "proj/.claude/rules/rust.md",
            "---\npaths:\n  - \"src/**/*.rs\"\n---\nrust rule",
        );
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        assert_eq!(set.scoped.len(), 1);

        let found = discover_for_read(
            &t.0.join("proj/src/main.rs"),
            &t.0.join("proj"),
            &set.scoped,
            &HashSet::new(),
            &ctx_with(None, None),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, rule);
        assert_eq!(found[0].kind, SourceKind::ProjectRule);
        assert!(found[0].content.contains("rust rule"));

        // Non-matching path → nothing.
        let found = discover_for_read(
            &t.0.join("proj/README.md"),
            &t.0.join("proj"),
            &set.scoped,
            &HashSet::new(),
            &ctx_with(None, None),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn discover_scoped_rule_matches_outside_cwd_via_base_dir() {
        // A user-level scoped rule still fires for a read outside cwd when the
        // path sits under the rule's base dir (home).
        let home = TestDir::new("lazyhome");
        let rule = home.write(
            ".claude/rules/dotfiles.md",
            "---\npaths:\n  - \".claude/**/*.md\"\n---\ndotfile rule",
        );
        let proj = TestDir::new("lazyproj");
        let set = load(&proj.0, &ctx_with(Some(home.0.clone()), None));
        assert_eq!(set.scoped.len(), 1);
        let found = discover_for_read(
            &home.0.join(".claude/skills/x/SKILL.md"),
            &proj.0,
            &set.scoped,
            &HashSet::new(),
            &ctx_with(Some(home.0.clone()), None),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, rule);
        assert_eq!(found[0].kind, SourceKind::UserRule);
    }

    #[test]
    fn discover_applies_excludes_to_nested_files() {
        let t = TestDir::new("lazyexcl");
        let excluded = t.write("proj/sub/CLAUDE.md", "excluded");
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        let ctx = LoadContext {
            excludes: vec![excluded.display().to_string()],
            ..ctx_with(None, None)
        };
        let found = discover_for_read(
            &t.0.join("proj/sub/f.rs"),
            &t.0.join("proj"),
            &set.scoped,
            &HashSet::new(),
            &ctx,
        );
        assert!(found.is_empty());
    }

    #[test]
    fn render_lazy_none_when_empty_and_reminder_when_present() {
        assert!(render_lazy(&[], crate::language::Language::En).is_none());
        let t = TestDir::new("lazyrender");
        t.write("proj/sub/CLAUDE.md", "sub body");
        let set = load(&t.0.join("proj"), &ctx_with(None, None));
        let found = discover_for_read(
            &t.0.join("proj/sub/f.rs"),
            &t.0.join("proj"),
            &set.scoped,
            &HashSet::new(),
            &ctx_with(None, None),
        );
        let text = render_lazy(&found, crate::language::Language::En).expect("non-empty render");
        assert!(text.starts_with("<system-reminder>"), "{text}");
        assert!(text.ends_with("</system-reminder>\n"), "{text}");
        assert!(text.contains("sub body"), "{text}");
        assert!(text.contains("scope=\"project\""), "{text}");
        // Deterministic across renders (history byte-stability).
        assert_eq!(
            text,
            render_lazy(&found, crate::language::Language::En).unwrap()
        );
    }
}
