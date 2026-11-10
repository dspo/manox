#!/usr/bin/env python3
"""One-time manox journal migration (v3 -> v4; flat tree -> core layout).

Run it once, after the v0.9.x runtime lands, with manox/the app stopped. It
is the operator-side conversion `UnsupportedFormat` points at; the runtime
never migrates.

What it does
  1. Every `core/<slug>/<session>/session.jsonl` whose header is version 3 is
     rewritten as version 4: each entry line gains `seq` = its chain depth
     (parent depth + 1, root = 0). Line order and every other field are kept.
  2. Every flat `sessions/<id>.jsonl`:
       - if the id already exists under `core/`, the flat copy is stale and
         is moved to the backup — unless it is NEWER than the core copy
         (mtime), which is reported as a conflict and left untouched for the
         operator to resolve;
       - otherwise it is re-homed to `core/<slug(cwd)>/<id>/session.jsonl`
         (and its `<id>.meta.json` becomes `session.meta.json`), converting
         the journal to v4 when needed.
  3. `parentSession` paths that pointed into the flat tree are rewritten to
     the re-homed core path (storage locality only; the identity is the id).
  4. User slash-command files under `~/.manox/commands` that still use the
     retired `$ARGUMENTS` placeholder are rewritten to `{{ arguments }}`.

Safety: dry-run by default; `--apply` writes. Every original is copied to
`--backup` (default `/private/tmp/manox-journal-backup-<timestamp>`) before
its file is replaced or moved. Every journal file is fenced with a
non-blocking `flock` before it is touched; a held fence (a live writer) is
reported and the file is skipped, never raced. A file that fails validation
is left untouched.

Usage:
  python3 script/migrate-journals.py                 # report only
  python3 script/migrate-journals.py --apply         # perform
  python3 script/migrate-journals.py --root DIR      # another state root
"""
import argparse
import fcntl
import json
import os
import shutil
import sys
import tempfile
import time
from contextlib import contextmanager
from pathlib import Path

HOME = Path(os.environ.get("HOME", str(Path.home())))
ROOT = HOME / ".manox"
CORE = ROOT / "core"
FLAT = ROOT / "sessions"


@contextmanager
def fenced(path: Path):
    """Exclusive non-blocking flock on a journal file.

    The runtime's writer takes the same per-file fence around every append, so
    failing to take it means a live writer is active: refuse the file instead
    of racing it (an `os.replace` under a live writer would drop that writer's
    next line into a ghost inode).
    """
    fd = os.open(path, os.O_RDWR)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as err:
            raise RuntimeError(f"fence held by a live writer: {err}") from err
        yield
    finally:
        os.close(fd)


def project_slug(cwd: str) -> str:
    out, sep_run = [], False
    for ch in cwd:
        if ch in "/\\:":
            if not sep_run:
                out.append("-")
            sep_run = True
        elif ch != "~" and (ch.isalnum() and ch.isascii() or ch in "._-"):
            out.append(ch)
            sep_run = False
        else:
            out.append("~%04X" % ord(ch))
            sep_run = False
    slug = "".join(out).lstrip("-")
    slug = slug or "root"
    return "--" + slug[:251] + "--"


def read_lines(path: Path):
    with open(path, "r", encoding="utf-8") as f:
        return [line for line in f if line.strip()]


def depths(entries):
    """id -> chain depth, or raise on a broken/cyclic/duplicated chain.

    Iterative: the longest linear sessions are exactly the ones most likely to
    need migration, and a recursive walk would hit Python's recursion limit
    (~995) before them.
    """
    parent = {}
    for entry in entries:
        entry_id = entry.get("id")
        if not entry_id:
            raise ValueError("entry without an id")
        if entry_id in parent:
            raise ValueError(f"duplicate entry id {entry_id}")
        parent[entry_id] = entry.get("parentId")
    missing = sorted(
        {pid for pid in parent.values() if pid is not None and pid not in parent}
    )
    if missing:
        raise ValueError("unknown parent id(s): %s" % ", ".join(missing[:3]))

    depth = {}
    for start in parent:
        if start in depth:
            continue
        chain, seen = [], set()
        cursor = start
        while cursor is not None and cursor not in depth:
            if cursor in seen:
                raise ValueError(f"cycle at {cursor}")
            seen.add(cursor)
            chain.append(cursor)
            cursor = parent[cursor]
        base = -1 if cursor is None else depth[cursor]
        for node in reversed(chain):
            base += 1
            depth[node] = base
    return depth


def convert_to_v4(path: Path):
    """Return (new_header_obj, [new_line_str]) without writing."""
    lines = read_lines(path)
    header = json.loads(lines[0])
    entries = [json.loads(line) for line in lines[1:]]
    chain_depth = depths(entries)
    if header.get("version") == 3:
        header["version"] = 4
    out = [json.dumps(header, ensure_ascii=False, separators=(",", ":")) + "\n"]
    for entry in entries:
        entry["seq"] = chain_depth[entry["id"]]
        out.append(json.dumps(entry, ensure_ascii=False, separators=(",", ":")) + "\n")
    if len(out) != len(lines):
        raise ValueError("line count changed during conversion")
    return header, out


