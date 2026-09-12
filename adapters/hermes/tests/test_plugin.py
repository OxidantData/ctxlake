"""Tests for the ctxlake Hermes plugin.

Two properties matter more here than anywhere else in this wave, because a Hermes
plugin runs *inside* the host agent's own process: a bug must never raise into that
process (`test_handler_exception_never_propagates`), and this plugin must never spawn
`ctxlake-hook` — that would mean a subprocess per LLM call, exactly the cost the
binary exists to avoid (`test_no_subprocess_is_ever_spawned`).
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

import hermes as plugin
import pytest


class FakeCtx:
    """Stands in for whatever object Hermes passes to `register()`; records which
    event names got a handler, and lets tests invoke them directly."""

    def __init__(self) -> None:
        self.handlers: dict[str, object] = {}

    def register_hook(self, event_name: str, handler: object) -> None:
        self.handlers[event_name] = handler


@pytest.fixture
def ctx(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> FakeCtx:
    monkeypatch.setenv("CTXLAKE_SPOOL_DIR", str(tmp_path / "spool"))
    monkeypatch.setenv("CTXLAKE_HOOK_ERROR_LOG", str(tmp_path / "hook-errors.log"))
    monkeypatch.setenv("CTXLAKE_FLEET_ID", "test-fleet")
    monkeypatch.setenv("CTXLAKE_AGENT_ID", "test-agent")
    c = FakeCtx()
    plugin.register(c)
    return c


def spool_lines(tmp_path: Path, session_id: str) -> list[dict]:
    path = tmp_path / "spool" / "hermes" / f"{session_id}.ndjson"
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line]


def test_register_registers_every_documented_event(ctx: FakeCtx) -> None:
    assert set(ctx.handlers.keys()) == set(plugin.EVENTS)


def test_hermes_has_no_compaction_event_and_this_is_not_papered_over() -> None:
    # Documents the known gap cited in __init__.py and the wave-1 task brief, rather
    # than silently mapping something else onto "compact".
    assert not any("compact" in e for e in plugin.EVENTS)


def test_on_session_start_maps_to_session_start(ctx: FakeCtx, tmp_path: Path) -> None:
    ctx.handlers["on_session_start"](session_id="s1", model="claude-opus", platform="cli")
    lines = spool_lines(tmp_path, "s1")
    assert len(lines) == 1
    env = lines[0]
    assert env["event_type"] == "session_start"
    assert env["session_id"] == "s1"
    assert env["runtime"] == "hermes"
    assert env["fleet_id"] == "test-fleet"
    assert env["agent_id"] == "test-agent"
    assert env["runtime_version"] == "cli"
    assert "content" not in env, "session_start has no content — must be omitted, not null"


def test_pre_llm_call_maps_prompt(ctx: FakeCtx, tmp_path: Path) -> None:
    ctx.handlers["pre_llm_call"](session_id="s1", user_message="fix the redactor", is_first_turn=True, model="claude-opus", platform="cli")
    env = spool_lines(tmp_path, "s1")[0]
    assert env["event_type"] == "prompt"
    assert env["role"] == "user"
    assert env["content"] == "fix the redactor"
    assert env["usage"]["model"] == "claude-opus"


def test_post_llm_call_maps_assistant(ctx: FakeCtx, tmp_path: Path) -> None:
    ctx.handlers["post_llm_call"](session_id="s1", user_message="fix it", assistant_response="done, see the diff", model="claude-opus", platform="cli")
    env = spool_lines(tmp_path, "s1")[0]
    assert env["event_type"] == "assistant"
    assert env["role"] == "assistant"
    assert env["content"] == "done, see the diff"


def test_pre_and_post_tool_call_join_on_tool_call_id(ctx: FakeCtx, tmp_path: Path) -> None:
    ctx.handlers["pre_tool_call"](session_id="s1", task_id="task-1", tool_call_id="tc-1", tool_name="bash", args={"command": "ls"})
    ctx.handlers["post_tool_call"](session_id="s1", task_id="task-1", tool_call_id="tc-1", tool_name="bash", args={"command": "ls"}, result="README.md", duration_ms=12)
    lines = spool_lines(tmp_path, "s1")
    assert len(lines) == 2
    pre, post = lines
    assert pre["event_type"] == "tool_call"
    assert pre["message_id"] == "tc-1" == post["message_id"]
    assert pre["turn_id"] == "task-1" == post["turn_id"]
    assert pre["tool"]["name"] == "bash"
    assert "result" not in pre["tool"], "pre_tool_call has no result yet — must be omitted"
    assert post["tool"]["result"] == "README.md"
    assert post["tool"]["duration_ms"] == 12


def test_approval_events_use_the_orca_status_approval_convention(ctx: FakeCtx, tmp_path: Path) -> None:
    ctx.handlers["pre_approval_request"](session_id="s1", command="rm -rf build", description="clean build dir")
    ctx.handlers["post_approval_response"](session_id="s1", command="rm -rf build", description="clean build dir", choice="approved")
    lines = spool_lines(tmp_path, "s1")
    assert lines[0]["tool"]["name"] == "approval"
    assert json.loads(lines[0]["tool"]["input"]) == {"command": "rm -rf build", "description": "clean build dir"}
    assert lines[1]["tool"]["result"] == "approved"


def test_session_end_and_finalize_both_map_to_session_end_and_write_done(ctx: FakeCtx, tmp_path: Path) -> None:
    ctx.handlers["on_session_end"](session_id="s1")
    ctx.handlers["on_session_finalize"](session_id="s1", platform="cli")
    lines = spool_lines(tmp_path, "s1")
    assert [line["event_type"] for line in lines] == ["session_end", "session_end"]
    assert (tmp_path / "spool" / "hermes" / "s1.done").exists()


def test_session_reset_maps_to_session_start(ctx: FakeCtx, tmp_path: Path) -> None:
    ctx.handlers["on_session_reset"](session_id="s1", platform="cli")
    env = spool_lines(tmp_path, "s1")[0]
    assert env["event_type"] == "session_start"


def test_a_leaked_secret_never_reaches_the_spool(ctx: FakeCtx, tmp_path: Path) -> None:
    ctx.handlers["pre_tool_call"](
        session_id="s1",
        task_id="task-1",
        tool_call_id="tc-1",
        tool_name="bash",
        args={"command": "cat .env"},
    )
    ctx.handlers["post_tool_call"](
        session_id="s1",
        task_id="task-1",
        tool_call_id="tc-1",
        tool_name="bash",
        args={"command": "cat .env"},
        result="AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
        duration_ms=5,
    )
    raw = (tmp_path / "spool" / "hermes" / "s1.ndjson").read_text()
    assert "AKIAIOSFODNN7EXAMPLE" not in raw, f"a real secret reached the spool file: {raw}"
    lines = spool_lines(tmp_path, "s1")
    assert lines[1]["redaction"]["status"] == "quarantined"
    assert "aws_access_key_id" in lines[1]["redaction"]["rules_fired"]


def test_reading_a_denied_path_withholds_result_but_keeps_the_call(ctx: FakeCtx, tmp_path: Path) -> None:
    ctx.handlers["post_tool_call"](
        session_id="s1",
        task_id="task-1",
        tool_call_id="tc-1",
        tool_name="read_file",
        args={"file_path": "/Users/x/.aws/credentials"},
        result="[default]\naws_access_key_id=AKIAIOSFODNN7EXAMPLE",
        duration_ms=1,
    )
    env = spool_lines(tmp_path, "s1")[0]
    assert env["tool"]["name"] == "read_file", "the call itself is still recorded"
    assert "withheld" in env["tool"]["result"]
    assert "AKIA" not in env["tool"]["result"]
    assert env["redaction"]["status"] == "quarantined"
    assert env["tool"]["paths"] == ["/Users/x/.aws/credentials"]


def test_writing_a_secret_to_a_denied_path_does_not_leak_via_input(ctx: FakeCtx, tmp_path: Path) -> None:
    # Regression: the path denylist was applied only to the tool *result*, never to
    # *args* — so a write to a denylisted path stored the secret verbatim in
    # `tool.input` while `redaction.status` said "quarantined" (fired only by the
    # unrelated, secret-free result).
    secret = "qV3kRt8zLmNp0XyW7bHfJ2sD4gUe6AcZ1oIl5TnB"
    ctx.handlers["post_tool_call"](
        session_id="s1",
        task_id="task-1",
        tool_call_id="tc-1",
        tool_name="write_file",
        args={
            "file_path": "/Users/dev/.aws/credentials",
            "content": f"[default]\naws_access_key={secret}",
        },
        result="wrote 2 lines",
        duration_ms=1,
    )
    raw = (tmp_path / "spool" / "hermes" / "s1.ndjson").read_text()
    assert secret not in raw, f"the secret written to a denylisted path leaked into the spool: {raw}"
    env = spool_lines(tmp_path, "s1")[0]
    assert env["redaction"]["status"] == "quarantined"


def test_session_id_with_path_traversal_is_rejected_and_writes_nothing_outside_root(
    ctx: FakeCtx, tmp_path: Path
) -> None:
    # Regression: `append_event` used to join `session_id` into a path with no
    # validation, so `<spool_root>/hermes/../../pwned.ndjson` — one level above
    # `spool_root` — was a real file `on_session_start(session_id="../../pwned")`
    # could produce.
    escape_target = tmp_path / "pwned.ndjson"
    ctx.handlers["on_session_start"](session_id="../../pwned", model="m", platform="cli")
    assert not escape_target.exists(), f"wrote outside the spool root: {escape_target}"
    # The plugin must never raise into the host process even so; the failure lands in
    # the error log instead (same contract as `test_handler_exception_never_propagates`).
    error_log = (tmp_path / "hook-errors.log").read_text()
    assert "unsafe session_id" in error_log


def test_spool_append_event_rejects_unsafe_session_ids_directly(tmp_path: Path) -> None:
    from hermes import _spool

    for bad in ("../../pwned", "..", "a/b", "a\\b", ""):
        with pytest.raises(ValueError):
            _spool.append_event(tmp_path, bad, "line")
    assert not any(tmp_path.rglob("*.ndjson")), "no file should have been created"


def test_spool_size_check_uses_the_tracked_sidecar_not_a_live_directory_scan(tmp_path: Path) -> None:
    # Regression: the cap check used to call `_dir_size`, which stats every file in
    # the runtime directory on every event — an O(files) syscall storm on a path this
    # plugin runs once per LLM/tool call, with a file count that only grows (wave 1
    # ships no daemon to drain it). Seed a small tracked size and plant a decoy file
    # that would blow the cap if anything actually scanned the directory for real
    # bytes on disk; the append must succeed because it trusts the sidecar.
    from hermes import _spool

    run_dir = tmp_path / "hermes"
    run_dir.mkdir(parents=True)
    (run_dir / "._spool_size_placeholder").write_bytes(b"")  # keep dir non-empty
    (run_dir / ".spool_size").write_text("10")
    (run_dir / "decoy.ndjson").write_bytes(b"x" * (_spool.MAX_RUNTIME_DIR_BYTES + 1))

    _spool.append_event(tmp_path, "sess-1", "line")

    assert (run_dir / "sess-1.ndjson").exists(), (
        "the tracked sidecar size (10 bytes), not a scan of the decoy file, must "
        "decide whether the cap is exceeded"
    )


def test_spool_size_tracking_self_heals_from_a_missing_sidecar(tmp_path: Path) -> None:
    from hermes import _spool

    run_dir = tmp_path / "hermes"
    run_dir.mkdir(parents=True)
    (run_dir / "huge.ndjson").write_bytes(b"x" * (_spool.MAX_RUNTIME_DIR_BYTES + 1))

    _spool.append_event(tmp_path, "new-session", "dropped")

    assert not (run_dir / "new-session.ndjson").exists()
    assert (run_dir / ".spool_size").exists(), "the scan's result should be persisted"


def test_spool_ordinary_session_ids_still_work(tmp_path: Path) -> None:
    from hermes import _spool

    _spool.append_event(tmp_path, "8f3e-a1.2", "line")
    assert (tmp_path / "hermes" / "8f3e-a1.2.ndjson").read_text() == "line\n"


def test_handler_exception_never_propagates(ctx: FakeCtx, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A Hermes plugin runs inside the host agent's process; a bug in capture must
    degrade to "this event wasn't captured", never to a broken turn."""

    def boom(*args: object, **kwargs: object) -> None:
        raise RuntimeError("disk exploded")

    monkeypatch.setattr(plugin._spool, "append_event", boom)
    # Must not raise:
    ctx.handlers["on_session_start"](session_id="s1", model="m", platform="cli")

    # And the failure must have gone somewhere a human can find it, not vanished.
    error_log = (tmp_path / "hook-errors.log").read_text()
    assert "disk exploded" in error_log


