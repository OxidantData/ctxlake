# Concepts — the three planes and one write pattern per plane

ctxlake has no server process and no database. The bucket *is* the system: every fact
an agent needs — who else is around, what happened, what is believed — is an object at
a known key, and the only coordination primitive available is what the object store
itself gives you: put, get, list, and compare-and-swap.

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

Getting `live/` wrong for five seconds is a stale presence indicator. Getting
`sessions/` wrong is losing history permanently, because bronze is append-only and
never rewritten. Those cannot share a write discipline without one paying for the
other's guarantees.

### Control — `live/`

Roster entries, declared intents, and leases (see [coordination.md](coordination.md)):
small JSON objects, one per agent or resource, each with a short useful lifetime. The
pattern is **compare-and-swap only**: read the object and its version, then write with
`PutMode::Update(version)`. A write that raced loses with `412 Precondition Failed` and
retries against the new version — nobody's update is silently lost, and nobody blocks.
A lease object always exists once it's been touched once; what changes is its
*contents* (`status: held` vs `status: free`), never its presence — the reason leases
work at all on every backend ctxlake supports (see [storage.md](storage.md)).

### Data — `sessions/` and `claims/events/`

Envelopes ([`envelope.rs`](../crates/ctxlake-core/src/envelope.rs)) and proposed claims,
written once and never mutated. The pattern is **single-writer append**: exactly one
process ever writes to a given key, enforced by construction — a session's Parquet file
is keyed by `agent=<id>/date=<d>/<session_id>`, and only that agent's own daemon ever
produces that key.

This plane is bronze: immutable, append-only, chronological by ULID `event_id`.
Compaction and the belief layer only ever derive new records from it, never rewrite it
— which is also why redaction cannot be a later pass (AGENTS.md invariant 7).

### Serving — `snapshot/`

Precomputed views — the briefing, the merged roster — built from `sessions/` and
`claims/` by [`ctxlake maint`](architecture.md). The pattern is **immutable
content-addressed publish, then a CAS pointer swap**: a new blob is written once under
a hash of its own content, so it can never collide with a previous version, and only
after that write succeeds does a tiny pointer object get CAS-updated to name it
current. This is the shape Delta Lake and Iceberg use for table commits: there is no
window where a reader can observe a half-published snapshot, because the object it
would half-read doesn't get a name until it's whole.

## Why not just use one pattern everywhere

- CAS on `sessions/` would mean every event append contends with every other agent's
  events for the same key, for no reason — each agent's history is disjoint by
  construction.
- Single-writer append on `live/` would mean a roster entry could never be updated
  after creation, which defeats a presence indicator entirely.
- Reading `snapshot/` through CAS-guarded objects would mean every briefing fetch risks
  colliding with an in-progress rebuild, instead of always landing on a complete,
  previously-published version.

Picking the pattern per plane is what makes "no cross-key atomicity, no database"
survivable: every individual operation is single-object-atomic, and no operation in
this design was ever specified to need more than one object.

## Maintenance is idempotent by content, on top of its own lease

`ctxlake maint` takes a lease on itself (`live/leases/_maintenance`) before compacting,
extracting, or publishing, so at most one host is "doing maintenance" at a time. That
lease is belt-and-braces, though, not the only thing preventing corruption — every step
underneath it is also idempotent by content:

- Compaction writes into `sessions/compacted/gen=<hash-of-the-sealed-session-set>/` —
  two hosts compacting the same sessions compute the same hash and write the same
  bytes to the same place.
- Extraction claims `claims/extracted/<session_id>` with `PutMode::Create` before
  doing work — exactly one host wins that create, and losing costs nothing but a
  wasted attempt (except on MinIO, where `Create` fails outright rather than only on
  conflict — see [coordination.md](coordination.md)).
- The snapshot publish is the content-addressed pattern above, applied to the belief
  layer.

See [coordination.md](coordination.md) for the one place this still isn't quite free
of the lease: the compaction marker is a plain overwrite, not CAS.

## The envelope: one schema across three runtimes

Claude Code, Cursor, and Hermes each expose a different hook surface with different
event names and payload shapes. Every runtime's adapter normalizes into one type,
[`Envelope`](../crates/ctxlake-core/src/envelope.rs), before anything else in the
system sees it — compaction, digests, and the belief layer all read only the envelope.

`SCHEMA_VERSION` is bumped when a field is *added*, never when an existing field is
*reinterpreted* — bronze is immutable, so a field whose meaning changed underneath it
would make historical data unreadable. See the per-runtime mapping tables in
[runtimes/](runtimes/claude-code.md).

## Redaction is a gate in front of all three planes, not a plane itself

Every plane above receives content that started as raw tool input, raw tool output, or
a raw prompt. All of it passes through
[`Redactor::scrub`](../crates/ctxlake-core/src/redact.rs) in the hook process, before
the spool, on every code path including `ctxlake import`. See
[security.md](security.md) for the mechanism and its honest limits.

## Next steps

- [coordination.md](coordination.md) — roster, intents, and leases in the control plane
- [storage.md](storage.md) — why CAS is the only primitive that works on every backend
- [architecture.md](architecture.md) — every process and file, traced end to end
