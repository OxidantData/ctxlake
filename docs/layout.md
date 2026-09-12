# Layout — bucket layout reference

Everything lives under one fleet root — the `--store` value you gave `ctxlake init`,
e.g. `s3://my-bucket/ctxlake`. This is the full tree, prefix by prefix.

```text
<fleet-root>/
  fleet.json                                     # fleet_id, created_at, schema_version — written once by `ctxlake init`

  live/                                           # control plane — CAS only, see concepts.md
    agents/<agent_id>.json                        # roster heartbeat: runtime, repo, branch, current tool, expires_at
    intents/<agent_id>.json                       # declared "about to touch" — advisory, no contention

  sessions/                                       # data plane — single-writer append, bronze, immutable
    dt=<yyyy-mm-dd>/fleet=<fleet_id>/runtime=<runtime>/agent=<agent_id>/session=<session_id>/
      seg-000000.parquet, seg-000001.parquet, ...  # appended one per flush; never rewritten once written
      _SEALED                                      # marks the session done — compaction won't touch a dir without this
      digest.json                                  # Tier 0 structural digest, written once by `ctxlake maint digest`
    compacted/dt=<yyyy-mm-dd>/fleet=<fleet_id>/
      part-000000.parquet, ...                     # `ctxlake maint compact`'s output — bronze above is never rewritten
      _COMPACTED                                    # records which sealed sessions this partition's parts reflect

  claims/                                         # data plane — single-writer append
    events/dt=<yyyy-mm-dd>/agent=<agent_id>/<ulid>.json  # proposed claims (memory_propose) — one file per proposal
    fleet/<claim_id>.json                         # promoted claims — only the gate (ctxlake maint) writes here
    extracted/<session_id>                        # idempotency marker — PutMode::Create, at most one host extracts a session

  snapshot/                                       # serving plane — immutable publish + CAS pointer swap
    <sha256-of-content>.sqlite                    # content-addressed, written once, never overwritten — see below
    latest.json                                   # tiny pointer object — CAS-updated to name the current blob

  quarantine/                                     # withheld content — see security.md
    <agent_id>/<date>/<ulid>.json                 # {status: quarantined, rules_fired, hash} — never the raw value
```

## Reading the tree

**`claims/extracted/<session_id>`** is written with `PutMode::Create` before
extraction touches a session — an idempotency marker, not a lock. Exactly one host's
create succeeds; every other host's identical attempt fails and moves on. There is no
holder, no TTL, and nothing to renew or steal, because nothing waits on it.

**`sessions/` is Hive-partitioned** on `dt`, `fleet`, `runtime`, and `agent` so any
Parquet engine — DuckDB, Spark, Oxidant — can prune by any of the four without reading
a manifest first. Each session gets its own directory of `seg-*.parquet` files — one
appended per confirmed flush, zero-padded so lexicographic `LIST` order is also
numeric order — rather than one file, because a session's spool can flush more than
once before it ends; nothing ever rewrites an already-written segment, which is what
makes "single-writer append" true at the *key* level even though a session's *rows*
accumulate over its lifetime. `_SEALED` is the marker that the session is done — see
[`ctxlake-sync`'s upload loop](../crates/ctxlake-sync/src/upload.rs) for what writes it
and [`ctxlake-maint`'s compaction](../crates/ctxlake-maint/src/compact.rs) for why it
must never touch a directory without one (that would race the still-appending writer).

**`sessions/<session>/digest.json`** is the Tier 0 structural digest —
[summarization.md](summarization.md)'s always-on, no-LLM tier — computed purely from
that one session's own sealed segments: files touched, commands and exit codes, token
usage, and friction signals (a command failing repeatedly, a file edited past a
threshold, a session abandoned after a run of failures). See
[`ctxlake-maint/src/digest.rs`](../crates/ctxlake-maint/src/digest.rs).

**`sessions/compacted/`** is where `ctxlake maint compact` writes — never into the
`dt=.../session=.../` directories above. Bronze is immutable, so compaction only ever
*adds* a derived, queryable rewrite of a whole `(date, fleet)` partition into fewer,
larger files, deduped on `(session_id, content_hash)` — never on `content_hash`
alone, which would silently merge different sessions' (and different agents')
events that happen to share content into one misattributed row (with one deliberate
exception: an envelope that never set `content` — every plain tool call — is never
deduped against another one just because both hash the same empty string; see the
module's own doc for why that distinction matters). Each output part file lives
under a `gen=<hash-of-the-sealed-session-set>/` directory, never a fixed filename
reused across runs — see the module's own doc for why a recompaction must never
overwrite a still-live generation's files — and `_COMPACTED` names which generation
is current, so a second run over an unchanged partition is a no-op, not a
duplication, and a reader always follows the marker rather than globbing the
directory directly.

**`claims/events/` vs `claims/fleet/`** are deliberately two different prefixes, not
one with a status field, because they have different write permissions: any agent can
append to its own `claims/events/<agent_id>/`, but nothing except the promotion gate
inside `ctxlake maint` ever writes under `claims/fleet/` (AGENTS.md invariant 9). Two
prefixes make that boundary checkable by bucket policy, not just by convention — see
[security.md](security.md)'s IAM section.

**`snapshot/latest.json`** is small on purpose — it holds nothing but `{content_hash}`,
a reference to the real blob sitting alongside it. That smallness is why the pointer
swap is cheap and fast even though the thing it points at can be large: the CAS write
that matters is a few dozen bytes, not the whole snapshot. Today that blob is a SQLite
file — `ctxlake maint snapshot`'s fold of `claims/events/` into one `claims` table, an
FTS5 index over claim text, and an (as yet unpopulated) 256-dim `embedding` BLOB column
— published write-then-swap: the blob lands first, the pointer only ever points at a
blob that's already there. See
[`ctxlake-maint/src/snapshot.rs`](../crates/ctxlake-maint/src/snapshot.rs) for the exact
schema and [`ctxlake-sync`'s cache module](../crates/ctxlake-sync/src/cache.rs) for how
it's mirrored down to every host, byte-for-byte, without knowing what's inside it.

**`quarantine/`** never holds the secret it's naming. See
[`RedactionOutcome::Quarantined`](../crates/ctxlake-core/src/redact.rs) — the object
holds a hash and the rule that fired, so dedup and auditing work without anyone ever
needing to read the withheld value back out of the lake.

## What is not partitioned by fleet

Everything above lives *inside* one fleet's root. A single bucket can host multiple
fleets side by side as sibling prefixes (`s3://my-bucket/ctxlake-teamA/`,
`s3://my-bucket/ctxlake-teamB/`) — nothing in the layout assumes exclusive ownership of
the bucket, only of its own root.

## Local paths (not in the bucket)

The spool and cache that sit between the hook and the store are local, per-host state,
not part of this tree — they're covered in the worked trace in
[architecture.md](architecture.md), which names the exact files a single tool call
passes through on both sides of the network.

## Next steps

- [storage.md](storage.md) — the CAS/put-if-absent matrix these keys are written with
- [concepts.md](concepts.md) — why each prefix above got the write pattern it did
- [architecture.md](architecture.md) — a worked trace naming every file, local and remote
