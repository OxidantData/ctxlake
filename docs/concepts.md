# Concepts — the three planes and one write pattern per plane

ctxlake has no server process and no database. The bucket *is* the system: every fact
an agent needs — who else is around, what happened, what is believed — is an object at
a known key, and the only coordination primitive available is what the object store
itself gives you: put, get, list, and compare-and-swap. Everything in this codebase is
a consequence of taking that constraint seriously instead of routing around it with a
sidecar service.

That constraint forces a choice most systems get to duck: **exactly one write pattern
per kind of data.** Mixing patterns on one key is where distributed bugs live — two
processes racing to append, or a reader observing a half-written record — so ctxlake
assigns each plane a pattern and never lets a second pattern touch that plane's keys.

## The three planes

| Plane | Prefix | What it holds | Write pattern | Who writes |
|---|---|---|---|---|
| **Control** | `live/` | who is doing what *right now* | compare-and-swap | the owning agent, repeatedly |
| **Data** | `sessions/`, `claims/events/` | what *happened* | single-writer append | the owning agent, once per record |
| **Serving** | `snapshot/` | what is *believed*, precomputed | immutable publish + CAS pointer swap | `ctxlake maint`, periodically |

Three planes, not one, because the three questions — "who's active", "what occurred",
"what's true" — have different freshness needs and different failure costs. Getting
`live/` wrong for five seconds is a stale presence indicator. Getting `sessions/` wrong
is losing history permanently, because bronze is append-only and never rewritten. Those
cannot share a write discipline without one of them paying for the other's guarantees.

### Control — `live/`

Roster entries, declared intents, and leases. Small JSON objects, one per agent or
resource, each with a short useful lifetime. The pattern is **compare-and-swap only**:
read the object (and its version/ETag), then write with `PutMode::Update(version)`. A
write that raced loses with `412 Precondition Failed` and retries against the new
version — nobody's update is silently lost, and nobody blocks.

Every `live/` object always exists once an agent has ever announced itself; what
changes is its *contents* (`status: held` vs `status: free`), never its presence. That
sounds like a small detail — it is actually the reason leases work at all on every
backend ctxlake supports. See [coordination.md](coordination.md) and
[storage.md](storage.md) for why.

### Data — `sessions/` and `claims/events/`

Envelopes ([`envelope.rs`](../crates/ctxlake-core/src/envelope.rs)) and proposed claims,
written once and never mutated. The pattern is **single-writer append**: exactly one
process ever writes to a given key, so there is nothing to race against and no CAS
retry loop is needed here. The "one writer" is enforced by construction, not by
locking — a session's Parquet file is keyed by `agent=<id>/date=<d>/<session_id>`, and
only that agent's own daemon ever produces that key.

This plane is bronze: immutable, append-only, chronological by ULID `event_id`. Nothing
downstream — compaction, digests, the belief layer — ever rewrites a bronze record,
only derives new records from it. That is also why redaction cannot be a later pass
(AGENTS.md invariant 7): once a secret lands here, it is there permanently.

### Serving — `snapshot/`

Precomputed views — the briefing an agent reads at session start, the merged roster —
built from `sessions/` and `claims/` by [`ctxlake maint`](architecture.md). The pattern
is **immutable content-addressed publish, then a CAS pointer swap**: a new blob is
written once under a hash of its own content (so it can never collide with or overwrite
a previous version), and only after that write succeeds does a tiny pointer object get
CAS-updated to name it current.

This is the same shape Delta Lake and Iceberg use for table commits, for the same
reason: it turns "did the reader see a consistent, complete version" into "did the
reader's GET happen before or after one atomic pointer flip" — there is no window where
a reader can observe a half-published snapshot, because the object it would have to
half-read doesn't get a name until it's whole.

## Why not just use one pattern everywhere

Because the three planes have opposite requirements and a shared pattern would have to
be the worst-case compromise of both:

- CAS on `sessions/` would mean every event append contends with every other agent's
  events for the same key — there's no reason for that contention to exist, since each
  agent's history is disjoint from every other agent's by construction.
- Single-writer append on `live/` would mean a roster entry could never be updated
  after creation, which defeats the entire purpose of a presence indicator.
- Reading `snapshot/` through CAS-guarded live objects would mean every briefing fetch
  risks colliding with an in-progress rebuild, instead of always landing on a complete,
  previously-published version.

Picking the pattern per plane, and refusing to let a second pattern touch a plane's
keys, is what makes "no cross-key atomicity, no database" survivable: every individual
operation is single-object-atomic, and that is *enough*, because no operation in this
design was ever specified to need more than one object.

## The envelope: one schema across three runtimes

Claude Code, Cursor, and Hermes each expose a different hook surface with different
event names and payload shapes. Every runtime's adapter normalizes into one type,
[`Envelope`](../crates/ctxlake-core/src/envelope.rs), before anything else in the
system sees it. Compaction, digests, and the belief layer all read only the envelope —
they have no idea which runtime produced a given record, and don't need to.

The versioning rule is strict on purpose: `SCHEMA_VERSION` is bumped when a field is
*added*, never when an existing field is *reinterpreted*. Bronze is immutable, so a
field whose meaning changed underneath it makes historical data unreadable — there is
no "migrate the old rows" option once they're sealed. See the per-runtime mapping
tables in [runtimes/](runtimes/claude-code.md) for how each hook surface lands here.

## Redaction is not a plane, it's a gate in front of all of them

Every one of the three planes above receives content that started as raw tool input,
raw tool output, or a raw prompt. All of it passes through
[`Redactor::scrub`](../crates/ctxlake-core/src/redact.rs) in the hook process, before
the spool, on every code path including `ctxlake import`. There is no plane whose
write pattern excuses it from this — CAS objects, appended envelopes, and published
snapshots are all built from content that already went through the same scrubber. See
[security.md](security.md) for the mechanism and its honest limits.

## Next steps

- [coordination.md](coordination.md) — roster, intents, and leases in the control plane
- [storage.md](storage.md) — why CAS is the only primitive that works on every backend
- [architecture.md](architecture.md) — every process and file, traced end to end
