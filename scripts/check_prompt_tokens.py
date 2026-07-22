#!/usr/bin/env python3
"""Check manox system + Default-mode prompts with a DeepSeek V3 tokenizer zip."""

import argparse
import json
import sys
import tempfile
import zipfile
from pathlib import Path

from tokenizers import Tokenizer


BUDGET = 1200


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("tokenizer_zip", type=Path)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--budget", type=int, default=BUDGET)
    args = parser.parse_args()

    with tempfile.TemporaryDirectory(prefix="manox-deepseek-tokenizer-") as tmp:
        with zipfile.ZipFile(args.tokenizer_zip) as archive:
            member = next(
                (name for name in archive.namelist() if name.endswith("/tokenizer.json")),
                None,
            )
            if member is None:
                raise SystemExit("tokenizer.json not found in archive")
            archive.extract(member, tmp)
        tokenizer = Tokenizer.from_file(str(Path(tmp) / member))

        root = args.repo / "crates/agent/src"
        rows = []
        for language, locale in (("en", "en"), ("zh-CN", "zh-CN")):
            system = (root / f"system_prompt.{language}.md").read_text()
            mode = (
                root / f"prompt/templates/{locale}/mode/default_instructions.md"
            ).read_text()
            rows.append(
                {
                    "language": language,
                    "system_tokens": len(tokenizer.encode(system).ids),
                    "default_mode_tokens": len(tokenizer.encode(mode).ids),
                    "combined_tokens": len(tokenizer.encode(system + "\n" + mode).ids),
                    "budget": args.budget,
                }
            )

    print(json.dumps(rows, ensure_ascii=False, indent=2))
    return 0 if all(row["combined_tokens"] <= args.budget for row in rows) else 1


if __name__ == "__main__":
    sys.exit(main())
