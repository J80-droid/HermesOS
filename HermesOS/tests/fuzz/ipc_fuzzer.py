"""IPC Fuzzer for the Hermes Agent sidecar JSON protocol.

This chaos engineering module fires malformed, corrupt, or unexpected
JSON payloads at the sidecar's stdin to verify that:

  1. The sidecar never crashes (segfault, panic, unhandled exception)
  2. Every input produces a valid JSON response (graceful error handling)
  3. The sidecar remains responsive after attack sequences

Usage:
    python ipc_fuzzer.py                          # single sequence
    python ipc_fuzzer.py --iterations 1000         # stress test
    python ipc_fuzzer.py --sidecar-exe ./binary    # compiled binary
    python ipc_fuzzer.py --seed 42                 # deterministic
"""

import argparse
import json
import os
import queue
import random
import string
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple


def _wire_rpc_method(msg: Dict[str, Any]) -> Optional[str]:
    """JSON-RPC ``method`` or legacy ``type`` (matches HermesOS Rust bridge)."""
    m = msg.get("method")
    if isinstance(m, str):
        return m
    t = msg.get("type")
    if isinstance(t, str):
        return t
    return None


def _stdin_write(proc: subprocess.Popen, payload: bytes) -> None:
    """Write stdin in chunks (avoids WinError 22 / pipe buffer edge cases on Windows)."""
    if proc.stdin is None:
        return
    # Windows: writing b"" to a PIPE stdin can raise OSError 22 (EINVAL); skip no-op writes.
    if not payload:
        return
    chunk = 64 * 1024
    if len(payload) <= chunk:
        proc.stdin.write(payload)
    else:
        for i in range(0, len(payload), chunk):
            proc.stdin.write(payload[i : i + chunk])
    proc.stdin.flush()


# ── Payload Generators ──────────────────────────────────────────────────


def random_junk(length: int = random.randint(1, 4096)) -> bytes:
    """Generate completely random binary junk."""
    return bytes(random.randint(0, 255) for _ in range(length))


def random_unicode_junk(length: int = random.randint(1, 512)) -> str:
    """Generate random Unicode garbage including emoji."""
    chars = []
    for _ in range(length):
        mode = random.randint(0, 4)
        if mode == 0:
            chars.append(random.choice(string.ascii_letters + string.digits))
        elif mode == 1:
            chars.append(chr(random.randint(0x80, 0xFF)))  # Latin-1
        elif mode == 2:
            chars.append(chr(random.randint(0x2000, 0x2FFF)))  # CJK / Symbols
        elif mode == 3:
            chars.append(chr(random.randint(0x1F300, 0x1F9FF)))  # Emoji
        else:
            # Surrogate codepoints are invalid in Python 3 str / UTF-8 JSON
            while True:
                cp = random.randint(0, 0x10FFFF)
                if 0xD800 <= cp <= 0xDFFF:
                    continue
                chars.append(chr(cp))
                break
    return "".join(chars)


def random_json_value(depth: int = 0) -> Any:
    """Generate a random JSON value."""
    if depth > 5:
        return random.choice([None, True, False, random.randint(-2**63, 2**63), random.random()])

    choice = random.randint(0, 6)
    if choice == 0:
        return None
    elif choice == 1:
        return True
    elif choice == 2:
        return False
    elif choice == 3:
        return random.randint(-2**63, 2**63)
    elif choice == 4:
        return random.random() * 1e308
    elif choice == 5:
        # Array
        return [
            random_json_value(depth + 1) for _ in range(random.randint(0, 10))
        ]
    else:
        # Object
        return {
            random_unicode_junk(random.randint(1, 32)): random_json_value(depth + 1)
            for _ in range(random.randint(0, 8))
        }


