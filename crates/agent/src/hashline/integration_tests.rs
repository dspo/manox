//! Integration tests covering the read → edit → re-edit round-trip end to end,
//! exercising the global snapshot store and the parse/apply/recover pipeline
//! together (without a gpui `App`, which the unit-test harness cannot easily
//! spin up).

use std::path::{Path, PathBuf};

use super::snapshot::SnapshotStore;
use super::{apply, compute_tag, format_numbered, normalize_to_lf, parse_patch, try_recover};

/// A throwaway file in the OS temp dir with a unique-ish name per process.
fn tmp_file(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("manox-hashline-test-{}-{name}", std::process::id()));
    p
}

fn write_file(path: &Path, content: &str) {
    std::fs::write(path, content.as_bytes()).unwrap();
}

/// Simulate `read_file`: normalize, record a snapshot, format numbered output.
fn read(store: &mut SnapshotStore, path: &Path) -> (String, String) {
    let raw = std::fs::read_to_string(path).unwrap();
    let text = normalize_to_lf(&raw);
    let snap = store.record(path, &text);
    (
        format_numbered(&path.display().to_string(), &text, &snap.tag),
        snap.tag,
    )
}

/// Simulate `edit_file`: parse the patch, validate the tag, apply (or recover),
/// write the result, and record the new snapshot. Returns the new tag.
fn edit(store: &mut SnapshotStore, path: &Path, patch: &str) -> (String, String) {
    let fp = parse_patch(patch).unwrap().pop().unwrap();
    assert_eq!(fp.path, path);
    let raw = std::fs::read_to_string(path).unwrap();
    let current = normalize_to_lf(&raw);
    let current_tag = compute_tag(&current);
    let new_text = if current_tag == fp.tag {
        apply(&current, &fp.ops).unwrap().text
    } else {
        try_recover(&current, &fp.tag, &fp.ops, store, path).unwrap()
    };
    std::fs::write(path, new_text.as_bytes()).unwrap();
    let new_snap = store.record(path, &new_text);
    (new_text, new_snap.tag)
}

#[test]
fn read_edit_roundtrip_swaps_line() {
    let path = tmp_file("roundtrip.rs");
    write_file(&path, "fn main() {\n    println!(\"hi\");\n}\n");
    let mut store = SnapshotStore::new();

    let (_, tag) = read(&mut store, &path);
    let patch = format!(
        "[{}#{}]\nSWAP 2.=2:\n+    println!(\"hello\");",
        path.display(),
        tag
    );
    let (new_text, new_tag) = edit(&mut store, &path, &patch);
    assert_eq!(new_text, "fn main() {\n    println!(\"hello\");\n}");
    assert_ne!(new_tag, tag);

    // Chain a second edit on the fresh tag.
    let patch2 = format!("[{}#{}]\nINS.TAIL:\n+main();", path.display(), new_tag);
    let (new_text2, _) = edit(&mut store, &path, &patch2);
    assert!(new_text2.ends_with("}\nmain();"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn stale_tag_recovered_when_target_unchanged() {
    let path = tmp_file("stale.rs");
    write_file(&path, "fn a() {\n    x();\n}\n");
    let mut store = SnapshotStore::new();

    let (_, tag) = read(&mut store, &path);
    // External edit: prepend an unrelated header line, shifting `x();` down.
    write_file(&path, "// header\nfn a() {\n    x();\n}\n");

    let patch = format!("[{}#{}]\nSWAP 2.=2:\n+    y();", path.display(), tag);
    let (new_text, _) = edit(&mut store, &path, &patch);
    assert_eq!(new_text, "// header\nfn a() {\n    y();\n}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn delete_via_block_op() {
    let path = tmp_file("delblk.rs");
    write_file(&path, "fn a() {\n    x();\n}\nfn b() {}\n");
    let mut store = SnapshotStore::new();

    let (_, tag) = read(&mut store, &path);
    let patch = format!("[{}#{}]\nDEL.BLK 1", path.display(), tag);
    let (new_text, _) = edit(&mut store, &path, &patch);
    assert_eq!(new_text, "fn b() {}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn insert_after_block() {
    let path = tmp_file("insblkpost.rs");
    write_file(&path, "fn a() {\n    x();\n}\nfn b() {}\n");
    let mut store = SnapshotStore::new();

    let (_, tag) = read(&mut store, &path);
    let patch = format!("[{}#{}]\nINS.BLK.POST 1:\n+// done", path.display(), tag);
    let (new_text, _) = edit(&mut store, &path, &patch);
    assert_eq!(new_text, "fn a() {\n    x();\n}\n// done\nfn b() {}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn multiple_hunks_in_one_patch() {
    let path = tmp_file("multi.rs");
    write_file(&path, "A\nB\nC\nD\nE\n");
    let mut store = SnapshotStore::new();

    let (_, tag) = read(&mut store, &path);
    let patch = format!(
        "[{}#{}]\nSWAP 1.=1:\n+X\nSWAP 5.=5:\n+Y",
        path.display(),
        tag
    );
    let (new_text, _) = edit(&mut store, &path, &patch);
    assert_eq!(new_text, "X\nB\nC\nD\nY");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn boundary_repair_drops_echoed_context() {
    let path = tmp_file("echo.rs");
    write_file(&path, "fn a() {\n    x();\n}\n");
    let mut store = SnapshotStore::new();

    let (_, tag) = read(&mut store, &path);
    // Body echoes `}` (the line below the range) — should be auto-dropped.
    let patch = format!("[{}#{}]\nSWAP 2.=2:\n+    y();\n+}}", path.display(), tag);
    let (new_text, _) = edit(&mut store, &path, &patch);
    assert_eq!(new_text, "fn a() {\n    y();\n}");
    let _ = std::fs::remove_file(&path);
}
