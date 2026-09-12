# mcp — `ctxlake mcp`

`ctxlake mcp` is a stdio MCP server: JSON-RPC 2.0 over newline-delimited stdin/stdout,
protocol revision `2024-11-05`. Claude Code, Cursor, and Hermes are all MCP clients, so
this is the one integration surface that works identically on every runtime — a
runtime with no native hook for something can still get it through here, and a runtime
*with* hooks gets a second, symmetric way to ask for the same information mid-turn.

> **Status: the server is implemented; `ctxlake mcp` is not wired up yet.** Every
> tool works today against the local filesystem via a real `ctxlake-mcp` binary, but
> `ctxlake-cli` has no `mcp` subcommand yet and no `install` support for the config
> block below. To run this today, point a runtime's MCP config directly at the
> `ctxlake-mcp` binary (`cargo build -p ctxlake-mcp --bin ctxlake-mcp`).
>
> `memory_search`/`memory_timeline` read the real local claim snapshot. `"enabled":
> false` means no `ctxlake maint` run has published a snapshot for this fleet yet (the
> default on a fresh install). A fleet running `[summarize] mode = "shadow"` instead
> reads `"enabled": true` with an always-empty result — the snapshot exists, every
> claim in it is simply marked not-agent-visible. Both are deliberate "reads nothing"
> outcomes — see [memory.md](memory.md).

## Why this process never touches the object store

Same reason as the hook (invariant 1): this process is on a path an agent is
synchronously waiting on.

- **Reads** come from the local cache: `~/.ctxlake/cache/<fleet_id>/*.json`.
- **Writes** go to the local spool: `~/.ctxlake/spool/mcp/<fleet_id>.ndjson`, for
  `ctxlake sync` to apply later.
- `crates/ctxlake-mcp` links only `ctxlake-core`, `serde`, and `serde_json` — no
  `object_store`, no `tokio`. A write-shaped tool's result can only ever say "this
  request is queued," never that it reached the fleet — this process cannot observe
  that round trip completing.

Override the on-disk root with `CTXLAKE_SPOOL_DIR` / `CTXLAKE_CACHE_DIR`. Cache reads
are scoped per fleet (`<cache_root>/<fleet_id>/`), since one host can run agents in
more than one fleet.

One consequence worth being explicit about, because it's easy to expect otherwise:
**`fleet_claim` cannot tell you that you now hold a lease.** Holding a lease is a fact
about the object store's `live/leases/` key (see [coordination.md](coordination.md)),
and this process never reads or writes that key directly. What it *can* honestly say
is "this request is queued for `ctxlake sync` to apply" — so that's exactly what every
write-shaped tool's result says. Leases are advisory even when the full round trip
happens (AGENTS.md invariant 5); a tool that can't even complete the round trip has to
be more careful about its claims, not less.

## Wiring it up

The eventual shape is each runtime's own MCP config pointing at the `ctxlake` binary:

```json
{
  "mcpServers": {
    "ctxlake": { "command": "ctxlake", "args": ["mcp"] }
  }
}
```

with `ctxlake install` merging that block in, the same way it merges hook entries.
Neither `ctxlake mcp` nor that install support exists yet (see the status callout) —
point `"command"` at the `ctxlake-mcp` binary instead, and set `CTXLAKE_FLEET_ID` /
`CTXLAKE_AGENT_ID` in the MCP config's `env` block yourself in the meantime.

## The tools

| Tool | Shape | What it does |
|---|---|---|
| `fleet_status()` | read | Who's active and what they hold, as of the last cache refresh. |
| `fleet_claim(paths[], reason, ttl_secs?)` | write | Queue an advisory-lease request. |
| `fleet_release(paths[]?)` | write | Queue a release; omit `paths` for "everything I hold." |
| `fleet_history(repo?, since?, limit?)` | read | Recent sessions and outcomes from the cache. |
| `fleet_handoff(summary, status, next?)` | write | Leave a note for whoever picks this up next. |
| `memory_search(query, scope?, subject?, claim_type?, k?)` | read | Promoted claims matching `query`, with attribution. |
| `memory_propose(claim, type, subject, evidence[])` | write | File a claim **candidate**. Never promotes. |
| `memory_timeline(subject, since?, limit?)` | read | What this fleet has actually tried, re: `subject`. |

Every read tool degrades honestly when its cache file is missing or unparseable — an
empty result with a `note`, never an error, never a guess. Every write tool returns
once the write is durably queued locally, not once it has reached the fleet.

`fleet_history`, `memory_search`, and `memory_timeline` each cap rows per call —
`limit`/`k` default to 50 (10 for `memory_search`) and clamp to a hard ceiling of 200;
an over-limit result says so with `"truncated": true`. `memory_propose`'s `evidence`
array is capped at 20 citations, rejected outright over that rather than trimmed.

