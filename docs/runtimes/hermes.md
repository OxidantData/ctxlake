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
> pages on *what Hermes does*, and, now that the Python-plugin adapter is gone,
> `adapters::hermes` (in `crates/ctxlake-hook`) does normalize exactly the payload shape
> described below, through the same binary every other runtime uses.
>
> The **import** half of this page ("History imports in full", below) has a different
> and narrower basis: `~/.hermes/state.db`'s schema as read from *one* live install with
> `PRAGMA table_info`. That is enough to build against and not enough to call frozen,
> which is why the importer refuses to run at all when a column it maps has moved. See
> [../import.md](../import.md).
>
> What is **not** yet true: the `command:` lines in the config block below are the
> target invocation, not what `ctxlake install hermes` writes (that installer command
> is a later wave). `ctxlake-hook`'s own contract (`main.rs`) takes the event name and
> runtime id as two *positional* arguments — `ctxlake-hook <event> <runtime>` — with no
> flag parsing at all, so a `--runtime hermes` flag is silently swallowed as a
> non-existent third positional argument and the hook fails closed (harmlessly — it
> still exits 0 — but it captures nothing). The event names must also be the ones
> `adapters::hermes::normalize` actually matches (`on_session_start`, `pre_llm_call`,
> `pre_tool_call`, `post_tool_call`, `post_llm_call`, `on_session_end`) — the shorter
> names below are the *envelope's* event vocabulary, not argv[1]'s.

<!-- TODO(ctxlake install hermes): once the installer exists, replace the config block
     below with its actual output and drop this callout — verify by running the
     installed hook end-to-end against a live Hermes session first. -->

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
    - command: env CTXLAKE_FLEET_ID=myteam CTXLAKE_AGENT_ID=herm-01 ctxlake-hook on_session_start hermes
  pre_llm_call:
    - command: env CTXLAKE_FLEET_ID=myteam CTXLAKE_AGENT_ID=herm-01 ctxlake-hook pre_llm_call hermes
  pre_tool_call:
    - command: env CTXLAKE_FLEET_ID=myteam CTXLAKE_AGENT_ID=herm-01 ctxlake-hook pre_tool_call hermes
      timeout: 5
      fail_closed: false
  post_tool_call:
    - command: env CTXLAKE_FLEET_ID=myteam CTXLAKE_AGENT_ID=herm-01 ctxlake-hook post_tool_call hermes
  post_llm_call:
    - command: env CTXLAKE_FLEET_ID=myteam CTXLAKE_AGENT_ID=herm-01 ctxlake-hook post_llm_call hermes
  on_session_end:
    - command: env CTXLAKE_FLEET_ID=myteam CTXLAKE_AGENT_ID=herm-01 ctxlake-hook on_session_end hermes
