# ctxlake Hermes plugin

An in-process Hermes plugin that captures agent lifecycle events into the local
ctxlake spool. Unlike the Claude Code and Cursor adapters, this is **not** a wrapper
around `ctxlake-hook` — it never spawns that binary, because Hermes fires
`pre_llm_call`/`post_llm_call` in-process on every LLM call, and a subprocess there
would reintroduce exactly the latency cost `ctxlake-hook` exists to avoid.

## Layout

Mirrors `~/.hermes/plugins/orca-status/`, the plugin convention observed on a live
Hermes install:

```
adapters/hermes/
├── plugin.yaml     # name, version, provides_hooks
├── __init__.py     # register(ctx) + per-event handlers
├── _redact.py       # secret markers / deny-paths / entropy — ported from
│                     # crates/ctxlake-core/src/redact.rs, hand-kept in sync
├── _spool.py         # same NDJSON-file-per-session layout as ctxlake-hook's spool
├── _ulid.py           # stdlib-only ULID generator (no Rust `ulid` crate available here)
└── tests/
    ├── conftest.py
    └── test_plugin.py
```

Only the Python standard library is used — a Hermes plugin loads into someone else's
interpreter, and `pip install`ing a dependency into it isn't ours to decide.

## Install

Copy this directory to `~/.hermes/plugins/ctxlake/` (a later wave's `ctxlake install`
will do this automatically, the same way it merges hooks into Claude Code and Cursor's
config). Configure identity with the same environment variables `ctxlake-hook` reads:

| Variable | Meaning | Default |
|---|---|---|
| `CTXLAKE_FLEET_ID` | fleet identifier | `unconfigured-fleet` |
| `CTXLAKE_AGENT_ID` | logical agent identity | `unconfigured-agent` |
| `CTXLAKE_SPOOL_DIR` | spool root | `~/.ctxlake/spool` |
| `CTXLAKE_HOOK_ERROR_LOG` | where capture failures are logged | `~/.ctxlake/hook-errors.log` |

## Known gaps

- **No compaction event.** Hermes has no equivalent of Claude Code's `PreCompact` /
  Cursor's `preCompact`; Hermes sessions simply have a gap in `EventType::Compact`
  coverage. Not papered over with a guess.
- **`on_session_end` and `on_session_finalize` both map to `EventType::SessionEnd`.**
  The schema has one "session ended" variant; Hermes fires two events for it.
- **`on_session_reset` maps to `EventType::SessionStart`** as the closest existing
  category for "a fresh context begins" — the same kind of judgment call as Cursor's
  `stop`/`afterAgentResponse` both landing on `Assistant` (see `adapters/cursor.rs`).
- **Per-event stdin field names for `pre_approval_request` / `post_approval_response`**
  come from the installed `orca-status` plugin's own `SELECTED_KEYS`, not from the
  task brief's payload table (which didn't list them) — the `tool_name: "approval"`
  convention is reused from that same live install rather than invented.

## Testing

```sh
python3 -m venv .venv && .venv/bin/pip install pytest
.venv/bin/python -m pytest adapters/hermes/tests/
```
