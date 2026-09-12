# mcp — `ctxlake mcp`

`ctxlake mcp` is a stdio MCP server: JSON-RPC 2.0 over newline-delimited stdin/stdout,
speaking protocol revision `2024-11-05`. It is the one integration surface that works
identically on every runtime this project supports — Claude Code, Cursor, and Hermes
are all MCP clients, so a runtime with no native hook surface for something can still
get it through this server, and a runtime *with* hooks gets a second, symmetric way to
ask for the same information mid-turn instead of only at fixed lifecycle points.

> **Status: the server is implemented; `ctxlake mcp` is not wired up yet.** The protocol
> layer, the fleet tools, and `memory_propose` all work today, against the local
> filesystem, and this crate ships a real `ctxlake-mcp` binary that runs them on real
> stdin/stdout. What does **not** exist yet is `ctxlake-cli`'s `mcp` subcommand —
> `crates/ctxlake-cli` is still the Wave 1 scaffold, so there is no `ctxlake mcp` to spawn
> and no `ctxlake install` to write the config block below into a runtime's settings. If
> you want to run this server today, point a runtime's MCP config at the `ctxlake-mcp`
> binary this crate builds directly (`cargo build -p ctxlake-mcp --bin ctxlake-mcp`), or
> call [`ctxlake_mcp::run_stdio()`](../crates/ctxlake-mcp/src/lib.rs) from your own thin
> wrapper — both do exactly what `ctxlake mcp` will do once it exists.
>
> Separately, `memory_search` and `memory_timeline` are wired up and tested, but read
> from a local cache file nothing in this codebase writes yet — the daemon that would
> populate it, and the promotion gate that would give it something to promote, are both
> later work (Wave 3). Until then both tools say so plainly (`"enabled": false`) rather
> than returning a fabricated result. See [memory.md](memory.md) for the belief layer
> they're waiting on.

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
`CTXLAKE_CACHE_DIR`), resolved through `ctxlake_core::paths` — the single definition every
crate shares. Cache reads are scoped per fleet (`<cache_root>/<fleet_id>/`), because one
host can legitimately run agents in more than one fleet and an unscoped roster would have
them overwrite each other's view of who is active.

## Wiring it up

The eventual shape — once `ctxlake-cli` grows an `mcp` subcommand and an `install`
command to go with it (see the status callout above) — is each runtime's own MCP config
pointing at the `ctxlake` binary:

```json
{
  "mcpServers": {
    "ctxlake": { "command": "ctxlake", "args": ["mcp"] }
  }
}
```

with `ctxlake install` merging that block into a runtime's config (see
[getting-started.md](getting-started.md)) the same way it merges the hook entries.
Neither `ctxlake mcp` nor `ctxlake install` exists today, so that block is not yet
something you can paste in and expect to work — point `"command"` at the `ctxlake-mcp`
binary this crate builds instead (see the status callout for how to build it).

`CTXLAKE_FLEET_ID` and `CTXLAKE_AGENT_ID` scope every read and write this process
does — set them in the runtime's MCP config's `env` block until `ctxlake install` can
set them for you.

## The tools

| Tool | Shape | What it does |
|---|---|---|
| `fleet_status()` | read | Who's active and what they hold, as of the last cache refresh. |
| `fleet_claim(paths[], reason, ttl_secs?)` | write | Queue an advisory-lease request. |
| `fleet_release(paths[]?)` | write | Queue a release; omit `paths` for "everything I hold." |
| `fleet_history(repo?, since?, limit?)` | read | Recent sessions and outcomes from the cache. |
| `fleet_handoff(summary, status, next?)` | write | Leave a note for whoever picks this up next. |
| `memory_search(query, scope?, k?)` | read | Promoted claims matching `query`, with attribution. |
| `memory_propose(claim, type, evidence[])` | write | File a claim **candidate**. Never promotes. |
| `memory_timeline(subject, since?, limit?)` | read | What this fleet has actually tried, re: `subject`. |

Every *read* tool degrades honestly when its cache file is missing or unparseable: an
empty result with a `note` explaining why, never an error and never a guess. Every
*write* tool queues to the local spool and returns once the write is durably queued
locally — never once it has reached the fleet, which this process cannot observe.

### Row counts are capped, independently of field length