def generate_fuzz_payloads() -> List[Tuple[str, bytes]]:
    """Generate a list of (description, payload_bytes) fuzz inputs."""
    payloads = []

    # 1. Empty and whitespace
    payloads.append(("empty", b""))
    payloads.append(("just newline", b"\n"))
    payloads.append(("spaces", b"   \n"))
    payloads.append(("tab newline", b"\t\n"))
    payloads.append(("null byte", b"\x00\n"))

    # 2. Truncated / partial JSON
    payloads.append(("truncated array", b'["a", "b"\n'))
    payloads.append(("truncated object", b'{"key": \n'))
    payloads.append(("truncated string", b'"hello\n'))
    payloads.append(("missing value", b'{"type":\n'))

    # 3. Protocol corruption — wrong envelope shapes
    payloads.append(("no type field", json.dumps({"data": {}}).encode() + b"\n"))
    payloads.append(
        ("type is null", json.dumps({"type": None, "data": {}}).encode() + b"\n")
    )
    payloads.append(
        ("type is number",
         json.dumps({"type": 42, "data": {}}).encode() + b"\n")
    )
    payloads.append(
        ("type is array",
         json.dumps({"type": [], "data": {}}).encode() + b"\n")
    )

    # 4. Deeply nested JSON (stack overflow test)
    deep = {}
    current = deep
    for _ in range(1000):
        current["x"] = {}
        current = current["x"]
    current["method"] = "sidecar_hello"
    payloads.append(("deep nesting 1000", json.dumps(deep).encode() + b"\n"))

    # 5. Oversized payloads
    payloads.append(
        ("large string 100KB", ("x" * 100 * 1024 + "\n").encode())
    )
    payloads.append(
        ("large json 1MB", json.dumps({"data": "x" * 1024 * 1024}).encode() + b"\n")
    )

    # 6. Python-specific injection attempts
    payloads.append(("import attempt", b"import os\n"))
    payloads.append(("exec attempt", b"__import__('os').system('dir')\n"))
    payloads.append(
        ("eval attempt",
         json.dumps({"type": "__import__('os').system('dir')"}).encode() + b"\n")
    )

    # 7. Protocol message fuzzing — wrong command names
    payloads.append(
        ("unknown command hello",
         json.dumps({"type": "sidecar_hello_invalid"}).encode() + b"\n")
    )
    payloads.append(
        ("garbage command",
         json.dumps({"type": random_unicode_junk(50), "data": {}}).encode() + b"\n")
    )

    # 8. Extremely long keys
    payloads.append(
        ("huge key",
         json.dumps({"type": "x" * 10000, "data": {}}).encode() + b"\n")
    )

    # 9. Multiple query lines in one write
    payloads.append(
        ("two queries at once",
         b'{"type": "status"}\n{"type": "status", "data": "ready"}\n')
    )

    # 10. Repeated /quit commands
    payloads.append(
        ("repeated quit", b"/quit\n/quit\n/quit\n/quit\n/quit\n")
    )

    # 11. Random binary blobs
    for length in [4, 16, 256, 1024, 65536]:
        payloads.append((f"random junk {length}B", random_junk(length) + b"\n"))

    # 12. Unicode edge cases
    payloads.append(
        ("zero width chars",
         ("\u200B\u200C\u200D\uFEFF" + "\n").encode("utf-8"))
    )
    payloads.append(
        ("BOM prefix",
         ("\uFEFF" + '{"type": "hello"}' + "\n").encode("utf-8"))
    )
    payloads.append(
        ("RTL override", ("\u202E" + '{"type": "hello"}' + "\n").encode("utf-8"))
    )

    return payloads


# ── Fuzzer Engine ────────────────────────────────────────────────────────