### `memory_propose` never writes a promoted claim

This is invariant 9, enforced structurally: there is no `memory_write` tool, `propose`
hard-codes `scope: "agent"`, and nothing in this crate ever constructs a
`claims/fleet/*` write — that key belongs to the promotion gate inside `ctxlake maint`
alone. `propose` also enforces [memory.md](memory.md)'s "no evidence, no claim" rule
itself, before anything is queued: an empty `evidence` array is refused, and so is any
citation missing a non-empty `session_id` or `message_id`.

### `memory_search`'s attribution shape

A hit renders exactly like [memory.md](memory.md) specifies, never as bare fact:

```text
[cc-03, 2026-09-09, 2 independent sessions, conf 0.81]
  `cargo test --workspace` needs RUSTFLAGS=-D warnings or the clippy gate fails later.
```

A contested claim says so inline (`, CONTESTED`), and a claim resting on a single
session reads as "1 session," never rounded up. Search combines FTS5 lexical matching
with cosine similarity over each claim's embedding — today a deterministic,
dependency-free lexical hash, not a real embedder (see [memory.md](memory.md)'s known
limitations).

## Every field read from the lake is treated as an attack surface

A claim, a roster entry, a handoff note — all written by another agent's session, and
this process trusts the daemon to move those bytes faithfully but not to have
sanitized them. Before any such field reaches a tool result:

1. **Zero-width and bidi control codepoints are stripped** — anything that can make
   text *read* differently than it *renders*, which is exactly what an injected
   instruction hiding inside a claim would want.
2. **Every field is length-bounded**, with a visible `…[truncated]` marker.

This runs recursively over the whole JSON value at render time, on every field this
crate ever hands back, not only once at ingest. It does **not** try to pattern-match
phrases like "ignore previous instructions" — that's a losing game. The actual defense
is architectural: [memory.md](memory.md)'s attribution framing means a peer's claim
never arrives as a bare assertion — it arrives labeled as somebody else's observation,
under a "verify before relying on these" heading. Sanitization only makes sure that
framing can't be visually hidden.

## Every write-shaped argument is scrubbed for secrets before the spool

The read-time cleaning above closes an injection channel; it says nothing about the
other direction of harm (invariant 7): a secret pasted into a `fleet_handoff` summary
or a `memory_propose` claim must never reach the spool file, because bronze is
immutable and nothing after that point can un-leak it. Every free-text argument runs
through the same `Redactor` `ctxlake-hook`'s adapters use, before the record is ever
built — recursively, so an `evidence` array's citations get the same treatment as any
named field.

The MCP spool caps its directory's total size and rotates a fleet's file once it gets
large. Unlike the hook, which must silently drop an over-cap event, this server has a
real return channel: hitting the cap comes back as an ordinary tool-call error the
calling agent can see and act on.

## The briefing's fleet-context block

The `SessionStart` briefing has a third block alongside live agents and recent
sessions: promoted claims, reusing this crate's own snapshot read and attribution
renderer — same sanitizer, same shape, same shadow-mode emptiness — capped small (five
lines by default), since `memory_search` is the tool to reach for anything more.

## Protocol conformance

`initialize`, `tools/list`, and `tools/call` are the whole method surface (plus
`ping`). Every other input path returns a JSON-RPC error rather than a panic or a
dropped connection:

| Input | Response |
|---|---|
| Malformed JSON | `-32700` Parse error, `id: null` |
| A JSON value that isn't an object | `-32600` Invalid Request |
| An object with no `method` | `-32600` Invalid Request (if it has an `id`) or dropped (if not) |
| An unknown method | `-32601` Method not found |
| `tools/call` for an unknown tool, or a missing/wrong-typed argument | `-32602` Invalid params |
| A tool refusing on its own terms (e.g. no evidence) | a normal result with `isError: true` — the session keeps going |
| A notification (`id` absent) | never answered, even for an unknown method |
| A stray `result`/`error` frame with no `method` | never answered — not addressed to this server |

**stdout carries JSON-RPC frames and nothing else.** Every diagnostic goes to stderr —
a stray `println!` would corrupt every frame after it. A subprocess test asserts every
line on the real binary's stdout is exactly one valid JSON-RPC frame.

## Next steps

- [memory.md](memory.md) — the claim model `memory_search`/`memory_propose` are built
  against, and the belief layer they're waiting on
- [coordination.md](coordination.md) — what the roster, intents, and leases actually
  promise
- [architecture.md](architecture.md) — this process's place in the full component map
