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
       - if the id already exists under `core/`, the flat copy is stale (the
         core copy is newer); it is moved to the backup, not merged;
       - otherwise it is re-homed to `core/<slug(cwd)>/<id>/session.jsonl`
         (and its `<id>.meta.json` becomes `session.meta.json`), converting
         the journal to v4 when needed.
  3. `parentSession` paths that pointed into the flat tree are rewritten to
     the re-homed core path (storage locality only; the identity is the id).
  4. User slash-command files under `~/.manox/commands` that still use the
     retired `$ARGUMENTS` placeholder are rewritten to `{{ arguments }}`.

Safety: dry-run by default; `--apply` writes. Every original is copied to
`--backup` (default `/private/tmp/manox-journal-backup-<timestamp>`) before
its file is replaced or moved. A file that fails validation is left untouched.

Usage:
  python3 script/migrate-journals.py           # report only
  python3 script/migrate-journals.py --apply   # perform
"""
import argparse, json, os, shutil, sys, tempfile, time
from pathlib import Path

HOME = Path(os.environ.get("HOME", str(Path.home())))
ROOT = HOME / ".manox"
CORE = ROOT / "core"
FLAT = ROOT / "sessions"
CORE = ROOT / "core"
FLAT = ROOT / "sessions"


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
    """id -> chain depth, or raise on a broken/cyclic parent chain."""
    parent = {e.get("id"): e.get("parentId") for e in entries}
    if any(pid is not None and pid not in parent for pid in parent.values()):
        missing = sorted({pid for pid in parent.values() if pid is not None and pid not in parent})
        raise ValueError("unknown parent id(s): %s" % ", ".join(missing[:3]))
    depth, visiting = {}, set()

    def resolve(i):
        if i in depth:
            return depth[i]
        if i in visiting:
            raise ValueError("cycle at %s" % i)
        visiting.add(i)
        p = parent[i]
        d = 0 if p is None else resolve(p) + 1
        visiting.discard(i)
        depth[i] = d
        return d

    for i in parent:
        resolve(i)
    return depth


def convert_to_v4(path: Path):
    """Return (new_header_obj, [new_line_str]) without writing."""
    lines = read_lines(path)
    header = json.loads(lines[0])
    entries = [json.loads(line) for line in lines[1:]]
    d = depths(entries)
    if header.get("version") == 3:
        header["version"] = 4
    out = [json.dumps(header, ensure_ascii=False, separators=(",", ":")) + "\n"]
    for e in entries:
        e["seq"] = d[e["id"]]
        out.append(json.dumps(e, ensure_ascii=False, separators=(",", ":")) + "\n")
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
    backup = Path(args.backup) if args.backup else Path("/private/tmp") / ("manox-journal-backup-%d" % time.time())
    print("root:", ROOT, "| apply:", args.apply, "| backup:", backup)

    core_files = sorted(CORE.glob("*/*/session.jsonl"))
    flat_files = sorted(p for p in FLAT.glob("*.jsonl") if not p.name.endswith(".lock"))
    core_ids = {p.parent.name for p in core_files}

    stats = dict(core_v3=0, core_v4=0, flat_stale=0, flat_rehome=0, errors=0)
    plan = []

    # 1. core: convert v3 in place.
    for p in core_files:
        h = json.loads(read_lines(p)[0])
        if h.get("version") != 3:
            stats["core_v4"] += 1
            continue
        stats["core_v3"] += 1
        plan.append(("core-v3", p, h.get("cwd", ""), h.get("id")))

    # 2. flat: decide stale vs re-home.
    for p in flat_files:
        lines = read_lines(p)
        h = json.loads(lines[0])
        sid = h.get("id")
        if sid in core_ids:
            stats["flat_stale"] += 1
            plan.append(("flat-stale", p, h.get("cwd", ""), sid))
        else:
            stats["flat_rehome"] += 1
            plan.append(("flat-rehome", p, h.get("cwd", ""), sid))

    print("stats:", stats)
    for kind, p, cwd, sid in plan[:10]:
        print(" ", kind, p.relative_to(ROOT), "->", project_slug(cwd), sid)

    if not args.apply:
        print("\n(dry run; pass --apply to perform)")
        return 0

    moved = {}
    for kind, p, cwd, sid in plan:
        try:
            lines = read_lines(p)
            h = json.loads(lines[0])
            if kind == "core-v3":
                backup_copy(p, backup, str(p.relative_to(ROOT)))
                header, out = convert_to_v4(p)
                atomic_write(p, out)
                print("converted", p.relative_to(ROOT))
            elif kind == "flat-stale":
                backup_copy(p, backup, str(p.relative_to(ROOT)))
                p.unlink()
                meta = p.with_name(sid + ".meta.json")
                if meta.exists():
                    backup_copy(meta, backup, str(meta.relative_to(ROOT)))
                    meta.unlink()
                print("stale flat removed", p.relative_to(ROOT))
            else:  # flat-rehome
                slug = project_slug(cwd)
                dst_dir = CORE / slug / sid
                dst = dst_dir / "session.jsonl"
                if dst.exists():
                    print("SKIP re-home (target exists):", p.relative_to(ROOT)); continue
                backup_copy(p, backup, str(p.relative_to(ROOT)))
                dst_dir.mkdir(parents=True, exist_ok=True)
                header, out = convert_to_v4(p)
                # rewrite flat parentSession paths to the re-homed core path
                ps = header.get("parentSession")
                if isinstance(ps, str) and str(FLAT) in ps:
                    pid = Path(ps).stem
                    if pid in moved:
                        header["parentSession"] = str(moved[pid] / "session.jsonl")
                        out[0] = json.dumps(header, ensure_ascii=False, separators=(",", ":")) + "\n"
                atomic_write(dst, out)
                moved[sid] = dst_dir
                meta = p.with_name(sid + ".meta.json")
                if meta.exists():
                    backup_copy(meta, backup, str(meta.relative_to(ROOT)))
                    shutil.copy2(meta, dst_dir / "session.meta.json")
                    meta.unlink()
                p.unlink()
                print("re-homed", p.relative_to(ROOT), "->", dst.relative_to(ROOT))
        except Exception as e:
            stats["errors"] += 1
            print("ERROR", p.relative_to(ROOT), ":", e, file=sys.stderr)

    # 4. command files: $ARGUMENTS -> {{ arguments }}
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

    print("done; errors:", stats["errors"], "commands:", len(changed), "backup:", backup)
    return 1 if stats["errors"] else 0


if __name__ == "__main__":
    sys.exit(main())