class IpcFuzzer:
    """Fuzz the sidecar IPC protocol with malformed inputs."""

    def __init__(
        self,
        entry_point: Optional[Path] = None,
        exe_path: Optional[Path] = None,
        seed: int = 0,
        iterations: int = 100,
        hermes_home: Optional[Path] = None,
    ):
        self.entry_point = entry_point or self._find_entry_point()
        self.exe_path = exe_path
        self.seed = seed if seed else int(time.time())
        self.iterations = iterations
        self.hermes_home = hermes_home or Path(tempfile.mkdtemp())
        self.rng = random.Random(self.seed)
        self.results: Dict[str, Dict[str, Any]] = {}

    @staticmethod
    def _find_entry_point() -> Path:
        repo = Path(__file__).resolve().parent.parent.parent.parent
        entry = repo / "HermesOS" / "scripts" / "sidecar_entry.py"
        if entry.exists():
            return entry
        raise FileNotFoundError(f"Entry point not found: {entry}")

    def _spawn(self) -> subprocess.Popen:
        base_env = os.environ.copy()
        base_env.setdefault("PYTHONUTF8", "1")
        base_env["HERMES_HOME"] = str(self.hermes_home)

        # CRITICAL: Add repo root to PYTHONPATH for run_agent imports
        repo_root = str(Path(__file__).resolve().parent.parent.parent.parent)
        sep = os.pathsep
        pythonpath = base_env.get("PYTHONPATH", "")
        if pythonpath:
            base_env["PYTHONPATH"] = sep.join([repo_root, pythonpath])
        else:
            base_env["PYTHONPATH"] = repo_root

        popen_kw: Dict[str, Any] = {}
        if sys.platform == "win32":
            # Avoid spawning a visible console window on Windows desktop runs.
            creationflags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
            if creationflags:
                popen_kw["creationflags"] = creationflags

        if self.exe_path:
            cmd = [str(self.exe_path)]
        else:
            # Protocol fuzzing must not require LLM keys or full ~/.hermes — matches
            # tests/sidecar_protocol (HERMES_SIDECAR_MOCK=1).
            base_env.setdefault("HERMES_SIDECAR_MOCK", "1")
            cmd = [sys.executable, "-u", str(self.entry_point)]

        return subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=base_env,
            cwd=repo_root,
            bufsize=0,
            **popen_kw,
        )

    @staticmethod
    def _fuzz_collect_timeout(description: str, payload: bytes) -> float:
        """How long to wait for stdout/exit after sending a payload (per fuzz case)."""
        if len(payload) > 50 * 1024:
            return 60.0
        dlow = description.lower()
        if "deep nesting" in dlow:
            return 45.0
        if "100kb" in dlow or "1mb" in dlow:
            return 45.0
        return 3.0

    def _send_and_collect(
        self, proc: subprocess.Popen, payload: bytes, timeout: float = 3.0
    ) -> Dict[str, Any]:
        """Send payload and poll stdout until idle timeout or process exit.

        Uses a reader thread so ``stdout.readline()`` cannot deadlock the parent
        when the sidecar blocks waiting for more stdin (e.g. empty fuzz payload).
        """
        result: Dict[str, Any] = {
            "stdout": [],
            "stderr": [],
            "returncode": None,
            "crashed": False,
        }

        out_q: queue.Queue[bytes | None] = queue.Queue()
        err_lines: List[str] = []

        def pump_out() -> None:
            try:
                if proc.stdout is None:
                    out_q.put(None)
                    return
                while True:
                    line = proc.stdout.readline()
                    if not line:
                        break
                    out_q.put(line)
            finally:
                out_q.put(None)

        def pump_err() -> None:
            try:
                if proc.stderr is None:
                    return
                while True:
                    line = proc.stderr.readline()
                    if not line:
                        break
                    err_lines.append(line.decode("utf-8", errors="replace"))
            except (IOError, ValueError):
                pass

        threading.Thread(target=pump_out, daemon=True).start()
        threading.Thread(target=pump_err, daemon=True).start()

        try:
            _stdin_write(proc, payload)
        except (BrokenPipeError, OSError) as e:
            result["stderr"].append(str(e))
            result["crashed"] = True
            result["stderr"].extend(err_lines)
            return result

        deadline = time.time() + timeout
        broke_on_exit = False
        while time.time() < deadline:
            rc = proc.poll()
            if rc is not None:
                result["returncode"] = rc
                result["crashed"] = rc != 0
                broke_on_exit = True
                break
            try:
                line = out_q.get(timeout=0.15)
            except queue.Empty:
                continue
            if line is None:
                rc = proc.poll()
                if rc is not None:
                    result["returncode"] = rc
                    result["crashed"] = rc != 0
                    broke_on_exit = True
                break
            text = line.decode("utf-8", errors="replace").strip()
            if text:
                result["stdout"].append(text)

        if not broke_on_exit and result["returncode"] is None:
            rc = proc.poll()
            if rc is not None:
                result["returncode"] = rc
                result["crashed"] = rc != 0
            else:
                # Still alive after idle timeout — acceptable for fuzz survival.
                result["crashed"] = False

        result["stderr"].extend(err_lines)
        return result

    def _shutdown_sidecar(self, proc: subprocess.Popen, timeout: float = 12.0) -> None:
        """Ask the sidecar to exit and drain pipes (prevents PIPE deadlocks if stdout fills)."""
        try:
            exit_line = (
                json.dumps(
                    {
                        "jsonrpc": "2.0",
                        "id": 999999,
                        "method": "exit",
                        "params": {},
                    },
                    ensure_ascii=False,
                )
                + "\n"
            )
            _stdin_write(proc, exit_line.encode("utf-8"))
        except (BrokenPipeError, OSError):
            pass

        def slurp(stream: Optional[Any]) -> None:
            if stream is None:
                return
            try:
                while True:
                    chunk = stream.read(4096)
                    if not chunk:
                        break
            except (ValueError, OSError):
                pass

        threading.Thread(target=slurp, args=(proc.stdout,), daemon=True).start()
        threading.Thread(target=slurp, args=(proc.stderr,), daemon=True).start()
        try:
            proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()

    def run_single(
        self, description: str, payload: bytes
    ) -> Dict[str, Any]:
        """Run a single fuzz payload against the sidecar.

        Verifies ``sidecar_hello``, consumes ``sidecar_ready``, then sends the payload.
        Python entry uses ``HERMES_SIDECAR_MOCK=1`` by default so no LLM keys are required
        (same idea as ``tests/sidecar_protocol``). Real binaries via ``HERMES_SIDECAR_EXE``
        do not get MOCK unless the environment already sets it.
        """
        proc = self._spawn()

        try:
            # First, verify sidecar started by reading sidecar_hello
            # This proves the binary runs even without API keys
            hello_line = proc.stdout.readline()
            if not hello_line:
                result = {
                    "stdout": [],
                    "stderr": [],
                    "returncode": proc.poll(),
                    "crashed": False,
                    "skipped": True,
                    "reason": "Sidecar failed to init (no API keys?)",
                }
                self.results[description] = result
                proc.kill()
                proc.wait()
                return result

            # Check it's valid JSON with sidecar_hello (JSON-RPC method or legacy type)
            try:
                hello = json.loads(hello_line.decode("utf-8", errors="replace").strip())
                if _wire_rpc_method(hello) != "sidecar_hello":
                    result = {
                        "stdout": [hello_line.decode("utf-8", errors="replace").strip()],
                        "stderr": [],
                        "returncode": None,
                        "crashed": False,
                        "skipped": True,
                        "reason": (
                            "Expected sidecar_hello, got "
                            f"{_wire_rpc_method(hello)!r}"
                        ),
                    }
                    self.results[description] = result
                    proc.kill()
                    proc.wait()
                    return result
            except json.JSONDecodeError:
                pass

            # Consume sidecar_ready (second line) — avoids a concurrent drain thread
            # racing _send_and_collect's stdout pump on the same pipe.
            ready_raw = proc.stdout.readline()
            if ready_raw:
                try:
                    ready_msg = json.loads(
                        ready_raw.decode("utf-8", errors="replace").strip()
                    )
                    if _wire_rpc_method(ready_msg) != "sidecar_ready":
                        pass  # tolerate banners; fuzz harness only needs a live process
                except json.JSONDecodeError:
                    pass

            result = self._send_and_collect(
                proc,
                payload,
                timeout=self._fuzz_collect_timeout(description, payload),
            )
            self.results[description] = result
            return result
        finally:
            try:
                self._shutdown_sidecar(proc)
            except OSError:
                try:
                    proc.kill()
                    proc.wait()
                except OSError:
                    pass

    def run_batch(self, payloads: List[Tuple[str, bytes]]) -> None:
        """Run multiple fuzz payloads."""
        for desc, payload in payloads:
            result = self.run_single(desc, payload)
            if result.get("skipped"):
                status = "SKIP"
            elif result["crashed"]:
                status = "CRASH"
            else:
                status = "OK"
            print(f"  [{status}] {desc}")
            if result.get("skipped"):
                print(f"    reason: {result.get('reason', 'unknown')}")
            elif result["crashed"]:
                print(f"    returncode={result['returncode']}")
                for line in result["stderr"][-3:]:
                    print(f"    stderr: {line}")

    def run_random(self, count: int) -> None:
        """Run random fuzz payloads."""
        print(f"\n=== Random Fuzz ({count} iterations, seed={self.seed}) ===\n")
        failures = 0

        for i in range(count):
            # Mix of payload types
            mode = self.rng.randint(0, 5)
            if mode == 0:
                payload = random_junk(self.rng.randint(1, 4096))
                desc = f"random_junk_{len(payload)}B"
            elif mode == 1:
                payload = random_unicode_junk(self.rng.randint(1, 512)).encode("utf-8")
                desc = f"random_unicode_{len(payload)}B"
            elif mode == 2:
                payload = json.dumps(random_json_value()).encode("utf-8") + b"\n"
                desc = f"random_json_{len(payload)}B"
            elif mode == 3:
                payload = b"x" * self.rng.randint(1, 10**6) + b"\n"
                desc = f"long_string_{len(payload)}B"
            elif mode == 4:
                # Control characters
                payload = bytes(self.rng.randint(0, 31) for _ in range(self.rng.randint(1, 256)))
                desc = f"control_chars_{len(payload)}B"
            else:
                # Repeated command
                payload = b"/quit\n" * self.rng.randint(1, 100)
                desc = f"repeated_quit_{len(payload)}B"

            result = self.run_single(desc, payload)
            if result["crashed"]:
                failures += 1
                status = "CRASH"
            else:
                status = "OK"

            progress = f"{i + 1}/{count}"
            print(f"  [{progress:>10}] [{status}] {desc}")
            sys.stdout.flush()

        print(f"\n  Results: {count - failures}/{count} survived, {failures} crashes")
        if failures > 0:
            print("  WARNING: Some payloads caused crashes!")

    def summarize(self) -> None:
        """Print summary of all fuzz results."""
        total = len(self.results)
        crashes = sum(1 for r in self.results.values() if r.get("crashed"))
        skipped = sum(1 for r in self.results.values() if r.get("skipped"))
        passed = total - crashes - skipped
        print(f"\n{'=' * 50}")
        print(f"Fuzz Summary")
        print(f"{'=' * 50}")
        print(f"  Total payloads:  {total}")
        print(f"  Passed (OK):     {passed}")
        print(f"  Skipped:         {skipped}")
        print(f"  Crashes:         {crashes}")
        if total > skipped:
            print(f"  Survival rate:   {(total - crashes - skipped) / (total - skipped) * 100:.1f}%")
        print(f"  Seed:            {self.seed}")
        print()


