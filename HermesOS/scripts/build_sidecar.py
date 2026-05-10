#!/usr/bin/env python3
"""PyInstaller build script for the Hermes Agent Tauri sidecar (Windows onefile).

Usage (from repository root, active venv recommended)::

    python HermesOS/scripts/build_sidecar.py [--hermes-version 0.13.0]

Outputs::

    HermesOS/src-tauri/binaries/hermes_agent-x86_64-pc-windows-msvc.exe

Requires::

    pip install pyinstaller

This script invokes PyInstaller programmatically via ``python -m PyInstaller`` with
``--noupx`` (UPX triggers Defender false positives), ``--onefile``, and explicit
``--paths`` so imports from the monorepo root resolve.
"""

from __future__ import annotations

import argparse
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path


# Repository root (parent of HermesOS/)
_SCRIPT_FILE = Path(__file__).resolve()
_REPO_ROOT = _SCRIPT_FILE.parent.parent.parent
_HERMESOS_DIR = _REPO_ROOT / "HermesOS"
_SCRIPTS_DIR = _HERMESOS_DIR / "scripts"
_BINARIES_DIR = _HERMESOS_DIR / "src-tauri" / "binaries"
_WORK_DIR = _REPO_ROOT / "build" / "pyinstaller_hermes_sidecar"

ENTRY_POINT = _SCRIPTS_DIR / "sidecar_entry.py"

TARGET_TRIPLE = "x86_64-pc-windows-msvc"
OUTPUT_NAME = f"hermes_agent-{TARGET_TRIPLE}"

# Hidden imports: dynamic imports / lazy-loaded stacks PyInstaller often misses.
# Extend as new optional deps appear in the agent tree.
HIDDEN_IMPORTS = [
    # Repo entry / bootstrap
    "hermes_bootstrap",
    "hermes_constants",
    "run_agent",
    # Core HTTP / async
    "openai",
    "anthropic",
    "pydantic",
    "pydantic_core",
    "pydantic.deprecated",
    "httpx",
    "httpcore",
    "h11",
    "anyio",
    "sniffio",
    "certifi",
    "idna",
    # Config & UX (CLI pulls these)
    "yaml",
    "tomli",
    "rich",
    "prompt_toolkit",
    # SQLite / stdlib hooks
    "sqlite3",
    # Windows
    "psutil",
    "ctypes",
    "ctypes.windll",
    # Tool / plugin discovery
    "tools",
    "tools.registry",
    "plugins.memory",
    "plugins.memory.honcho",
    "plugins.memory.mem0",
    "plugins.memory.supermemory",
    "plugins.memory.byterover",
    "plugins.memory.hindsight",
    "plugins.memory.holographic",
    "plugins.memory.openviking",
    "plugins.memory.retaindb",
    "skills",
]


def check_prerequisites() -> None:
    try:
        import PyInstaller  # noqa: F401
    except ImportError:
        print("ERROR: PyInstaller not installed. Run: pip install pyinstaller", file=sys.stderr)
        sys.exit(1)

    if not ENTRY_POINT.exists():
        print(f"ERROR: Entry point not found: {ENTRY_POINT}", file=sys.stderr)
        sys.exit(1)

    if platform.system() != "Windows":
        print(
            "WARNING: This script targets Windows (x86_64-pc-windows-msvc). "
            "Building on another OS requires a cross toolchain (not supported here).",
            file=sys.stderr,
        )


def build_sidecar(hermes_version: str) -> None:
    check_prerequisites()

    _BINARIES_DIR.mkdir(parents=True, exist_ok=True)
    _WORK_DIR.mkdir(parents=True, exist_ok=True)

    out_exe = _BINARIES_DIR / f"{OUTPUT_NAME}.exe"
    print("=" * 60)
    print("Hermes Agent — PyInstaller sidecar (onefile)")
    print(f"  Repo root:    {_REPO_ROOT}")
    print(f"  Entry:        {ENTRY_POINT}")
    print(f"  Output:       {out_exe}")
    print(f"  HERMES ver:   {hermes_version}")
    print("=" * 60)

    cmd: list[str] = [
        sys.executable,
        "-m",
        "PyInstaller",
        "--onefile",
        "--noupx",
        "--noconfirm",
        "--name",
        OUTPUT_NAME,
        "--distpath",
        str(_BINARIES_DIR),
        "--workpath",
        str(_WORK_DIR),
        "--specpath",
        str(_WORK_DIR),
        "--paths",
        str(_REPO_ROOT),
    ]

    if platform.system() == "Windows":
        cmd.append("--noconsole")

    for mod in HIDDEN_IMPORTS:
        cmd.extend(["--hidden-import", mod])

    cmd.extend(
        [
            "--collect-data",
            "skills",
            "--collect-all",
            "plugins.memory",
            "--collect-all",
            "tools",
        ]
    )

    cmd.append(str(ENTRY_POINT))


    print(f"\nRunning:\n  {' '.join(cmd)}\n")

    env = {**os.environ, "HERMES_SIDECAR_VERSION": hermes_version}

    result = subprocess.run(cmd, cwd=str(_REPO_ROOT), env=env)

    if result.returncode != 0:
        print(f"\nERROR: PyInstaller failed with exit code {result.returncode}", file=sys.stderr)
        sys.exit(result.returncode)

    if not out_exe.exists():
        print(f"\nERROR: Expected binary missing: {out_exe}", file=sys.stderr)
        sys.exit(1)

    size_mb = out_exe.stat().st_size / (1024 * 1024)
    print(f"\nOK: {out_exe}")
    print(f"     Size: {size_mb:.1f} MB")
    print("\nNext: pnpm tauri build   (from HermesOS/) — bundles binaries/hermes_agent via externalBin")


def clean_workdir() -> None:
    """Optional: remove PyInstaller work dir for a fully clean rebuild."""
    if _WORK_DIR.exists():
        shutil.rmtree(_WORK_DIR, ignore_errors=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Build Hermes sidecar onefile EXE for Tauri")
    parser.add_argument(
        "--hermes-version",
        default=os.environ.get("HERMES_SIDECAR_VERSION", "0.13.0"),
        help="Version string embedded in sidecar hello (default: 0.13.0)",
    )
    parser.add_argument(
        "--clean-work",
        action="store_true",
        help="Delete build/pyinstaller_hermes_sidecar before building",
    )
    args = parser.parse_args()

    if args.clean_work:
        clean_workdir()

    build_sidecar(args.hermes_version)