def atomic_write(path: Path, lines):
    fd, tmp = tempfile.mkstemp(dir=str(path.parent), prefix=".migrate-", suffix=".jsonl")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.writelines(lines)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def backup_copy(src: Path, backup: Path, rel: str):
    dst = backup / rel
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, dst)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--apply", action="store_true")
    ap.add_argument("--root", default=str(HOME / ".manox"))
    ap.add_argument("--backup", default=None)
    args = ap.parse_args()
    global ROOT, CORE, FLAT
    ROOT = Path(args.root)
    CORE = ROOT / "core"
    FLAT = ROOT / "sessions"
    backup = Path(args.backup) if args.backup else Path("/private/tmp") / (
        "manox-journal-backup-%d" % time.time()
    )
    print("root:", ROOT, "| apply:", args.apply, "| backup:", backup)

    core_files = sorted(CORE.glob("*/*/session.jsonl"))
    flat_files = sorted(p for p in FLAT.glob("*.jsonl") if not p.name.endswith(".lock"))
    core_by_id = {p.parent.name: p for p in core_files}

    stats = dict(
        core_v3=0,
        core_v4=0,
        flat_stale=0,
        flat_rehome=0,
        flat_conflict=0,
        errors=0,
        warnings=0,
    )
    plan = []

    for p in core_files:
        header = json.loads(read_lines(p)[0])
        if header.get("version") != 3:
            stats["core_v4"] += 1
            continue
        stats["core_v3"] += 1
        plan.append(("core-v3", p, header.get("cwd", ""), header.get("id")))

    for p in flat_files:
        header = json.loads(read_lines(p)[0])
        sid = header.get("id")
        if sid in core_by_id:
            if p.stat().st_mtime > core_by_id[sid].stat().st_mtime + 60:
                stats["flat_conflict"] += 1
                plan.append(("flat-conflict", p, header.get("cwd", ""), sid))
            else:
                stats["flat_stale"] += 1
                plan.append(("flat-stale", p, header.get("cwd", ""), sid))
        else:
            stats["flat_rehome"] += 1
            plan.append(("flat-rehome", p, header.get("cwd", ""), sid))

    # Re-home targets are resolved before anything is written, so a parent
    # processed later can still be named by its child's `parentSession`.
    targets = {
        sid: CORE / project_slug(cwd) / sid / "session.jsonl"
        for kind, _p, cwd, sid in plan
        if kind == "flat-rehome"
    }

    print("stats:", stats)
    for kind, p, cwd, sid in plan[:10]:
        print(" ", kind, p.relative_to(ROOT), "->", project_slug(cwd), sid)
    for kind, p, _cwd, sid in plan:
        if kind == "flat-conflict":
            print(
                "CONFLICT: flat copy is newer than core; left untouched:",
                p.relative_to(ROOT),
                "id",
                sid,
            )

    if not args.apply:
        print("\n(dry run; pass --apply to perform)")
        return 0

    moved = {}
    for kind, p, cwd, sid in plan:
        if kind == "flat-conflict":
            continue
        try:
            if kind == "core-v3":
                with fenced(p):
                    backup_copy(p, backup, str(p.relative_to(ROOT)))
                    header, out = convert_to_v4(p)
                    atomic_write(p, out)
                print("converted", p.relative_to(ROOT))
            elif kind == "flat-stale":
                with fenced(p):
                    backup_copy(p, backup, str(p.relative_to(ROOT)))
                    p.unlink()
                    meta = p.with_name(sid + ".meta.json")
                    if meta.exists():
                        backup_copy(meta, backup, str(meta.relative_to(ROOT)))
                        meta.unlink()
                print("stale flat removed", p.relative_to(ROOT))
            else:  # flat-rehome
                dst = targets[sid]
                if dst.exists():
                    print("SKIP re-home (target exists):", p.relative_to(ROOT))
                    continue
                with fenced(p):
                    backup_copy(p, backup, str(p.relative_to(ROOT)))
                    header, out = convert_to_v4(p)
                    ps = header.get("parentSession")
                    if isinstance(ps, str) and str(FLAT) in ps:
                        pid = Path(ps).stem
                        if pid in targets:
                            header["parentSession"] = str(targets[pid])
                            out[0] = (
                                json.dumps(header, ensure_ascii=False, separators=(",", ":"))
                                + "\n"
                            )
                    dst.parent.mkdir(parents=True, exist_ok=True)
                    atomic_write(dst, out)
                    moved[sid] = dst.parent
                    meta = p.with_name(sid + ".meta.json")
                    if meta.exists():
                        backup_copy(meta, backup, str(meta.relative_to(ROOT)))
                        shutil.copy2(meta, dst.parent / "session.meta.json")
                        meta.unlink()
                    p.unlink()
                print("re-homed", p.relative_to(ROOT), "->", dst.relative_to(ROOT))
        except Exception as err:
            stats["errors"] += 1
            print("ERROR", p.relative_to(ROOT), ":", err, file=sys.stderr)

    # Command files: the retired `$ARGUMENTS` placeholder.
    cmd_root = ROOT / "commands"
    changed = []
    for p in sorted(cmd_root.rglob("*.md")) if cmd_root.is_dir() else []:
        try:
            text = p.read_text(encoding="utf-8")
        except OSError:
            continue
        if "$ARGUMENTS" not in text:
            continue
        backup_copy(p, backup, str(p.relative_to(ROOT)))
        p.write_text(text.replace("$ARGUMENTS", "{{ arguments }}"), encoding="utf-8")
        changed.append(p.relative_to(ROOT))
    for rel in changed:
        print("rewrote command placeholder:", rel)

    problems = stats["errors"] + stats["flat_conflict"]
    print(
        "done; errors:",
        stats["errors"],
        "conflicts:",
        stats["flat_conflict"],
        "commands:",
        len(changed),
        "backup:",
        backup,
    )
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
