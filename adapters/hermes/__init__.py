# ctxlake capture plugin for Hermes.
#
# Convention (`register(ctx): ctx.register_hook(event_name, handler)`, handlers taking
# `**kwargs`) copied from ~/.hermes/plugins/orca-status/__init__.py, the only in-process
# Hermes plugin observed on a live install — trust it over any docs, per the wave-1
# task brief.
#
# CRITICAL DIFFERENCE FROM ctxlake-hook: this module runs IN-PROCESS inside the host's
# Python interpreter, once per LLM call and once per tool call. It must never spawn
# `ctxlake-hook` (a subprocess per LLM call would itself blow the latency budget the
# hook binary exists to protect) and it must never raise — every handler is wrapped so
# a bug here degrades to "this event wasn't captured", not "the user's turn broke".
#
# Hermes has no compaction event. `EventType::Compact`'s counterpart on the Rust
# schema is simply never emitted here — Hermes sessions have a gap in that dimension
# that Claude Code's and Cursor's don't. Documented, not faked.
from __future__ import annotations

import os
import socket
from datetime import datetime, timezone
from typing import Any, Callable

from . import _redact, _spool, _ulid

SCHEMA_VERSION = 1  # must track crates/ctxlake-core/src/envelope.rs::SCHEMA_VERSION

EVENTS = [
    "on_session_start",
    "pre_llm_call",
    "post_llm_call",
    "pre_tool_call",
    "post_tool_call",
    "pre_approval_request",
    "post_approval_response",
    "on_session_end",
    "on_session_finalize",
    "on_session_reset",
]

# Bound payload traversal the way orca-status does (hook args/results can carry an
# entire tool payload): depth, item count, and string length are all capped before
# anything is turned into text this plugin might hash or store.
MAX_JSONABLE_DEPTH = 5
MAX_JSONABLE_ITEMS = 50
MAX_JSONABLE_NODES = 500
MAX_JSONABLE_STRING = 8192
_TRUNCATED = "...[truncated]"


def _fleet_id() -> str:
    v = os.environ.get("CTXLAKE_FLEET_ID", "")
    return v if v else "unconfigured-fleet"


def _agent_id() -> str:
    v = os.environ.get("CTXLAKE_AGENT_ID", "")
    return v if v else "unconfigured-agent"


def _host_id() -> str:
    try:
        hostname = socket.gethostname()
    except OSError:
        hostname = "unknown-host"
    return _redact.content_hash(hostname)


def _now_rfc3339() -> str:
    now = datetime.now(timezone.utc)
    return now.strftime("%Y-%m-%dT%H:%M:%S.") + f"{now.microsecond // 1000:03d}Z"


def _truncate(s: str) -> str:
    if len(s) <= MAX_JSONABLE_STRING:
        return s
    return s[:MAX_JSONABLE_STRING] + _TRUNCATED


def _jsonable(value: Any, depth: int = 0, budget: list[int] | None = None) -> Any:
    """Bound an arbitrary Python value before it is turned into text — copied
    verbatim in spirit from orca-status's `_jsonable`, which exists for exactly the
    reason cited there: hook args can carry an entire tool payload, and this plugin
    is best-effort, not a general serializer."""
    if budget is None:
        budget = [MAX_JSONABLE_NODES]
    if budget[0] <= 0:
        return _TRUNCATED
    budget[0] -= 1
    if depth > MAX_JSONABLE_DEPTH:
        return _truncate(repr(value))
    if value is None or isinstance(value, (int, float, bool)):
        return value
    if isinstance(value, str):
        return _truncate(value)
    if isinstance(value, dict):
        out: dict[str, Any] = {}
        for index, (k, v) in enumerate(value.items()):
            if index >= MAX_JSONABLE_ITEMS:
                out[_TRUNCATED] = True
                break
            out[_truncate(str(k))] = _jsonable(v, depth + 1, budget)
        return out
    if isinstance(value, (list, tuple, set)):
        out_list = []
        for index, item in enumerate(value):
            if index >= MAX_JSONABLE_ITEMS:
                out_list.append(_TRUNCATED)
                break
            out_list.append(_jsonable(item, depth + 1, budget))
        return out_list
    return _truncate(repr(value))


def _to_json_text(value: Any) -> str | None:
    if value is None:
        return None
    if isinstance(value, str):
        return _truncate(value)
    import json

    return _truncate(json.dumps(_jsonable(value), separators=(",", ":"), sort_keys=True))


class _RedactionAcc:
    """Same job as `adapters::common::RedactionAcc` on the Rust side: fold several
    fields' redaction outcomes into the one status/rules pair the envelope carries,
    with quarantine always winning."""

    def __init__(self) -> None:
        self.quarantined = False
        self.redacted = False
        self.rules: list[str] = []

    def record(self, outcome: _redact.Outcome) -> None:
        if outcome.status == "quarantined":
            self.quarantined = True
        elif outcome.status == "redacted":
            self.redacted = True
        for r in outcome.rules:
            if r not in self.rules:
                self.rules.append(r)

    def record_denied_path(self) -> None:
        self.quarantined = True
        if "denied_path" not in self.rules:
            self.rules.append("denied_path")

    def to_dict(self) -> dict[str, Any]:
        status = "quarantined" if self.quarantined else "redacted" if self.redacted else "clean"
        d: dict[str, Any] = {"status": status}
        if self.rules:
            d["rules_fired"] = self.rules
        return d


def _scrub(acc: _RedactionAcc, value: str | None, is_tool_output: bool) -> str | None:
    if value is None:
        return None
    outcome, out = _redact.scrub(value, is_tool_output)
    acc.record(outcome)
    return out


