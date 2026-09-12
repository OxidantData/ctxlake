# Managed by ctxlake. The local spool, mirroring crates/ctxlake-hook/src/spool.rs.
#
# Layout is identical on purpose: `<spool_root>/hermes/<session_id>.ndjson`, rotated
# at MAX_SPOOL_FILE_BYTES, marked done with a sibling `.done` file. The daemon (a
# later wave) drains all three runtimes' spool directories the same way, so Hermes
# must land its lines in the same shape ctxlake-hook does — see that module's docs
# for the concurrency reasoning (one `write()` syscall per line under O_APPEND is
# atomic across writers on POSIX; this plugin runs in-process, so "concurrent
# writers" here means two Hermes sessions on one machine, not parallel tool calls
# within one session the way Claude Code/Cursor can produce).
from __future__ import annotations

import os
import sys
import time
from pathlib import Path

MAX_SPOOL_FILE_BYTES = 32 * 1024 * 1024
MAX_RUNTIME_DIR_BYTES = 512 * 1024 * 1024


def spool_root() -> Path:
    override = os.environ.get("CTXLAKE_SPOOL_DIR")
    if override:
        return Path(override)
    return Path(os.environ.get("HOME", ".")) / ".ctxlake" / "spool"


def error_log_path() -> Path:
    override = os.environ.get("CTXLAKE_HOOK_ERROR_LOG")
    if override:
        return Path(override)
    return Path(os.environ.get("HOME", ".")) / ".ctxlake" / "hook-errors.log"


def append_event(root: Path, session_id: str, line: str) -> None:
    """Append one line to `hermes/<session_id>.ndjson` under `root`. Best-effort: any
    I/O failure is swallowed here (the caller is a Hermes lifecycle hook, and this
    plugin's contract — see __init__.py — is to never raise into the host process).
    """
    run_dir = root / "hermes"
    try:
        run_dir.mkdir(parents=True, exist_ok=True)
    except OSError:
        return

    if _dir_size(run_dir) >= MAX_RUNTIME_DIR_BYTES:
        print(
            f"ctxlake (hermes plugin): WARNING spool dir {run_dir} is at or over "
            f"{MAX_RUNTIME_DIR_BYTES} bytes; dropping event rather than filling the disk",
            file=sys.stderr,
        )
        return

    path = run_dir / f"{session_id}.ndjson"
    _rotate_if_full(path)

    data = (line + "\n").encode("utf-8")
    try:
        # O_APPEND + a single write() call: see module docstring for why this is the
        # atomicity boundary, same as the Rust spool.
        fd = os.open(path, os.O_APPEND | os.O_CREAT | os.O_WRONLY, 0o600)
        try:
            os.write(fd, data)
        finally:
            os.close(fd)
    except OSError:
        return


def mark_session_done(root: Path, session_id: str) -> None:
    run_dir = root / "hermes"
    try:
        run_dir.mkdir(parents=True, exist_ok=True)
        (run_dir / f"{session_id}.done").touch()
    except OSError:
        pass


def log_error(msg: str) -> None:
    path = error_log_path()
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        with open(path, "a", encoding="utf-8") as f:
            f.write(f"{int(time.time() * 1000)} {msg}\n")
    except OSError:
        pass


def _rotate_if_full(path: Path) -> None:
    try:
        size = path.stat().st_size
    except OSError:
        return  # no file yet — nothing to rotate.
    if size < MAX_SPOOL_FILE_BYTES:
        return
    suffix = int(time.time() * 1_000_000)
    rotated = path.with_suffix(f".ndjson.{suffix}")
    try:
        path.rename(rotated)
    except OSError:
        pass  # another writer already rotated it — fine, see spool.rs's docstring.


def _dir_size(dir_path: Path) -> int:
    total = 0
    try:
        with os.scandir(dir_path) as entries:
            for entry in entries:
                try:
                    if entry.is_file():
                        total += entry.stat().st_size
                except OSError:
                    continue
    except OSError:
        return 0
    return total