```

Three details in that command line, none of them decorative:

- **The event and runtime are positional**, `ctxlake-hook <event> <runtime>`. There is no
  flag parsing in the binary at all.
- **The event is Hermes's own name**, passed through verbatim. Each runtime's adapter
  dispatches on that runtime's vocabulary (`PostToolUse`, `postToolUse`,
  `post_tool_call`); the envelope is where they converge, not the argv.
- **The env prefix is required.** No hook schema of the three has an `env` key, so a
  shell-style `VAR=value` prefix is the only way to reach the hook's environment lookup.
  Without it, events capture fine but land attributed to `unconfigured-fleet` /
  `unconfigured-agent`.

Each `command:` is `ctxlake-hook <hermes event name> hermes` — positional, matching
`main.rs`'s `argv[1]` (event) / `argv[2]` (runtime id) contract exactly, not the
envelope's own shorter event names (`session_start`, `tool_call`, …) from the mapping
table above. `ctxlake install hermes` (a later wave) will write this, merging into an
existing `hooks:` block rather than replacing it.

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
| `compact` | — | **No hook equivalent.** Import recovers it from `state.db`; see the gap below. |

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

The table below is about **Hermes's own hook mechanism** — what the shell-hook surface
is capable of, verified against Hermes's source the same way the rest of this page is.
It is not a status report on `adapters::hermes`: this wave's adapter only *captures*.
`hermes::response_for` returns `{}` for every event — briefing injection and blocking
both need the daemon's view of the lake (a lease, a synthesized digest), which does not
reach the hook in this wave, the same gap `claude_code.rs` and `cursor.rs` both document
for their own runtimes. Wiring an actual `{"context": "..."}` or `{"action": "block"}`
response is later work, not something this page can claim is live today.

| Capability | Hermes supports it | How |
|---|---|---|
| Capture every event | yes | the six hooks above — this is what `adapters::hermes` uses today |
| Inject the briefing | yes, mechanically | `pre_llm_call` returning `{"context": "..."}` — **not yet sent**; see above |
| Block a colliding edit | yes, mechanically | `pre_tool_call` returning `{"action": "block", "message": ...}`, or `exit 2` — **not yet sent**; see above |
| Claude Code response format | yes | `{"decision": "modify", "tool_input": {...}}` is normalized internally |
| Fail closed on hook crash | yes | `fail_closed: true`, `pre_tool_call` only |
| Compaction marker | **no** | no hook exists — `ctxlake import` recovers it from `state.db` instead |
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

## The compaction gap — in *live capture*, and only there

Hermes has no `pre_compact` equivalent, so a **live-captured** Hermes session has no
marker for the moment its context was discarded. Claude Code and Cursor both emit one.
`adapters::hermes` never synthesizes one: there is no event to synthesize it from, and
nothing infers it.

The consequence is narrow but real: from live capture alone you cannot reconstruct what
the agent could still see at a given point. Tool calls, outcomes, and digests are
unaffected. There is a `session:compress` event on the *gateway* hook surface, but that
fires for messaging-platform sessions rather than the agent's own tool-calling loop, so
it is not a substitute.

**Import closes this gap, which is the one place a backfill beats live capture.**
`~/.hermes/state.db` marks compacted messages with `compacted = 1` after the fact, so
`ctxlake import --runtime hermes` emits a `compact` event where the data shows one — one
marker per contiguous run of compacted rows, at the run's last message. A hook can only
see what it is called for; a database can be read afterwards. See
[../import.md](../import.md).

The transcript itself has a separate, narrower gap worth stating plainly rather than
folding into "unaffected": `pre_llm_call`/`post_llm_call` envelopes are captured with
`role` set but **no `content`** — `adapters::hermes`'s module doc explains why (no field
name for the prompt/assistant text has been verified against a live payload capture, and
this adapter would rather leave a field empty than spool a guessed key). So today a
Hermes session's bronze record has one `Prompt` and one `Assistant` envelope per turn,
correctly ordered and timestamped, but without the actual text — fix this the same way
`cursor.rs`'s fields were corrected, by diffing against a live capture. Import does not
have this gap either: `messages.content` is right there in the database.

## History imports in full

Hermes keeps a complete SQLite state database at `~/.hermes/state.db` — sessions,
every user/assistant/tool message, the structured `tool_calls` an assistant asked for,
per-session token and cost accounting, and the titles Hermes generates for its own UI.

That makes Hermes a **high-fidelity import source, comparable to Claude Code**, not the
"live capture only, nothing to backfill" runtime an earlier version of
[../import.md](../import.md) claimed it was. `ctxlake import --runtime hermes` backfills
sessions that predate ctxlake entirely.

```sh
ctxlake import --runtime hermes --since 90d
```

Three properties worth knowing before you run it against a machine with a live agent on
it, each covered in full by [../import.md](../import.md):

- **The database is opened read-only and lock-free** — `file:<path>?mode=ro&immutable=1`
  with `SQLITE_OPEN_READ_ONLY`. ctxlake never contends with your running agent for its
  own state.
- **Every imported event goes through the same redactor as live capture.** A historical
  transcript is the likeliest place an un-redacted `cat .env` already sits.
- **A changed schema fails loudly.** Hermes carries a `schema_version` table because its
  shape can move; the importer requires each column it maps by name and refuses with the
  missing ones listed, rather than importing nothing and calling it success.

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
