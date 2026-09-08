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
//!   lists and summaries come from `ListThreads` + host events. Landed for
//!   the list/registry surface (the sidebar reads the multiplexer's wire
//!   rows; its budget is 0); the workspace residue is itemized in the
//!   budget table below.
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
            // U3a+U3b: the redundant mirror writes are removed (the server
            // pump is the single writer in-proc and its §D.5 deltas fold
            // into the multiplexer's wire rows). The residual budget is
            // sanctioned debt — user-action writes awaiting gateway calls
            // (Archive/SetTag/RemoveProject, the park seeds, the attach
            // badge clear, archive-if-idle, ExecuteFresh's archive,
            // register_project), the dual-track bridge handle, and the
            // registry-push decoration block.
            // U6b②: the two attach-face store reads retired (open_thread's
            // load_thread + attach_created_session's live/load pair) — the
            // attach path is the landing mirror now.
            // U6b⑤: the park-seed store writes retired (mark_running/
            // mark_background_work — the U3b server pump is the single
            // flag writer; its deltas already fed every mirror).
            ("workspace.rs", "store mirror writes (U3)", STORE_WRITE, 7),
            // U1-flush: the parked flush's two facade writes (insert_user_message
            // + run_turn) retired to the gateway wire.
            // U6b③: the two construction parks retired (both create flows
            // ride the v2 CreateSession intent now) — residual: the two
            // browser-suite landing fallbacks (the designed landing-park
            // path, replayed by ensure_engine on materialization).
            ("workspace.rs", "facade writes (U1/U6)", FACADE_WRITE, 2),
            // U2: the list/registry reads are retired — the sidebar renders
            // the multiplexer's wire rows and the chip menu reads the pushed
            // decoration cache. The residual 21 are sanctioned debt: the 16
            // write-site acquisitions above (U3b), the attach-surface thread
            // load (U6), the three right-pane threads.db persistence reads
            // (desktop-local UI state), and the one dual-track bridge
            // acquisition whose rescan pump pushes the decoration columns the
            // wire list does not carry yet and re-pulls the list through the
            // gateway.
            // U3b: the seven mirror-write blocks took their
            // thread_store_global() acquisitions with them (21 - 7).
            // U6b②: the bridge-head binding (U6a) and attach_created_session's
            // global() retired with the attach-face reads.
            ("workspace.rs", "store reads (U2)", STORE_GLOBAL, 10),
            // 21 = 18 + the GW3 CancelDelivery withdrawal send + the two
            // U1-flush parked-wire sends (the AppendUserMessage note + the
            // v2 Submit — protocol surfaces the migrations add by design;
            // U9 folds them into the multiplexer with the rest).
            // U6b① grew this 21→23 by design: the browser-suite toggles
            // migrated FROM the FACADE_WRITE bypass surface TO the protocol
            // (the ledger-mandated direction — a bypass write became two
            // wire sends; their landing-fallback keeps the facade needle,
            // so FACADE_WRITE holds at 5).
            // U6b③ grew this 23→24: the ExecuteFresh seed turn's
            // PlanSeedExecution note (a bypass facade-seed became a wire
            // send — the ledger-mandated direction; the budget missed the
            // raise in that commit and is corrected here).
            ("workspace.rs", "protocol sends (U9)", SENDS, 24),
            // U3/GW5: retired — the SessionStatus store-mirror block was
            // the multiplexer's only write site.
            ("multiplexer.rs", "store mirror writes (U3)", STORE_WRITE, 0),
            // The multiplexer IS the gateway client: its sends are the
            // sanctioned wire surface and stay unbudgeted here, but they
            // must never spread to other files (checked below).
            ("slash_command.rs", "protocol sends (controller)", SENDS, 9),
            // U2: retired — the sidebar's rows come from the multiplexer's
            // wire list and its decoration from the workspace push; the
            // store acquisition and event pump are gone.
            ("views/sidebar.rs", "store reads (U2)", STORE_GLOBAL, 0),
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
            "source_gates.rs",
            // The gateway client half legitimately constructs wire frames.
            "client_store_handle.rs",
            // U9a: the extracted workspace test module is test code in its
            // entirety (it compiles only under the parent's `#[cfg(test)]
            // mod tests;` declaration, so it carries no inner marker for
            // `production_part` to truncate at).
            "workspace/tests.rs",
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

    /// U9 / K.7-6 terminal grep gate: the view/component layer never
    /// touches the protocol send surface (the multiplexer is the only
    /// wire face) and never holds kernel object handles. The frozen
    /// kernel-handle budget is context_rail's `ThreadHandle` (doc line +
    /// field + ctor) — the U7b rail-visibility migration item (Q-face
    /// state ownership); it only shrinks.
    #[test]
    fn views_never_touch_wire_or_kernel_handles() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("views");
        const SENDS: &[&str] = &[".send_call(", ".send_note("];
        const KERNEL: &[&str] = &["ThreadHandle"];
        let mut send_total = 0;
        let mut kernel_total = 0;
        for entry in fs::read_dir(&dir).expect("views dir readable") {
            let path = entry.expect("dir entry").path();
            if path.extension().map(|e| e != "rs").unwrap_or(true) {
                continue;
            }
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("gate cannot read {}: {e}", path.display()));
            send_total += SENDS
                .iter()
                .map(|n| source.matches(n).count())
                .sum::<usize>();
            kernel_total += KERNEL
                .iter()
                .map(|n| source.matches(n).count())
                .sum::<usize>();
        }
        assert_eq!(
            send_total, 0,
            "views must never touch the protocol send surface (U9 gate — the multiplexer is the only wire face)"
        );
        assert!(
            kernel_total <= 3,
            "the views kernel-handle surface only shrinks (frozen budget 3 = context_rail's ThreadHandle doc/field/ctor, the U7b rail-visibility item); found {kernel_total}"
        );
    }
}
