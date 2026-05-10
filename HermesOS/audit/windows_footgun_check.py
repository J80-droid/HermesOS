"""HermesOS Rust checks for Windows-specific pitfalls (also run on Linux CI).

Historisch probleem: ``std::process::Command`` voor shells vs. WinError 22 op pipes.
Hermes gebruikt exact één ``Command`` voor piped Python IPC (`run_python_ipc_script`);
meerdere uses zijn een regressie die we blokkeren.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path


def main() -> int:
    repo = Path(__file__).resolve().parents[1]
    lib_rs = repo / "src-tauri" / "src" / "lib.rs"
    src = lib_rs.read_text(encoding="utf-8")
    issues: list[str] = []

    if src.count("std::process::Command") > 1:
        issues.append(
            "Multiple std::process::Command uses — prefer tauri-plugin-shell "
            "for orchestration; piped Python IPC should stay centralized.",
        )

    if re.search(r"os::kill[^)]+,\s*0\b", src):
        issues.append(
            "Using os::kill(pid, 0) which sends CTRL_C_EVENT on Windows",
        )

    if issues:
        print("Windows compatibility issues found:")
        for i in issues:
            print(f"  - {i}")
        return 1

    print("No Windows compatibility issues found in Rust code.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
