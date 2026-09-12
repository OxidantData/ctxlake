# Layout — bucket layout reference

Everything lives under one fleet root — the `--store` value you gave `ctxlake init`,
e.g. `s3://my-bucket/ctxlake`. This is the full tree, prefix by prefix.

```text
<fleet-root>/
  fleet.json                                     # fleet_id, created_at, schema_version — written once by `ctxlake init`

  live/                                           # control plane — CAS only, see concepts.md
    agents/<agent_id>.json                        # roster heartbeat: runtime, repo, branch, current tool, expires_at
    intents/<agent_id>.json                       # declared "about to touch" — advisory, no contention
    leases/<resource_key>.json                    # advisory lock — status: free | held, owner, expires_at

  sessions/                                       # data plane — single-writer append, bronze, immutable
    runtime=<runtime>/agent=<agent_id>/date=<yyyy-mm-dd>/<session_id>.parquet

  claims/                                         # data plane — single-writer append
    events/<agent_id>/<ulid>.json                 # proposed claims (memory_propose) — one file per proposal
    fleet/<claim_id>.json                         # promoted claims — only the gate (ctxlake maint) writes here

  snapshot/                                       # serving plane — immutable publish + CAS pointer swap
    briefing/<sha256-of-content>.json             # content-addressed, written once, never overwritten
    briefing/current.json                         # tiny pointer object — CAS-updated to name the current blob
    roster/<sha256-of-content>.json
    roster/current.json

  quarantine/                                     # withheld content — see security.md
    <agent_id>/<date>/<ulid>.json                 # {status: quarantined, rules_fired, hash} — never the raw value
```

## Reading the tree

**`<resource_key>`** is `resource_key(repo, resource)` from
[`hash.rs`](../crates/ctxlake-core/src/hash.rs) — a SHA-256 over `repo\0resource`
(the null byte is there specifically so `("ab", "c")` and `("a", "bc")` can never
collide into the same lease). It's deterministic, so two agents locking the same
logical resource always compute the same key without coordinating first, and it's
plain hex, so it's a safe object-key component on every backend.

**`sessions/` is Hive-partitioned** on `runtime`, `agent`, and `date` so any Parquet
engine — DuckDB, Spark, Oxidant — can prune by any of the three without reading a
manifest first. One file per session, written exactly once when the session's spool is
sealed; nothing ever appends to an already-written session file, which is what makes
"single-writer append" true at the *key* level even though each file's *rows* accumulate
over the session's lifetime in the local spool before that one flush.

**`claims/events/` vs `claims/fleet/`** are deliberately two different prefixes, not
one with a status field, because they have different write permissions: any agent can
append to its own `claims/events/<agent_id>/`, but nothing except the promotion gate
inside `ctxlake maint` ever writes under `claims/fleet/` (AGENTS.md invariant 9). Two
prefixes make that boundary checkable by bucket policy, not just by convention — see
[security.md](security.md)'s IAM section.

**`snapshot/*/current.json`** is small on purpose — it holds nothing but a reference
(content hash, maybe a timestamp) to the real blob sitting under the same prefix. That
smallness is why the pointer swap is cheap and fast even though the thing it points at
can be large: the CAS write that matters is a few dozen bytes, not the whole briefing.

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
