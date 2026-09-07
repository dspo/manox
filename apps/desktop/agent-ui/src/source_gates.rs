//! Structural ratchet gates (§J.6 groundwork; migration items U1/U2/U3/U6/U9).
//!
//! The desktop's kernel-bypass surface is frozen at its audited count: these
//! gates can only ever be loosened by REMOVING bypass sites and ratcheting
//! the budget down, never by adding sites. Each pattern family names the
//! migration that retires it:
//! - store mirror writes (`with_mut(|s|`) — U3 single-writer: the server
//!   pump owns the thread-store flags; the desktop mirror writes are
//!   redundant in-proc and race the pump across processes.
//! - facade writes (`with_mut(|t|`) — U1/U6: user intent goes through the
//!   gateway (`ClientCall::Submit` etc.); rendering state comes from the
//!   client store, not a locally driven kernel facade.
//! - store reads (`thread_store::global()` / `thread_store_global()`) — U2:
//!   lists and summaries come from `ListThreads` + host events.
//! - protocol sends outside the gateway client — U9 layering: views talk to
//!   the multiplexer, never to the wire directly.
//!
//! Counts are production-side only: each file is truncated at its first
//! `#[cfg(test)]` marker before counting.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    fn production_part(source: &str) -> &str {
        match source.find("#[cfg(test)]") {
            Some(idx) => &source[..idx],
            None => source,
        }
    }

    fn prod_count(src_dir: &Path, file: &str, needles: &[&str]) -> usize {
        let path = src_dir.join(file);
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("gate cannot read {}: {e}", path.display()));
        let prod = production_part(&source);
        needles.iter().map(|n| prod.matches(n).count()).sum()
    }

    /// The frozen budget. Lowering a number here must accompany the commit
    /// that removes the sites; raising one is a regression (the assert
    /// message names the migration item to do instead).
    #[test]
    fn desktop_bypass_surface_never_grows() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        const STORE_WRITE: &[&str] = &["with_mut(|s|"];
        const FACADE_WRITE: &[&str] = &["with_mut(|t|"];
        const STORE_GLOBAL: &[&str] = &["thread_store::global()", "thread_store_global()"];
        const SENDS: &[&str] = &[".send_call(", ".send_note("];
        // (file, pattern-family name, needles, frozen count)
        let budget: &[(&str, &str, &[&str], usize)] = &[
            // U3a: nine redundant mirror writes removed (the server pump is the
            // single writer in-proc); the residual budget is sanctioned
            // debt — error-arm mark_idle gaps, verdict-time clears,
            // user-action writes awaiting gateway calls (U3b).
            ("workspace.rs", "store mirror writes (U3)", STORE_WRITE, 18),
            ("workspace.rs", "facade writes (U1/U6)", FACADE_WRITE, 8),
            ("workspace.rs", "store reads (U2)", STORE_GLOBAL, 30),
            ("workspace.rs", "protocol sends (U9)", SENDS, 18),
            // U3/GW5: retired — the SessionStatus store-mirror block was
            // the multiplexer's only write site.
            ("multiplexer.rs", "store mirror writes (U3)", STORE_WRITE, 0),
            // The multiplexer IS the gateway client: its sends are the
            // sanctioned wire surface and stay unbudgeted here, but they
            // must never spread to other files (checked below).
            ("slash_command.rs", "protocol sends (controller)", SENDS, 9),
            ("views/sidebar.rs", "store reads (U2)", STORE_GLOBAL, 1),
        ];
        for (file, family, needles, max) in budget {
            let got = prod_count(&src, file, needles);
            assert!(
                got <= *max,
                "{file}: production `{family}` count {got} exceeds the frozen budget {max}. \
                 This surface only shrinks — do the migration instead of adding a site, \
                 and ratchet the budget down when you remove one."
            );
        }
    }

    /// Views (and every file outside the sanctioned senders) never touch the
    /// wire or the kernel store: the god-object exemption list is explicit
    /// and finite.
    #[test]
    fn no_new_files_touch_wire_or_store() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // Files with a budgeted protocol-send or store surface (this gate's
        // table above). Everything else must be clean.
        let exempt: &[&str] = &[
            "workspace.rs",
            "multiplexer.rs",
            "slash_command.rs",
            "views/sidebar.rs",
            "source_gates.rs",
            // The gateway client half legitimately constructs wire frames.
            "client_store_handle.rs",
        ];
        let mut offenders: Vec<String> = Vec::new();
        collect_rs_files(&src, &src, &mut offenders, exempt);
        assert!(
            offenders.is_empty(),
            "these files gained a wire-send or thread-store surface (views must not; \
             §J.6 / U9): {offenders:?}"
        );
    }

    fn collect_rs_files(root: &Path, dir: &Path, offenders: &mut Vec<String>, exempt: &[&str]) {
        for entry in fs::read_dir(dir).expect("src dir readable") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                collect_rs_files(root, &path, offenders, exempt);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if exempt.contains(&rel.as_str()) {
                continue;
            }
            let source = fs::read_to_string(&path).expect("rs file readable");
            let prod = production_part(&source);
            const NEEDLES: &[&str] = &[
                ".send_call(",
                ".send_note(",
                "thread_store::global()",
                "thread_store_global()",
                "with_mut(|s|",
                "with_mut(|t|",
            ];
            if NEEDLES.iter().any(|n| prod.contains(n)) {
                offenders.push(rel);
            }
        }
    }
}
