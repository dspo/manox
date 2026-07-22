//! Data-driven table of supported language servers. Adding a language is one
//! `LspServerSpec` row — the routing, detection, and spawn paths all key off
//! these fields.

/// A language server manox can drive. Every field is static data; no behavior.
///
/// - `detect`: command whose exit status tells whether the server binary is on
///   `PATH`.
/// - `probe`: command that proves the resolved executable can actually run.
///   This catches shims such as a rustup proxy whose component is not installed.
/// - `spawn`: argv for the stdio server (pyright / ts-language-server need
///   `--stdio` to enter LSP mode; rust-analyzer / gopls are stdio by default).
/// - `extensions`: file extensions routed to this server. `GoToDefinition`
///   etc. pick the server by the path's extension.
/// - `root_hints`: markers used to locate the workspace root (walk up from the
///   target file until one of these exists).
pub struct LspServerSpec {
    pub id: &'static str,
    pub detect: &'static [&'static str],
    pub probe: &'static [&'static str],
    pub spawn: &'static [&'static str],
    pub language_id: &'static str,
    pub extensions: &'static [&'static str],
    pub root_hints: &'static [&'static str],
}

impl Clone for LspServerSpec {
    fn clone(&self) -> Self {
        *self
    }
}
impl Copy for LspServerSpec {}

impl LspServerSpec {
    pub fn handles(&self, ext: &str) -> bool {
        self.extensions.contains(&ext)
    }
}

/// The four first-class servers. Order is stable for deterministic routing
/// when (hypothetically) two servers share an extension — none do today.
pub const SPECS: &[LspServerSpec] = &[
    LspServerSpec {
        id: "rust-analyzer",
        detect: &["which", "rust-analyzer"],
        probe: &["rust-analyzer", "--version"],
        spawn: &["rust-analyzer"],
        language_id: "rust",
        extensions: &["rs"],
        root_hints: &["Cargo.toml"],
    },
    LspServerSpec {
        id: "gopls",
        detect: &["which", "gopls"],
        probe: &["gopls", "version"],
        spawn: &["gopls"],
        language_id: "go",
        extensions: &["go"],
        root_hints: &["go.mod"],
    },
    LspServerSpec {
        id: "pyright",
        detect: &["which", "pyright-langserver"],
        probe: &["pyright-langserver", "--version"],
        // pyright's LSP entry is `pyright-langserver`, not the `pyright` CLI.
        spawn: &["pyright-langserver", "--stdio"],
        language_id: "python",
        extensions: &["py"],
        root_hints: &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
        ],
    },
    LspServerSpec {
        id: "typescript-language-server",
        detect: &["which", "typescript-language-server"],
        probe: &["typescript-language-server", "--version"],
        spawn: &["typescript-language-server", "--stdio"],
        language_id: "typescript",
        extensions: &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
        root_hints: &["tsconfig.json", "package.json", "jsconfig.json"],
    },
];

/// Route a file extension to its server, if any.
pub fn spec_for_extension(ext: &str) -> Option<&'static LspServerSpec> {
    SPECS.iter().find(|s| s.handles(ext))
}

/// Look up a server by its `id`.
pub fn spec_for_id(id: &str) -> Option<&'static LspServerSpec> {
    SPECS.iter().find(|s| s.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_known_extensions() {
        assert_eq!(spec_for_extension("rs").unwrap().id, "rust-analyzer");
        assert_eq!(spec_for_extension("go").unwrap().id, "gopls");
        assert_eq!(spec_for_extension("py").unwrap().id, "pyright");
        assert_eq!(
            spec_for_extension("ts").unwrap().id,
            "typescript-language-server"
        );
        assert_eq!(
            spec_for_extension("tsx").unwrap().id,
            "typescript-language-server"
        );
    }

    #[test]
    fn unknown_extension_routes_to_none() {
        assert!(spec_for_extension("md").is_none());
        assert!(spec_for_extension("").is_none());
    }

    #[test]
    fn spec_for_id_round_trips() {
        for s in SPECS {
            assert_eq!(spec_for_id(s.id).map(|q| q.id), Some(s.id));
        }
        assert!(spec_for_id("nope").is_none());
    }
}