def _build_envelope(event_name: str, kwargs: dict[str, Any]) -> dict[str, Any]:
    session_id = str(kwargs.get("session_id") or "unknown-session")
    acc = _RedactionAcc()

    env: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "event_id": _ulid.new_ulid(),
        "emitted_at": _now_rfc3339(),
        "fleet_id": _fleet_id(),
        "agent_id": _agent_id(),
        "runtime": "hermes",
        "host_id": _host_id(),
        "session_id": session_id,
        "cwd": os.getcwd(),
    }

    # `platform` (e.g. "vscode", "cli") names the surface Hermes is embedded in —
    # there is no dedicated envelope slot for it, and `runtime_version` is the
    # closest fit (both describe "which build of this runtime produced the event").
    platform = kwargs.get("platform")
    if platform:
        env["runtime_version"] = str(platform)

    model = kwargs.get("model")

    if event_name == "on_session_start":
        env["event_type"] = "session_start"
    elif event_name == "pre_llm_call":
        env["event_type"] = "prompt"
        env["role"] = "user"
        env["content"] = _scrub(acc, _to_json_text(kwargs.get("user_message")), False)
    elif event_name == "post_llm_call":
        env["event_type"] = "assistant"
        env["role"] = "assistant"
        env["content"] = _scrub(acc, _to_json_text(kwargs.get("assistant_response")), False)
    elif event_name in ("pre_tool_call", "post_tool_call"):
        env["event_type"] = "tool_call"
        env["turn_id"] = kwargs.get("task_id")
        env["message_id"] = kwargs.get("tool_call_id")
        env["tool"] = _build_tool(acc, str(kwargs.get("tool_name") or "unknown"), kwargs.get("args"), kwargs.get("result") if event_name == "post_tool_call" else None, kwargs.get("duration_ms"))
    elif event_name in ("pre_approval_request", "post_approval_response"):
        # `tool_name="approval"` is orca-status's own convention for these two events
        # (see ~/.hermes/plugins/orca-status/__init__.py::_payload_for_event) —
        # reused rather than invented, since it is exactly what a live install does.
        env["event_type"] = "tool_call"
        approval_input = {"command": kwargs.get("command", ""), "description": kwargs.get("description", "")}
        result = kwargs.get("choice") if event_name == "post_approval_response" else None
        env["tool"] = _build_tool(acc, "approval", approval_input, result, None)
    elif event_name == "on_session_end":
        env["event_type"] = "session_end"
    elif event_name == "on_session_finalize":
        # Hermes fires both `on_session_end` and `on_session_finalize`; the schema has
        # one `SessionEnd` variant, not two, so both collapse onto it. Lossy and
        # documented rather than inventing a distinction the schema doesn't have.
        env["event_type"] = "session_end"
    elif event_name == "on_session_reset":
        # The closest existing category to "a fresh context begins" — see the same
        # reasoning applied to Cursor's `stop`/`afterAgentResponse` in adapters/cursor.rs.
        env["event_type"] = "session_start"
    else:
        raise ValueError(f"unknown hermes event: {event_name}")

    if model:
        env["usage"] = {"model": str(model), "input_tokens": 0, "output_tokens": 0, "cache_read_tokens": 0, "cache_write_tokens": 0}

    env["redaction"] = acc.to_dict()
    env["content_hash"] = _redact.content_hash(env.get("content") or "")
    return {k: v for k, v in env.items() if v is not None}


def _build_tool(acc: _RedactionAcc, name: str, args: Any, result: Any, duration_ms: Any) -> dict[str, Any]:
    input_text = _to_json_text(args)
    input_hash = _redact.content_hash(input_text or "")
    input_text = _scrub(acc, input_text, False)

    result_text = _to_json_text(result)
    path = _extract_path(args)
    result_text, denied = _spool_withhold(path, result_text)
    if denied:
        acc.record_denied_path()
    result_text = _scrub(acc, result_text, True)

    # Match the Rust envelope's `skip_serializing_if`: an absent optional field is a
    # missing key here too, never a JSON `null` — a nested dict isn't covered by
    # `_build_envelope`'s top-level `if v is not None` filter, so this dict is built
    # conditionally by hand instead.
    tool: dict[str, Any] = {"name": name, "input_hash": input_hash}
    if input_text is not None:
        tool["input"] = input_text
    if result_text is not None:
        tool["result"] = result_text
    if isinstance(duration_ms, (int, float)):
        tool["duration_ms"] = int(duration_ms)
    if path:
        tool["paths"] = [path]
    return tool


def _spool_withhold(path: str | None, result: str | None) -> tuple[str | None, bool]:
    return _redact.withhold_if_denied_path(path, result)


def _extract_path(args: Any) -> str | None:
    if isinstance(args, dict):
        for key in ("file_path", "path"):
            v = args.get(key)
            if isinstance(v, str):
                return v
    return None


def _make_hook(event_name: str) -> Callable[..., None]:
    def _hook(**kwargs: Any) -> None:
        # Best-effort, full stop: a bug in capture must never surface in the host
        # agent's own turn (same contract as ctxlake-hook's "exit 0 either way", the
        # in-process equivalent of which is "never let an exception escape").
        try:
            envelope = _build_envelope(event_name, kwargs)
            root = _spool.spool_root()
            import json

            line = json.dumps(envelope, separators=(",", ":"), sort_keys=True)
            _spool.append_event(root, envelope["session_id"], line)
            if envelope.get("event_type") == "session_end":
                _spool.mark_session_done(root, envelope["session_id"])
        except Exception as e:  # noqa: BLE001 - deliberately broad, see docstring above
            try:
                _spool.log_error(f"hermes plugin: {event_name} failed: {e}")
            except Exception:
                pass

    return _hook


def register(ctx: Any) -> None:
    for event_name in EVENTS:
        ctx.register_hook(event_name, _make_hook(event_name))
