# mcp — `ctxlake mcp`

`ctxlake mcp` is a stdio MCP server: JSON-RPC 2.0 over newline-delimited stdin/stdout,
speaking protocol revision `2024-11-05`. It is the one integration surface that works
identically on every runtime this project supports — Claude Code, Cursor, and Hermes
are all MCP clients, so a runtime with no native hook surface for something can still
get it through this server, and a runtime *with* hooks gets a second, symmetric way to
ask for the same information mid-turn instead of only at fixed lifecycle points.

> **Status: implemented, with an honest gap.** The protocol layer, the fleet tools, and
> `memory_propose` all work today, against the local filesystem. `memory_search` and
> `memory_timeline` are wired up and tested, but read from a local cache file nothing in
> this codebase writes yet — the daemon that would populate it, and the promotion gate
> that would give it something to promote, are both later work (Wave 3). Until then both
> tools say so plainly (`"enabled": false`) rather than returning a fabricated result.
> See [memory.md](memory.md) for the belief layer they're waiting on.

## Why this process never touches the object store

`docs/architecture.md`'s component table draws the same box around `ctxlake-mcp` that it
draws around `ctxlake-hook`, for the same reason: this process is on a path an agent is
synchronously waiting on, so AGENTS.md invariant 1 — the store is never on the hook path,
in either direction — applies here too. Concretely:

- **Reads** come from the local **cache**: `~/.ctxlake/cache/<fleet_id>/*.json`, whatever
  `ctxlake sync`'s store→cache leg most recently wrote there.
- **Writes** go to the local **spool**: `~/.ctxlake/spool/mcp/<fleet_id>.ndjson`, one JSON
  line per write-shaped tool call, for `ctxlake sync` to apply later.
- `crates/ctxlake-mcp/Cargo.toml` links only `ctxlake-core`, `serde`, and `serde_json` —
  no `object_store`, no `tokio`. That is not merely a description of current behavior;
  it means the network boundary can't be crossed by accident. Reintroducing either
  dependency would be a visible, explainable diff, not a quiet regression.

One consequence worth being explicit about, because it's easy to expect otherwise:
**`fleet_claim` cannot tell you that you now hold a lease.** Holding a lease is a fact
about the object store's `live/leases/` key (see [coordination.md](coordination.md)),
and this process never reads or writes that key directly. What it *can* honestly say is
"this request is queued for `ctxlake sync` to apply" — so that's exactly what every
write-shaped tool's result says. Leases are advisory even when the full round trip
happens (AGENTS.md invariant 5); a tool that can't even complete the round trip has to be
more careful about its claims, not less.

The on-disk root is `~/.ctxlake/{spool,cache}` (override with `CTXLAKE_SPOOL_DIR` /
`CTXLAKE_CACHE_DIR`), the same spool root `ctxlake-hook` already writes to — not the
fleet_id/agent_id-scoped `~/.local/share/ctxlake/...` path `architecture.md` describes as
the eventual target layout. One daemon will need to drain both the hook's and this
server's output; pointing at two different trees today would just mean rebuilding that
merge later for no benefit now.

## Wiring it up

Each runtime's own MCP config points at the `ctxlake` binary:

```json
{
  "mcpServers": {
    "ctxlake": { "command": "ctxlake", "args": ["mcp"] }
  }
}
```

`CTXLAKE_FLEET_ID` and `CTXLAKE_AGENT_ID` scope every read and write this process does;
`ctxlake install` sets both when it merges this block into a runtime's config (see
[getting-started.md](getting-started.md)).

## The tools

| Tool | Shape | What it does |
|---|---|---|
| `fleet_status()` | read | Who's active and what they hold, as of the last cache refresh. |
| `fleet_claim(paths[], reason, ttl_secs?)` | write | Queue an advisory-lease request. |
| `fleet_release(paths[]?)` | write | Queue a release; omit `paths` for "everything I hold." |
| `fleet_history(repo?, since?)` | read | Recent sessions and outcomes from the cache. |
| `fleet_handoff(summary, status, next?)` | write | Leave a note for whoever picks this up next. |
| `memory_search(query, scope?, k?)` | read | Promoted claims matching `query`, with attribution. |
| `memory_propose(claim, type, evidence[])` | write | File a claim **candidate**. Never promotes. |
| `memory_timeline(subject, since?)` | read | What this fleet has actually tried, re: `subject`. |