def test_handler_exception_from_a_malformed_payload_never_propagates(ctx: FakeCtx) -> None:
    # `args` for a tool event is documented as a dict; a caller handing something else
    # must not crash the host process even though this plugin cannot make sense of it.
    ctx.handlers["pre_tool_call"](session_id="s1", task_id="t1", tool_call_id="tc1", tool_name="bash", args="not-a-dict")
    ctx.handlers["post_tool_call"](session_id=None)  # missing almost everything


def test_no_subprocess_is_ever_spawned(ctx: FakeCtx, monkeypatch: pytest.MonkeyPatch) -> None:
    """The entire reason this plugin exists in Python rather than shelling out to
    `ctxlake-hook`: a subprocess per LLM call would blow the hook's own latency budget."""

    def forbidden(*args: object, **kwargs: object) -> None:
        raise AssertionError(f"a subprocess was spawned: args={args!r} kwargs={kwargs!r}")

    monkeypatch.setattr(subprocess, "Popen", forbidden)
    monkeypatch.setattr(subprocess, "run", forbidden)
    monkeypatch.setattr(subprocess, "call", forbidden)
    monkeypatch.setattr(os, "system", forbidden)
    if hasattr(os, "posix_spawn"):
        monkeypatch.setattr(os, "posix_spawn", forbidden)

    ctx.handlers["on_session_start"](session_id="s1", model="m", platform="cli")
    ctx.handlers["pre_llm_call"](session_id="s1", user_message="hi", is_first_turn=True, model="m", platform="cli")
    ctx.handlers["post_llm_call"](session_id="s1", user_message="hi", assistant_response="hello", model="m", platform="cli")
    ctx.handlers["pre_tool_call"](session_id="s1", task_id="t1", tool_call_id="tc1", tool_name="bash", args={"command": "ls"})
    ctx.handlers["post_tool_call"](session_id="s1", task_id="t1", tool_call_id="tc1", tool_name="bash", args={"command": "ls"}, result="ok", duration_ms=1)
    ctx.handlers["pre_approval_request"](session_id="s1", command="rm x", description="d")
    ctx.handlers["post_approval_response"](session_id="s1", command="rm x", description="d", choice="approved")
    ctx.handlers["on_session_end"](session_id="s1")
    ctx.handlers["on_session_finalize"](session_id="s1", platform="cli")
    ctx.handlers["on_session_reset"](session_id="s1", platform="cli")
    # No assertion beyond "nothing above raised" — `forbidden` would have raised if
    # any handler had spawned a process.


