"""PyInstaller entry point for the Hermes Agent sidecar binary.

This file is the --onefile entry point for PyInstaller. It wraps
run_agent.py's AIAgent with sidecar-specific setup:

  1. Ensures HERMES_HOME env var is set (Rust bridges this, but default for dev)
  2. Strict IPC: **only** newline-delimited JSON on stdout; logging/diagnostics on stderr
  3. Async stdin loop (executor-based readline for Windows pipe compatibility)
  4. Graceful shutdown on ``{"type":"control","command":"shutdown"}`` or stdin EOF

UTF-8 bootstrap is handled automatically by run_agent.py importing
hermes_bootstrap as its first import -- no additional setup needed.

QUIET MODE: The AIAgent is created with verbose=False to suppress
Rich ANSI output on stdout. This is critical for the Rust Tauri layer.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import sys
import threading
import time
from typing import Any, Dict, Optional

# ---------------------------------------------------------------------------
# Stdout discipline: every JSON line must be serialized through send_json_rpc.
# ---------------------------------------------------------------------------

_stdout_lock = threading.Lock()


def _configure_logging_to_stderr_only() -> None:
    """Route Python logging and warnings to stderr only (never stdout)."""
    logging.captureWarnings(True)
    root = logging.getLogger()
    for h in root.handlers[:]:
        root.removeHandler(h)
    handler = logging.StreamHandler(sys.stderr)
    handler.setFormatter(logging.Formatter("%(levelname)s %(name)s: %(message)s"))
    root.addHandler(handler)
    root.setLevel(logging.INFO)


# Add project root to sys.path for core modules
_ROOT_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
if _ROOT_DIR not in sys.path:
    sys.path.insert(0, _ROOT_DIR)


def send_json_rpc(
    msg_id: Optional[Any],
    result: Optional[Any] = None,
    error: Optional[Any] = None,
    method: Optional[str] = None,
    params: Optional[Any] = None,
) -> None:
    """
    Write a JSON-RPC 2.0 message to stdout (thread-safe).
    """
    payload: Dict[str, Any] = {
        "jsonrpc": "2.0",
        "timestamp": time.time(),
    }
    if msg_id is not None:
        payload["id"] = msg_id

    if method is not None:
        payload["method"] = method
        if params is not None:
            payload["params"] = params
    elif error is not None:
        payload["error"] = error
    else:
        payload["result"] = result

    line = json.dumps(payload, ensure_ascii=False) + "\n"
    with _stdout_lock:
        sys.stdout.write(line)
        sys.stdout.flush()


async def _read_stdin_line() -> str:
    """Non-blocking stdin readline for asyncio (required on Windows pipes)."""
    loop = asyncio.get_running_loop()
    return await loop.run_in_executor(None, sys.stdin.readline)


def _close_session_db(agent: Any) -> None:
    """End session row and close SQLite (release WAL locks)."""
    db = getattr(agent, "_session_db", None)
    sid = getattr(agent, "session_id", None)
    if db is None:
        return
    try:
        if sid and getattr(db, "end_session", None):
            db.end_session(sid, "sidecar_shutdown")
    except Exception as e:
        logging.debug("end_session during sidecar shutdown: %s", e)
    try:
        if hasattr(db, "close"):
            db.close()
    except Exception as e:
        logging.debug("SessionDB.close during sidecar shutdown: %s", e)


def _shutdown_sidecar(agent: Any, msg_id: Optional[Any], reason: str) -> None:
    """
    Cooperative teardown then ``sys.exit(0)``.
    Order: interrupt in-flight agent work → persist session end → close DB → agent.close().
    """
    logging.info("Sidecar graceful shutdown (%s)", reason)
    try:
        agent.interrupt()
    except Exception as e:
        logging.debug("agent.interrupt during shutdown: %s", e)

    _close_session_db(agent)

    try:
        agent.close()
    except Exception as e:
        logging.debug("agent.close during shutdown: %s", e)

    try:
        from agent.auxiliary_client import shutdown_cached_clients

        shutdown_cached_clients()
    except Exception as e:
        logging.debug("shutdown_cached_clients: %s", e)

    try:
        from hermes_cli.plugins import invoke_hook

        invoke_hook(
            "on_session_finalize",
            session_id=getattr(agent, "session_id", None),
            platform="sidecar",
        )
    except Exception as e:
        logging.debug("on_session_finalize hook: %s", e)

    try:
        send_json_rpc(msg_id, result={"status": "shutdown", "reason": reason})
    except Exception:
        pass
    try:
        sys.stderr.flush()
        sys.stdout.flush()
    except Exception:
        pass
    sys.exit(0)


async def _run_chat_task(
    agent: Any,
    msg_id: Any,
    query: str,
) -> None:
    """Runs blocking ``run_conversation`` in a worker thread; streams via callback."""

    def stream_callback(delta: str) -> None:
        send_json_rpc(
            None,
            method="event",
            params={"type": "message.delta", "payload": delta},
        )

    loop = asyncio.get_running_loop()

    def sync_chat() -> Dict[str, Any]:
        return agent.run_conversation(query, stream_callback=stream_callback)

    try:
        send_json_rpc(None, method="status", params="processing")
        result = await loop.run_in_executor(None, sync_chat)
        send_json_rpc(
            msg_id,
            result={
                "final_text": result.get("final_response"),
                "completed": result.get("completed"),
                "api_calls": result.get("api_calls"),
                "last_reasoning": result.get("last_reasoning"),
            },
        )
        send_json_rpc(None, method="status", params="ready")
    except asyncio.CancelledError:
        send_json_rpc(
            msg_id,
            error={
                "code": -32800,
                "message": "Request cancelled (sidecar shutdown)",
            },
        )
        send_json_rpc(None, method="status", params="ready")
        raise
    except Exception as e:
        logging.exception("run_conversation failed")
        send_json_rpc(
            msg_id,
            error={"code": -32603, "message": str(e)},
        )
        send_json_rpc(None, method="status", params="ready")


async def _async_main() -> None:
    _configure_logging_to_stderr_only()

    if not os.environ.get("HERMES_HOME"):
        from hermes_constants import get_hermes_home

        os.environ.setdefault("HERMES_HOME", str(get_hermes_home()))

    send_json_rpc(
        None,
        method="sidecar_hello",
        params={
            "protocol_version": 2,
            "hermes_version": os.environ.get("HERMES_SIDECAR_VERSION", "0.13.0"),
            "build_time": __import__("datetime").datetime.now().isoformat(),
            "target": "x86_64-pc-windows-msvc",
        },
    )

    if os.environ.get("HERMES_SIDECAR_MOCK") == "1":

        class MockAgent:
            def interrupt(self, message: Optional[str] = None) -> None:
                pass

            def close(self) -> None:
                pass

            def run_conversation(self, query: str, stream_callback=None):
                if stream_callback:
                    stream_callback("Mock token 1")
                    stream_callback("Mock token 2")
                return {
                    "final_response": f"Mock response to: {query}",
                    "completed": True,
                    "api_calls": 0,
                    "last_reasoning": "Mock reasoning",
                }

        agent: Any = MockAgent()
    else:
        from contextlib import redirect_stdout

        with redirect_stdout(sys.stderr):
            from run_agent import AIAgent

            agent = AIAgent(
                model="",
                max_iterations=90,
                verbose_logging=False,
                quiet_mode=True,
            )

    send_json_rpc(None, method="sidecar_ready")

    active_chat: Optional[asyncio.Task] = None

    def _on_chat_done(t: asyncio.Task) -> None:
        try:
            exc = t.exception()
            if exc:
                logging.error("chat task failed: %s", exc)
        except asyncio.CancelledError:
            pass
        except Exception:
            logging.exception("chat task completion error")

    while True:
        try:
            line = await _read_stdin_line()
        except (EOFError, KeyboardInterrupt):
            line = ""

        if line == "":
            logging.info("stdin closed (EOF); shutting down sidecar")
            if active_chat and not active_chat.done():
                active_chat.cancel()
                try:
                    await active_chat
                except asyncio.CancelledError:
                    pass
            _shutdown_sidecar(agent, None, "stdin_eof")
            return

        raw = line.strip()
        if not raw:
            continue

        try:
            request = json.loads(raw)
        except json.JSONDecodeError:
            send_json_rpc(None, error={"code": -32700, "message": "Parse error"})
            continue

        if not isinstance(request, dict):
            send_json_rpc(None, error={"code": -32600, "message": "Invalid Request"})
            continue

        # --- Control plane (host-initiated shutdown) ---
        if request.get("type") == "control" and request.get("command") == "shutdown":
            if active_chat and not active_chat.done():
                active_chat.cancel()
                try:
                    await active_chat
                except asyncio.CancelledError:
                    pass
            _shutdown_sidecar(agent, request.get("id"), "control_shutdown")
            return

        msg_id = request.get("id")
        method = request.get("method")
        params = request.get("params", {})

        if method == "chat":
            query = params.get("query") if isinstance(params, dict) else None
            if not query:
                send_json_rpc(
                    msg_id,
                    error={"code": -32602, "message": "Missing 'query' parameter"},
                )
                continue

            if active_chat and not active_chat.done():
                send_json_rpc(
                    msg_id,
                    error={
                        "code": -32000,
                        "message": "Agent busy; wait for the current turn to finish",
                    },
                )
                continue

            active_chat = asyncio.create_task(_run_chat_task(agent, msg_id, query))
            active_chat.add_done_callback(_on_chat_done)

        elif method == "status":
            send_json_rpc(msg_id, result="ready")

        elif method == "exit":
            send_json_rpc(msg_id, result="exiting")
            if active_chat and not active_chat.done():
                active_chat.cancel()
                try:
                    await active_chat
                except asyncio.CancelledError:
                    pass
            _shutdown_sidecar(agent, msg_id, "method_exit")
            return

        else:
            send_json_rpc(
                msg_id,
                error={"code": -32601, "message": f"Method not found: {method}"},
            )


def main() -> None:
    asyncio.run(_async_main())


if __name__ == "__main__":
    main()
