# Hermes — `hooks:` in `config.yaml`

Hermes reaches full parity with Claude Code, and it gets there through a mechanism the
other runtimes do not have: **shell hooks declared in `~/.hermes/config.yaml`**, whose
wire contract is deliberately Claude Code-compatible.

That matters more than it sounds. It means ctxlake uses **the same `ctxlake-hook` binary**
on Hermes that it uses everywhere else — no Python plugin, no second redactor, no
second spool writer, and so no chance of the two drifting apart.

> **Verification basis:** every capability, field name and semantic below was read from
> Hermes's own source — `agent/shell_hooks.py`, `hermes_cli/plugins.py` (`VALID_HOOKS`),
> and `website/docs/user-guide/features/hooks.md`. This is the best-grounded of the three
> pages on *what Hermes does*.
>
> What is **not** yet true: ctxlake currently ships a Python-plugin adapter
> (`adapters/hermes/`) rather than the shell hooks described here. Shell hooks are the
> intended mechanism, for the reason in the next section, and the plugin is slated for
> removal. Until that lands, the config shown below is the target, not what
> `ctxlake install hermes` writes.

> **Verified against Hermes's own source**, not inferred: `agent/shell_hooks.py`,
> `hermes_cli/plugins.py` (`VALID_HOOKS`), and `website/docs/user-guide/features/hooks.md`.
> Field names, payload shape, blocking semantics, and the config schema below are read
> from the implementation.

## Two mechanisms, and why we use the shell one

Hermes supports both:

| | Shell hooks | Python plugins |
|---|---|---|
| Declared in | `~/.hermes/config.yaml` under `hooks:` | `~/.hermes/plugins/<name>/` |
| Our code | the `ctxlake-hook` binary | a `register(ctx)` module |
| Redaction | the same Rust implementation as every runtime | would have to be re-implemented |
| Ordering | after Python plugins | first |

Python plugins are the more powerful surface — they run in-process and see richer
arguments. We still choose shell hooks, because a second implementation of a *security
control* is a liability that outweighs the extra fidelity. Two redactors with no shared
source diverge silently: the Python half stops catching a pattern and every Hermes
session leaks it while Claude Code sessions stay clean, with nothing failing anywhere.

If you need the in-process surface for something else, the two coexist — Hermes runs
Python plugins first, then shell hooks, and the first valid directive wins.

## Configuration

```yaml
# ~/.hermes/config.yaml
hooks:
  on_session_start:
    - command: ctxlake-hook session_start --runtime hermes
  pre_llm_call:
    - command: ctxlake-hook prompt --runtime hermes
  pre_tool_call:
    - command: ctxlake-hook pre_tool --runtime hermes
      timeout: 5
      fail_closed: false
  post_tool_call:
    - command: ctxlake-hook tool --runtime hermes
  post_llm_call:
    - command: ctxlake-hook assistant --runtime hermes
  on_session_end:
    - command: ctxlake-hook session_end --runtime hermes
```

`ctxlake install hermes` writes this, merging into an existing `hooks:` block rather than
replacing it.

Each entry accepts `command`, and optionally `matcher` (a regex over the tool name),
`timeout` (seconds), and `fail_closed`.

> **`fail_closed` only does something on `pre_tool_call`.** It is the sole blocking event,
> and Hermes logs a warning if you set the flag anywhere else rather than letting you
> believe it took effect. ctxlake leaves it `false` by default: a collision check is
> advisory, and a hook crash should never be able to wedge your agent.

An unknown event name is warned about with a did-you-mean suggestion and skipped, so a
typo fails visibly rather than silently capturing nothing.

## Event mapping

| Envelope event | Hermes hook | Notes |
|---|---|---|
| `session_start` | `on_session_start` | |
| `prompt` | `pre_llm_call` | Fires once per turn, before the tool loop |
| `tool_call` (pre) | `pre_tool_call` | Once per call — 3 parallel calls fire it 3 times |
| `tool_call` (post) | `post_tool_call` | Carries `result` and `duration_ms` |
| `assistant` | `post_llm_call` | |
| `session_end` | `on_session_end` | Also `on_session_finalize`, `on_session_reset` |
| `compact` | — | **No equivalent.** See the gap below. |

