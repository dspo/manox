#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="${CX_INSTALL_DIR:-$HOME/.local/bin}"
TARGET_BIN="$INSTALL_DIR/cx"
ZSHRC="${ZDOTDIR:-$HOME}/.zshrc"
LEGACY_CCC="$HOME/.local/bin/ccc"

mkdir -p "$INSTALL_DIR"

cd "$ROOT_DIR"
cargo build --release
install "$ROOT_DIR/target/release/cx" "$TARGET_BIN"

if [[ -f "$ZSHRC" ]]; then
  python3 - "$ZSHRC" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1]).expanduser()
text = path.read_text()
lines = text.splitlines()

remove = {
    "# AI CLI 统一启动器 — 调用 ~/.local/bin/ccc (TUI + 探测)",
    "# ccc = Copilot + Claude + Codex",
    'claude()  { ~/.local/bin/ccc claude "$@"; }',
    'codex()   { ~/.local/bin/ccc codex "$@"; }',
    'copilot() { ~/.local/bin/ccc copilot "$@"; }',
}

filtered = []
skip_blank = False
for line in lines:
    if line in remove:
        skip_blank = True
        continue
    if skip_blank and line == "":
        skip_blank = False
        continue
    skip_blank = False
    filtered.append(line)

new_text = "\n".join(filtered).rstrip() + "\n"
path.write_text(new_text)
PY
fi

rm -f "$LEGACY_CCC"

echo "已安装: $TARGET_BIN"
echo "已移除 ~/.zshrc 中旧的 ccc 劫持，并删除 ~/.local/bin/ccc"
