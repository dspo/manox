//! Integration tests covering the read → edit → re-edit round-trip end to end,
//! exercising a caller-owned snapshot store and the parse/apply/recover pipeline
//! together against real files in a temp dir.

use std::path::{Path, PathBuf};

use super::snapshot::SnapshotStore;
use super::{apply, compute_tag, format_numbered, normalize_to_lf, parse_patch, try_recover};

fn write_file(path: &Path, content: &str) {
    std::fs::write(path, content.as_bytes()).unwrap();
}

/// Simulate `read`: normalize, record a snapshot, format numbered output.
fn read(store: &mut SnapshotStore, path: &Path) -> (String, String) {
    let raw = std::fs::read_to_string(path).unwrap();
    let text = normalize_to_lf(&raw);
    let snap = store.record(path, &text);
    (
        format_numbered(&path.display().to_string(), &text, &snap.tag),
        snap.tag,
    )
}

/// Simulate `edit`: parse the patch, validate the tag, apply (or recover),
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

fn tmp_file(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    dir.path().join(name)
}

#[test]
fn read_edit_roundtrip_swaps_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = tmp_file(&dir, "roundtrip.rs");
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
}

#[test]
fn stale_tag_recovered_when_target_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = tmp_file(&dir, "stale.rs");
    write_file(&path, "fn a() {\n    x();\n}\n");
    let mut store = SnapshotStore::new();

    let (_, tag) = read(&mut store, &path);
    // External edit: prepend an unrelated header line, shifting `x();` down.
    write_file(&path, "// header\nfn a() {\n    x();\n}\n");

    let patch = format!("[{}#{}]\nSWAP 2.=2:\n+    y();", path.display(), tag);
    let (new_text, _) = edit(&mut store, &path, &patch);
    assert_eq!(new_text, "// header\nfn a() {\n    y();\n}");
}

#[test]
fn delete_via_block_op() {
    let dir = tempfile::tempdir().unwrap();
    let path = tmp_file(&dir, "delblk.rs");
    write_file(&path, "fn a() {\n    x();\n}\nfn b() {}\n");
    let mut store = SnapshotStore::new();

    let (_, tag) = read(&mut store, &path);
    let patch = format!("[{}#{}]\nDEL.BLK 1", path.display(), tag);
    let (new_text, _) = edit(&mut store, &path, &patch);
    assert_eq!(new_text, "fn b() {}");
}

#[test]
fn insert_after_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = tmp_file(&dir, "insblkpost.rs");
    write_file(&path, "fn a() {\n    x();\n}\nfn b() {}\n");
    let mut store = SnapshotStore::new();

    let (_, tag) = read(&mut store, &path);
    let patch = format!("[{}#{}]\nINS.BLK.POST 1:\n+// done", path.display(), tag);
    let (new_text, _) = edit(&mut store, &path, &patch);
    assert_eq!(new_text, "fn a() {\n    x();\n}\n// done\nfn b() {}");
}

#[test]
fn multiple_hunks_in_one_patch() {
    let dir = tempfile::tempdir().unwrap();
    let path = tmp_file(&dir, "multi.rs");
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
}

#[test]
fn boundary_repair_drops_echoed_context() {
    let dir = tempfile::tempdir().unwrap();
    let path = tmp_file(&dir, "echo.rs");
    write_file(&path, "fn a() {\n    x();\n}\n");
    let mut store = SnapshotStore::new();

    let (_, tag) = read(&mut store, &path);
    // Body echoes `}` (the line below the range) — should be auto-dropped.
    let patch = format!("[{}#{}]\nSWAP 2.=2:\n+    y();\n+}}", path.display(), tag);
    let (new_text, _) = edit(&mut store, &path, &patch);
    assert_eq!(new_text, "fn a() {\n    y();\n}");
}