## The payload

Hermes serializes stdin as `{"hook_event_name": <event>, ...}` with these fields:

```json
{
  "hook_event_name": "post_tool_call",
  "tool_name": "terminal",
  "tool_input": { "command": "cargo test" },
  "session_id": "...",
  "cwd": "/home/you/project",
  "extra": { "result": "...", "duration_ms": 1089, "task_id": "...", "...": "..." }
}
```

`hook_event_name`, `tool_name`, `tool_input`, `session_id`, and `cwd` are top-level and
named identically to Claude Code's, which is why one binary serves both. Everything else
— including `result`, `duration_ms`, `task_id`, `model`, and `platform` — is nested under
`extra`.

> **`extra` is the one place the adapters genuinely differ.** Claude Code puts
> `tool_result` at the top level; Hermes puts `result` inside `extra`. An adapter that
> reads only the shared top-level fields silently records every Hermes tool call with no
> result and no duration.

`session_id` falls back to `parent_session_id`, then to an empty string. An empty
`session_id` is refused rather than spooled under a placeholder — an event that cannot be
attributed to a session is worse than a missing event, because it pollutes every
aggregate it lands in.

## Capabilities

| Capability | Supported | How |
|---|---|---|
| Capture every event | yes | the six hooks above |
| Inject the briefing | yes | `pre_llm_call` returning `{"context": "..."}` |
| Block a colliding edit | yes | `pre_tool_call` returning `{"action": "block", "message": ...}`, or `exit 2` |
| Claude Code response format | yes | `{"decision": "modify", "tool_input": {...}}` is normalized internally |
| Fail closed on hook crash | yes | `fail_closed: true`, `pre_tool_call` only |
| Compaction marker | **no** | no hook exists |
| MCP client | yes | |

### Injection lands in the user message, by design

A `pre_llm_call` hook returning `{"context": "..."}` has that text appended to the current
turn's **user message**, never the system prompt. Hermes does this to protect the prompt
cache: the system prompt stays byte-identical across turns so cached tokens are reused.

This suits ctxlake exactly. The briefing is per-turn, volatile context — a peer's lease
acquired thirty seconds ago — and volatile content belongs after the cache breakpoint, not
in front of it. Injecting into the system prompt would invalidate the cache on every turn.

When several plugins inject, their contributions are joined with blank lines in
alphabetical order of directory name.

## The compaction gap

Hermes has no `pre_compact` equivalent, so a Hermes session has no marker for the moment
its context was discarded. Claude Code and Cursor both emit one.

The consequence is narrow but real: for a Hermes session you cannot reconstruct what the
agent could still see at a given point. Everything else — the transcript, tool calls,
outcomes, digests — is unaffected. There is a `session:compress` event on the *gateway*
hook surface, but that fires for messaging-platform sessions rather than the agent's own
tool-calling loop, so it is not a substitute.

ctxlake records the gap rather than papering over it: Hermes sessions simply have no
`compact` events, and nothing infers one.

## Coexistence

Shell hooks are config-owned, not plugin-owned. One practical consequence worth knowing:
a forced plugin reload clears the hook registry and Hermes re-registers config hooks
afterwards to compensate. If you ever see ctxlake stop capturing after reloading an
unrelated plugin, that is the window — and `hermes hooks list` will show whether the
registration came back.

Other hook consumers are unaffected by ctxlake: `install` appends to the `hooks:` block,
each event takes a list, and every registered hook for an event runs.

## Next steps

- [runtimes/claude-code.md](claude-code.md) — the reference implementation
- [coordination.md](../coordination.md) — what the briefing contains
- [security.md](../security.md) — why one redactor matters
