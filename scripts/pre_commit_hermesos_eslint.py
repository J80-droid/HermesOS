#!/usr/bin/env python3
"""HermesOS ESLint voor pre-commit (ESLint 9 + HermesOS-plugins); werkt op Windows en POSIX."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HERMES = ROOT / "HermesOS"


def main() -> int:
    bindir = HERMES / "node_modules" / ".bin"
    eslint = bindir / ("eslint.cmd" if os.name == "nt" else "eslint")
    if not eslint.is_file():
        print(
            f"{eslint} niet gevonden — voer `pnpm install` uit in HermesOS/",
            file=sys.stderr,
        )
        return 1
    return subprocess.call([str(eslint), ".", "--max-warnings", "0"], cwd=HERMES)


if __name__ == "__main__":
    raise SystemExit(main())