def test_ulid_is_26_crockford_base32_characters() -> None:
    from hermes import _ulid

    u = _ulid.new_ulid()
    assert len(u) == 26
    assert all(c in "0123456789ABCDEFGHJKMNPQRSTVWXYZ" for c in u)
    assert _ulid.new_ulid() != u, "two calls must not collide in practice"


def test_ulid_is_monotonic_within_a_process() -> None:
    # Regression: `new_ulid()` drew fresh random bits on every call, so two ids
    # minted in the same millisecond sorted arbitrarily instead of in call order.
    # Hermes runs in-process and mints ids back-to-back (a `pre_tool_call` immediately
    # followed by its `post_tool_call`), which lands in the same millisecond often
    # enough that this was not a hypothetical: 1000 successive calls produced hundreds
    # of out-of-order adjacent pairs before this fix.
    from hermes import _ulid

    ids = [_ulid.new_ulid() for _ in range(1000)]
    out_of_order = [(a, b) for a, b in zip(ids, ids[1:]) if not a < b]
    assert not out_of_order, f"{len(out_of_order)} adjacent pair(s) out of order: {out_of_order[:3]}"


def test_content_hash_matches_the_rust_side_format() -> None:
    from hermes import _redact

    h = _redact.content_hash("abc")
    assert h.startswith("sha256:")
    assert len(h) == len("sha256:") + 64
    # Known SHA-256("abc") — cross-checked against `sha256sum` / the Rust test in
    # hash.rs, so a hashing-format drift between the two languages fails loudly here.
    assert h == "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"


def test_conftest_put_the_plugin_on_sys_path() -> None:
    # Sanity check for the test harness itself: if this ever fails, every test above
    # would have failed at collection with an ImportError instead, which is a much
    # more confusing failure to debug.
    assert "hermes" in sys.modules
