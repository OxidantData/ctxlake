# Storage — backends, bucket layout, and what it costs

ctxlake targets five backends through [`object_store`](https://docs.rs/object_store):
AWS S3, Google Cloud Storage, MinIO, Cloudflare R2, and the local filesystem. They are
not interchangeable at the primitive level.

## The two primitives that matter

- **Put-if-absent** — write only if the key does not exist yet.
- **Compare-and-swap (CAS)** — write only if the key is still at the version you last
  read. `live/` and the `snapshot/` pointer both depend on it.

| Backend | Put-if-absent | CAS |
|---|---|---|
| AWS S3 | `If-None-Match: *` | `If-Match: <etag>` |
| GCS | `ifGenerationMatch=0` | `ifGenerationMatch=<generation>` |
| MinIO | **not supported** | `If-Match: <etag>` |
| Cloudflare R2 | `If-None-Match: *` | `If-Match: <etag>` (bucket must be in **ETagMatch** mode) |
| Local filesystem | `O_EXCL` | rename + `flock` |

**MinIO has no put-if-absent.** It rejects `If-None-Match: *`
([minio/minio#20346](https://github.com/minio/minio/issues/20346), closed as "working as
intended"). So every CAS-dependent write in ctxlake — roster heartbeats, the snapshot
pointer — acquires on CAS alone, never on put-if-absent:
`If-Match`/`ifGenerationMatch`/rename+flock is the row every backend has in common.

An object's *first* version uses neither primitive: a `404` on the first `GET` is followed
by a plain `PUT`, then a re-read to obtain a version. An agent's roster key has exactly one
writer ever, and the snapshot pointer is seeded once by whichever `ctxlake maint` run gets
there first.

## `doctor` executes the primitives, it does not assume them

```text
$ ctxlake doctor
backend: s3-compatible (MinIO)
  put-if-absent (If-None-Match: *)  ... UNSUPPORTED (412 on retry, not 200 as expected)
  CAS (If-Match: <etag>)            ... ok
  Date header present               ... ok
  list                              ... ok
verdict: roster heartbeats and live/ writes will work; anything relying on
  put-if-absent will not.
```

This matters most for R2, where CAS support depends on which conditional-write mode the
bucket was created in — a bucket in the wrong mode returns success codes for writes that
did not apply the condition, which is worse than an honest error. `doctor` also prints
which backend it thinks it is talking to, sniffed from the URL scheme and endpoint.

> **What is not verified here.** **R2** takes the same `s3://` code path as MinIO with
> `S3ConditionalPut::ETagMatch` set unconditionally, checked by tests that introspect the
> builder — but no write against a real R2 bucket has been run. **GCS** uses generations,
> not ETags, and its `PutMode::Create` is a *real* put-if-absent (so the "not supported"
> row above is an S3/MinIO-family gap, not a universal one) — verified by reading the
> vendored dependency's source, because no GCS bucket was reachable from this
> environment. Treat both as "implemented and read carefully", not "field-proven", until
> `ctxlake doctor` has run against your own bucket.

**The local filesystem backend** implements CAS directly: write a temp file in the same
directory, `flock` a sibling lock file, verify the target still matches what you read,
then `rename()` over it — atomic for concurrent readers on POSIX. For local dev,
single-host setups and CI, not multi-host fleets.

> **If your bucket cannot do CAS at all, there is no fallback.** Some smaller
> S3-compatible vendors have no conditional-write support whatsoever; the roster and
> snapshot design assumes CAS exists, because there is no way to build "at most one write
> wins" without it. `doctor` telling you before you deploy is the point of running it.

## Bucket layout

Everything lives under one fleet root — the `--store` value you gave `ctxlake init`. A
single bucket can host several fleets as sibling prefixes; nothing assumes exclusive
ownership of the bucket, only of its own root.

```text
<fleet-root>/
  fleet.json                                     # fleet_id, created_at, schema_version — written once by `ctxlake init`

  live/                                          # control plane — CAS only
    agents/<agent_id>.json                       # roster heartbeat: runtime, repo, branch, current tool, expires_at
    intents/<agent_id>.json                      # declared "about to touch" — advisory, no contention

  sessions/                                      # data plane — single-writer append, bronze, immutable
    dt=<date>/fleet=<id>/runtime=<rt>/agent=<id>/session=<id>/
      seg-000000.parquet, ...                    # one per flush; zero-padded so LIST order is numeric order
      _SEALED                                    # session done — compaction never touches a directory without it
      digest.json                                # Tier 0 structural digest, written once by `ctxlake maint`
    compacted/dt=<date>/fleet=<id>/gen=<hash>/
      part-000000.parquet, ...                   # derived rewrite; bronze above is never rewritten
      _COMPACTED                                 # names the current generation

  claims/                                        # data plane — single-writer append
    events/dt=<date>/agent=<id>/<ulid>.json      # proposed claims — one file per proposal
    fleet/<claim_id>.json                        # promoted claims — only the gate writes here
    extracted/<session_id>                       # idempotency marker — PutMode::Create, one host per session

  snapshot/                                      # serving plane — immutable publish + CAS pointer swap
    <sha256-of-content>.sqlite                   # content-addressed, written once, never overwritten
    latest.json                                  # tiny pointer — CAS-updated to name the current blob

  quarantine/                                    # withheld content
    <agent_id>/<date>/<ulid>.json                # {status, rules_fired, hash} — never the raw value
```

- **`sessions/` is Hive-partitioned** on `dt`, `fleet`, `runtime` and `agent`, so any
  Parquet engine — DuckDB, Spark, Oxidant — prunes on any of the four without a manifest.
- **Compaction dedupes on `(session_id, content_hash)`**, never `content_hash` alone,
  which would merge different agents' identical events into one misattributed row. An
  envelope that never set `content` is exempt, so plain tool calls are not all deduped
  against each other on the empty string.
- **`gen=<hash>` is the hash of the sealed session set**, so a second run over an
  unchanged partition is a no-op rather than a duplication, and readers follow
  `_COMPACTED` rather than globbing.
- **`claims/events/` and `claims/fleet/` are two prefixes, not one with a status field**,
  because any agent may append to its own events prefix while only the promotion gate
  writes `claims/fleet/`. Two prefixes make that boundary enforceable by bucket policy.
- **`snapshot/latest.json` holds nothing but `{content_hash}`**, so the CAS write that
  matters is a few dozen bytes however large the blob is. Today the blob is a SQLite file
  — the fold of `claims/events/` into one `claims` table, an FTS5 index, and an as-yet
  unpopulated 256-dim `embedding` column — published blob-first, pointer second.
- **`quarantine/` never holds the secret it names** — only a hash and the rule that
  fired, so dedup and auditing work without the withheld value being readable back.

The spool and cache between the hook and the store are local per-host state, not part of
this tree — see [architecture.md](architecture.md).

## What it costs

Object storage has no idle cost and no server to size. It does have request pricing that
punishes polling. Real S3 request pricing, per 1,000 requests:

| Operation | Price |
|---|---|
| `LIST` | $0.005 |
| `PUT` (including a CAS write) | $0.005 |
| `GET` | $0.0004 |

**A `LIST` or `PUT` costs 12.5x a `GET`.**

### The O(N²) polling problem

Naively, "who else is active" means every agent, every cycle, lists the roster prefix and
fetches every peer individually: `N` `LIST`s and `N(N-1)` `GET`s per cycle. At a 5-second
interval that is 17,280 cycles/day.

| Fleet size | `LIST`s/day | `GET`s/day | Cost/day |
|---|---|---|---|
| 5 agents | 86,400 | 345,600 | $0.43 + $0.14 ≈ **$0.57/day** |
| 20 agents | 345,600 | 6,566,400 | $1.73 + $2.63 ≈ **$4.35/day** |
| 50 agents | 864,000 | 42,336,000 | $4.32 + $16.93 ≈ **$21.25/day** (~$640/mo) |

Discovery traffic only — each agent's own heartbeat `PUT` is `O(N)` either way and is
excluded from both tables. Doubling the fleet roughly quadruples discovery cost.

### Roster fan-in: O(N) instead of O(N²)

Once per cycle, one aggregation step `LIST`s `live/agents/`, `GET`s each of the `N`
heartbeats, and `PUT`s one merged snapshot; every agent then does one `GET` of it instead
of `N-1` peer fetches. That merged-snapshot `PUT` is cost the naive scheme never pays, so
it belongs in the model:

```text
cost/day = (17,280 LISTs + 17,280 PUTs) × $0.005/1000  +  2N × 17,280 × $0.0004/1000
         = $0.1728  +  N × $0.0138
```

| Fleet size | Cost/day |
|---|---|
| 5 agents | $0.1728 + 5 × $0.0138 ≈ **$0.24/day** |
| 20 agents | $0.1728 + 20 × $0.0138 ≈ **$0.45/day** |
| 50 agents | $0.1728 + 50 × $0.0138 ≈ **$0.86/day** (~$26/mo) |

Same fleet, same freshness, **~25x cheaper at 50 agents**, even counting the aggregation
`PUT`. This is why the roster is a fanned-in snapshot in `live/`.

### Other costs worth knowing

| Source | What it costs | Mitigation |
|---|---|---|
| **Latency floor** | S3-class stores: p50 20–100ms, p99 200ms+ | The local cache — the hook never waits on the network |
| **S3 Express One Zone** | ~50% cheaper per request and single-digit-ms, but single-AZ durability and ~4.8x storage | Use for `live/` only. Never `sessions/` or `snapshot/`: losing an AZ there is losing history |
| **CAS contention on a hot key** | A failed CAS write still bills as a full `PUT` | Backoff with jitter avoids a thundering herd; it does not make contention free |
| **Heartbeat write amplification** | A `PUT` is 12.5x a `GET`; renewing too often is the easiest way to overspend | Default renewal is 1/5 of the roster TTL |
| **Small-object explosion** | `LIST` overhead and per-object metadata outweigh the information | `ctxlake maint`'s compaction step |
| **Snapshot fan-out** | `N` GETs of the same blob per interval | A conditional GET against the pointer makes most cycles a cheap `304` |
| **Clock skew** | A fleet has no shared clock | Every expiry check reads the store's own `Date` response header, never a local clock |

### The honest ceiling: about 50 agents

Somewhere around 50 concurrently active agents polling `live/` at these intervals, `O(N)`
request volume and CAS contention on the roster snapshot stop being a few dollars a day
and become an operational concern. **ctxlake does not autoscale past this.** The documented
path is to move `live/` — roster and intents — onto DynamoDB or Redis, while `sessions/`
and `snapshot/` stay unchanged on the lake.

## Next steps

- [how-it-works.md](how-it-works.md) — why each prefix got the write pattern it did
- [architecture.md](architecture.md) — every timing knob and its blast radius
- [security.md](security.md) — the IAM policy that makes "bronze is immutable" enforced