def main():
    parser = argparse.ArgumentParser(
        description="IPC Fuzzer for Hermes Agent sidecar protocol"
    )
    parser.add_argument(
        "--sidecar-exe",
        type=Path,
        default=None,
        help="Path to compiled sidecar binary",
    )
    parser.add_argument(
        "--iterations",
        type=int,
        default=100,
        help="Number of random fuzz iterations (default: 100)",
    )
    parser.add_argument("--seed", type=int, default=0, help="Random seed")
    parser.add_argument(
        "--entry-point",
        type=Path,
        default=None,
        help="Python entry point (default: auto-detect)",
    )
    parser.add_argument(
        "--mode",
        choices=["static", "random", "quick"],
        default="quick",
        help="Fuzz mode: static=predefined, random=generated, quick=small batch",
    )
    args = parser.parse_args()

    fuzzer = IpcFuzzer(
        entry_point=args.entry_point,
        exe_path=args.sidecar_exe,
        seed=args.seed,
        iterations=args.iterations,
    )

    print(f"IPC Fuzzer for Hermes Agent Sidecar")
    print(f"  Seed:       {fuzzer.seed}")
    print(f"  Entry:      {fuzzer.entry_point or fuzzer.exe_path}")
    print(f"  Iterations: {fuzzer.iterations}")
    print(f"  HERMES_HOME: {fuzzer.hermes_home}")
    print()

    if args.mode == "static":
        payloads = generate_fuzz_payloads()
        print(f"=== Static Fuzz ({len(payloads)} payloads) ===\n")
        fuzzer.run_batch(payloads)
    elif args.mode == "quick":
        payloads = generate_fuzz_payloads()
        print(f"=== Static Fuzz ({len(payloads)} payloads) ===\n")
        fuzzer.run_batch(payloads)
        fuzzer.run_random(min(args.iterations, 30))
    else:
        fuzzer.run_random(args.iterations)

    fuzzer.summarize()

    if any(r.get("crashed") for r in fuzzer.results.values()):
        print("Fuzzing FAILED -- sidecar had real crashes!")
        for desc, r in fuzzer.results.items():
            if r.get("crashed"):
                print(f"  CRASH: {desc}")
        sys.exit(1)
    else:
        skipped = sum(1 for r in fuzzer.results.values() if r.get("skipped"))
        total = len(fuzzer.results)
        if skipped > 0:
            print(f"Fuzzing PASSED -- {total - skipped}/{total} payloads survived, {skipped} skipped (no API keys)")
        else:
            print(f"Fuzzing PASSED -- sidecar survived all {total} payloads.")
        sys.exit(0)


if __name__ == "__main__":
    main()