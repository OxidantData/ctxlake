# Cursor — hooks.json v1, event mapping, import fidelity

Cursor Agent CLI is near-parity with Claude Code: capture, injection, blocking, and MCP
are all supported through the same shape of mechanism. "Near," not "full," because two
things genuinely differ — session-boundary detection and import fidelity — and both
are called out explicitly below rather than smoothed over.

## Config file and mechanism

`ctxlake install` merges hook entries into `hooks.json` — project-scoped `.cursor/hooks.json`
or user-scoped `~/.cursor/hooks.json` — under schema version 1 (`"version": 1`).
Like Claude Code's settings file, each event key holds an array of hook command
entries, and the installer appends to that array rather than replacing it, writing a
`.bak` first (invariant 8). MCP servers are registered separately in `.cursor/mcp.json`
/ `~/.cursor/mcp.json`, merged the same way.

Each hook runs `ctxlake-hook` as a short-lived child process with the event payload as
JSON on stdin — the same transport as Claude Code, and the same reason it satisfies
invariant 1: no persistent connection, no socket opened by the hook itself.

## Event mapping

| Cursor hook | ctxlake `EventType` | Notes |
|---|---|---|
| `beforeSubmitPrompt` | `Prompt` | The human's turn; can inject context ahead of the prompt or block a bad submission. |
| `beforeShellExecution` | *(pre-capture only)* | Path/command pre-check point — used for the same denylist-based blocking Claude Code does at `PreToolUse`. |
| `beforeReadFile` | *(pre-capture only)* | Pre-check for the path denylist ([`Redactor::is_denied_path`](../../crates/ctxlake-core/src/redact.rs)) before a read executes. |
| `beforeMCPExecution` | *(pre-capture only)* | Same pre-check role, scoped to MCP-originated tool calls. |
| `afterFileEdit` | `ToolCall` | The primary capture point for file-editing tool calls: paths touched, and a diff-derived result. |
| `stop` | `Assistant` | The assistant's final message for the turn. |

Two gaps against Claude Code's mapping, both worth naming plainly rather than papering
over with a best-effort mapping:

- **No dedicated `SessionStart` event.** Cursor's hook set has no documented
  equivalent — session boundaries are inferred from the `cursor-agent` process
  lifecycle (the first hook event of a new invocation) rather than an explicit event.
  Briefing injection therefore happens at the first `beforeSubmitPrompt` of a session,
  not at a true session-start point.
- **No dedicated pre-compaction event**, the same gap
  [`envelope.rs`](../../crates/ctxlake-core/src/envelope.rs) documents for Hermes.
  Cursor's own context management is opaque from the hook surface, so `EventType::Compact`
  is never emitted for a Cursor session — there is no moment to catch before that
  history is gone.

## Capability matrix

| Capability | Support | Mechanism |
|---|---|---|
| Capture | Yes | `afterFileEdit` + `stop` + the pre-check hooks above |
| Briefing injection | Yes | `beforeSubmitPrompt` context injection |
| Block | Yes | `beforeShellExecution` / `beforeReadFile` / `beforeMCPExecution` can deny before execution |
| Fail-closed | Supported by the runtime; used narrowly, same posture as Claude Code | Path-denylist blocking only; capture failures fail open |
| Compaction marker | **No** — named gap above | — |
| MCP | Yes | `ctxlake mcp` runs as a stdio child registered via `.cursor/mcp.json` |

## Coexistence with other hook consumers

`hooks.json`'s array-per-event structure exists for the same reason Claude Code's
does: more than one tool can hold an entry on the same event without one clobbering the
other. `ctxlake install` appends; `ctxlake uninstall` removes only the entries it
added, matched by a stable marker, leaving anything else in the file untouched.

## Import fidelity: metadata and prompts only, and why

`ctxlake import --cursor` backfills whatever history is already on disk from past
Cursor sessions — but only **metadata and prompt text**, not full transcripts or tool
call bodies. This is a deliberate refusal, not a missing feature, and the reason is the
storage format itself.

Cursor's local session state lives in a SQLite database structured as an opaque blob
store:

```sql
CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB);
```

Everything that matters — the actual conversation, tool calls, diffs — sits inside the
`data` column in a serialized, internal format, versioned by a `schemaVersion` that
Cursor can and does move without notice across releases. There is no published schema
for what's inside a blob, no stability contract on its layout, and no way to tell from
the database alone whether a given blob's internal format matches the version a
reverse-engineered parser was built against.

**We deliberately do not reverse-engineer it.** A parser built by inspecting today's
blob layout would silently produce garbage — or worse, plausible-looking-but-wrong
data — the moment Cursor ships an update that changes it, with no version check that
could catch the drift before it corrupts an import. Bronze is immutable
([../concepts.md](../concepts.md)): a bad import isn't a bug you patch forward, it's bad data
sitting permanently in `sessions/`. `ctxlake import --cursor` reads only what's safe and
stable to interpret from outside the blob — row-level metadata (timestamps, workspace
and session identifiers) and prompt text where it's recoverable in a readable form —
and leaves everything inside `data` uncaptured rather than guess at it.

The practical effect: backfilled Cursor history in `sessions/` has real prompts and
real timing, but no reconstructed tool-call detail. Live capture going forward, through
the hook mapping above, has no such gap — this limitation is specific to backfilling
history that already existed before `ctxlake install` ran.

## Next steps

- [claude-code.md](claude-code.md) — the reference mapping this one is compared against
- [../import.md](../import.md) — import fidelity across all three runtimes
- [../architecture.md](../architecture.md) — the worked trace this mapping feeds into