Every *read* tool degrades honestly when its cache file is missing or unparseable: an
empty result with a `note` explaining why, never an error and never a guess. Every
*write* tool queues to the local spool and returns once the write is durably queued
locally — never once it has reached the fleet, which this process cannot observe.

### `memory_propose` never writes a promoted claim

This is AGENTS.md invariant 9, enforced structurally, not by convention: there is no
`memory_write` tool in `tools/list`, `propose()` hard-codes `status: "candidate"` in the
record it queues regardless of anything the caller sent, and nothing in
`crates/ctxlake-mcp/` ever constructs a `claims/fleet/*` write — that key is written only
by the promotion gate inside `ctxlake maint`, a different binary this crate does not even
depend on. A conformance test asserts the tool list stays free of `memory_write`, and a
second test asserts a proposed claim's spooled record carries `"candidate"` and never
`"promoted"`.

`propose` also enforces [memory.md](memory.md)'s "no evidence, no claim" rule itself,
before anything is queued: an empty `evidence` array is refused outright.

### `memory_search`'s attribution shape

When the local claims cache exists (today: never — see the status note above), a result
renders exactly like [memory.md](memory.md) specifies, never as bare fact:

```text
[cc-03, 2026-09-09, 2 independent sessions, conf 0.81]
  `cargo test --workspace` needs RUSTFLAGS=-D warnings or the clippy gate fails later.
```

A contested claim says so inline (`, CONTESTED`), and a claim resting on a single session
reads as "1 independent session," not silently rounded up to sound more corroborated than
it is.

## Every field read from the lake is treated as an attack surface

A claim, a roster entry, a handoff note — all of it was written by another agent's
session, mirrored into the local cache by a daemon this process trusts to move bytes
faithfully but not to have sanitized them. Before any such field reaches a tool result:

1. **Zero-width and bidi control codepoints are stripped** — zero-width
   joiners/non-joiners/space, the BOM, the word joiner, and bidi embedding/override/
   isolate controls. All of them can make text *read* differently than it *renders*,
   which is exactly the property an injected instruction hiding inside a claim would
   want.
2. **Every field is length-bounded**, with a visible `…[truncated]` marker rather than a
   silent cut, so an oversized field can't quietly dominate a rendered result.

This happens in `crates/ctxlake-mcp/src/sanitize.rs`, at render time, on every field this
crate ever hands back — not only once, at ingest, on the theory that whatever wrote the
cache already cleaned it. It deliberately does **not** try to pattern-match phrases like
"ignore previous instructions"; that's a losing game, and it isn't the actual defense
here. The actual defense is architectural: [memory.md](memory.md)'s attribution framing
means a peer's claim never arrives as a bare assertion the reading model might follow —
it arrives labeled as somebody else's observation, with a session count and confidence
attached, under a "verify before relying on these" heading. Sanitization's job is only to
make sure that framing can't be visually hidden or defeated by an invisible character.

## Protocol conformance

`initialize`, `tools/list`, and `tools/call` are the whole method surface (plus `ping`,
answered but not required). Every other input path returns a JSON-RPC error rather than
a panic or a dropped connection:

| Input | Response |
|---|---|
| Malformed JSON | `-32700` Parse error, `id: null` |
| A JSON value that isn't an object | `-32600` Invalid Request |
| An object with no `method` | `-32600` Invalid Request (if it has an `id`) or dropped (if not — a notification) |
| An unknown method | `-32601` Method not found |
| `tools/call` for an unknown tool name | `-32602` Invalid params |
| `tools/call` with a missing/wrong-typed required argument | `-32602` Invalid params |
| A tool called correctly but refusing on its own terms (e.g. no evidence) | a normal result with `isError: true` — not a protocol error, so the session keeps going |
| A notification (`id` absent) | never answered, even for an unknown method — the JSON-RPC 2.0 rule |
| A stray `result`/`error` frame with no `method` | never answered — it isn't addressed to this server |

**stdout carries JSON-RPC frames and nothing else.** Every diagnostic in this crate goes
to stderr. A client parses stdout one line at a time as JSON; a stray `println!` would
corrupt every frame after it in a way that is hard to diagnose from the client's side —
this is checked by a test that drives a full mixed session (well-formed calls, garbage,
notifications, bad arguments) through the real serving loop and asserts every line
written is valid, single-line JSON-RPC.

## Next steps

- [memory.md](memory.md) — the claim model `memory_search`/`memory_propose` are built
  against, and the belief layer they're waiting on
- [coordination.md](coordination.md) — what an advisory lease actually promises, and
  what it does not
- [architecture.md](architecture.md) — this process's place in the full component map
