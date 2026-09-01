/// Gate: core/ must never import from ext/ — the former crate boundary.
#[test]
fn core_never_imports_ext() {
    let core_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/core");
    let mut ok = true;
    fn walk(dir: &std::path::Path, ok: &mut bool) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, ok);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && let Ok(content) = std::fs::read_to_string(&path)
                    && content.contains("crate::ext")
                {
                    eprintln!("VIOLATION: {} imports crate::ext", path.display());
                    *ok = false;
                }
            }
        }
    }
    walk(&core_dir, &mut ok);
    assert!(ok, "core/ modules must not import crate::ext");
}