`sanitize.rs`'s per-field length bound (below) stops one oversized field from crowding
out a model's context window; it does nothing about *row count*. `fleet_history`,
`memory_search`, and `memory_timeline` each cap how many rows a single call can return —
`limit`/`k` default to 50 (10 for `memory_search`, unchanged) and are clamped to a hard
ceiling of 200 no matter what a caller asks for. A result that had more matching rows
than it returned says so with `"truncated": true`, rather than silently dropping the
tail the way the length bound's truncation marker makes visible for a single field.
`memory_propose`'s `evidence` array gets the same treatment on the write side: at most
20 citations per call, rejected outright over that rather than silently trimmed.

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
cache already cleaned it. Concretely, `sanitize::clean_value` recurses over the *whole*
JSON value a cache file handed back — object keys and values, array elements, at any
depth — rather than naming a fixed list of fields to clean. This crate does not control
the schema the future cache-writer (`ctxlake sync`, Wave 3) will actually use, so a fixed
field list would only cover whatever shape today's author guessed at; a field the list
didn't name, or one nested inside a field it did, would pass through untouched. Recursing
over the value has no such gap: whatever shape a cache record takes, every string in it
is cleaned before this crate hands it back.

It deliberately does **not** try to pattern-match phrases like "ignore previous
instructions"; that's a losing game, and it isn't the actual defense here. The actual
defense is architectural: [memory.md](memory.md)'s attribution framing means a peer's
claim never arrives as a bare assertion the reading model might follow — it arrives
labeled as somebody else's observation, with a session count and confidence attached,
under a "verify before relying on these" heading. Sanitization's job is only to make sure
that framing can't be visually hidden or defeated by an invisible character.

## Every write-shaped argument is scrubbed for secrets before the spool

The read-time cleaning above closes an injection channel; it says nothing about a
different direction of harm, AGENTS.md invariant 7: a secret an agent pastes into a
`fleet_claim` reason, a `fleet_handoff` summary, or a `memory_propose` claim or evidence
citation must never reach `spool.rs`'s ndjson file, because `ctxlake sync` ships that
file's contents into bronze, and bronze is immutable — nothing after this point can
un-leak it. `crates/ctxlake-mcp/src/write_guard.rs` runs every free-text argument to
`fleet_claim`, `fleet_release`, `fleet_handoff`, and `memory_propose` through
`ctxlake_core::redact::Redactor` — the same scrubber `ctxlake-hook`'s adapters use for the
identical reason — before the record is ever built, so a literal secret marker (`sk-`,
`AKIA`, a PEM header, ...) is withheld rather than written verbatim. `evidence` is
caller-shaped JSON, not a fixed set of named fields, so it gets the same recursive
treatment as the read path: every string at any depth is scrubbed, not only the fields
this crate happens to know about.

Nothing drains `spool/mcp/*.ndjson` yet either (that daemon-side consumer is later work,
same as `ctxlake-hook`'s own spool), and every agent in a fleet shares one file per fleet.
`spool.rs` caps that directory's total size and rotates a fleet's file once it gets large,
mirroring `ctxlake-hook`'s own spool guard — but where the hook must silently drop an
over-cap event (it can never fail the host agent's turn), this server has a real return
channel: hitting the cap comes back as an ordinary tool-call error the calling agent can
see and act on.

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
corrupt every frame after it in a way that is hard to diagnose from the client's side.
Two tests check this from two different vantage points, because they catch different
bugs: `lib.rs`'s in-memory test drives a full mixed session (well-formed calls, garbage,
notifications, bad arguments) through `serve()` against a buffer it controls, which
proves the *response-building* logic never emits more or less than one frame per
request — but it cannot see a stray write to the process's *real* stdout, since nothing
in that test path touches it. `tests/stdio_subprocess.rs` closes that gap by spawning the
actual `ctxlake-mcp` binary as a child process and asserting every line on its real
stdout is exactly one valid JSON-RPC frame; a `println!`/`eprintln!`-to-stdout mistake
anywhere on the dispatch path fails this test and only this test.

## Next steps

- [memory.md](memory.md) — the claim model `memory_search`/`memory_propose` are built
  against, and the belief layer they're waiting on
- [coordination.md](coordination.md) — what an advisory lease actually promises, and
  what it does not
- [architecture.md](architecture.md) — this process's place in the full component map
