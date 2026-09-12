# Scaling — where this does not scale, with the cost arithmetic

Object storage has no idle cost and no server to size. It also has a request-pricing
model that punishes the naive version of exactly what a coordination layer wants to
do — poll for what changed. This page puts a number on "does this scale."

## The price list this page uses

Real S3 request pricing, per 1,000 requests:

| Operation | Price |
|---|---|
| `LIST` | $0.005 |
| `PUT` (including a CAS write) | $0.005 |
| `GET` | $0.0004 |

**A `LIST` or `PUT` costs 12.5x a `GET`.** Every section below is a variation on
"something here issues a `LIST` or `PUT` where a `GET` would do."

## The O(N²) polling problem

Naively, "who else is active" means every agent, every poll cycle, lists the roster
prefix and fetches every peer's object individually. That's `O(N)` list work and
`O(N²)` fetch work fleet-wide, because each of `N` agents fetches `N-1` others.

At a 5-second poll interval (17,280 cycles/day), fleet-wide that's `N` `LIST`s and
`N(N-1)` `GET`s per cycle:

```text
LISTs/day = N × 17,280
GETs/day  = N(N-1) × 17,280
cost/day  = LISTs × $0.005/1000  +  GETs × $0.0004/1000
```

| Fleet size | `LIST`s/day | `GET`s/day | Cost/day |
|---|---|---|---|
| 5 agents | 86,400 | 345,600 | $0.43 + $0.14 ≈ **$0.57/day** |
| 20 agents | 345,600 | 6,566,400 | $1.73 + $2.63 ≈ **$4.35/day** |
| 50 agents | 864,000 | 42,336,000 | $4.32 + $16.93 ≈ **$21.25/day** (~$640/mo) |

(Discovery traffic only — each agent's own heartbeat `PUT` is `O(N)` either way and
excluded from both this total and the fan-in total below, for the same reason.) The
`N(N-1)` term is the problem: doubling the fleet roughly quadruples discovery cost.

## Roster fan-in: O(N) instead of O(N²)

Don't have every consumer fetch every producer — fan producers **in** to one shared
object, and have every consumer read that instead. Once per poll cycle, one
aggregation step `LIST`s `live/agents/`, `GET`s each of the `N` heartbeats, and `PUT`s
one merged roster snapshot; every agent then does one `GET` of that snapshot instead
of `N-1` peer fetches. That merged-snapshot `PUT` is cost the naive scheme never pays,
so it belongs in this model:

```text
LISTs/day (aggregation, once/cycle)     = 17,280
PUTs/day  (merged snapshot, once/cycle) = 17,280
GETs/day  = 2N × 17,280   (N to build the snapshot, N for agents to read it)

cost/day  = (LISTs + PUTs) × $0.005/1000  +  GETs × $0.0004/1000
          = $0.0864 + $0.0864  +  N × $0.0138
          = $0.1728  +  N × $0.0138
```

| Fleet size | Cost/day |
|---|---|
| 5 agents | $0.1728 + 5 × $0.0138 ≈ **$0.24/day** |
| 20 agents | $0.1728 + 20 × $0.0138 ≈ **$0.45/day** |
| 50 agents | $0.1728 + 50 × $0.0138 ≈ **$0.86/day** (~$26/mo) |

Same fleet, same freshness interval, **~25x cheaper at 50 agents** — discovery goes
from `O(N²)` GETs to `O(N)`, even counting the aggregation `PUT` the naive scheme never
pays. This is why the roster is a fanned-in snapshot in `live/`, not something every
agent's daemon reconstructs independently.

## Other costs worth knowing

| Source | What it costs | Mitigation |
|---|---|---|
| **Latency floor** | S3-class stores: p50 20–100ms, p99 200ms+. Putting this on the hook path makes every tool call feel laggy. | The local cache — the hook never waits on the network (invariant 1). |
| **S3 Express One Zone** | ~50% cheaper per request, single-digit-ms latency, but single-AZ durability and ~4.8x storage cost. | Use it for `live/` only — small, replaceable-on-restart objects. Never for `sessions/` or `snapshot/`: bronze is the permanent record, and losing an AZ there is losing history. |
| **CAS contention on a hot key** | A failed CAS write still bills as a full `PUT`. Many agents' heartbeats landing on the fan-in roster snapshot at once turns retries into a real cost line. | Backoff with jitter avoids a thundering herd; it doesn't make the contention free. |
| **Heartbeat write amplification** | A `PUT` costs 12.5x a `GET`. Renewing a heartbeat more often than necessary is the easiest way to overspend. | The default renewal interval is 1/5 of the roster TTL — one missed cycle doesn't drop a live peer, without renewing so often the `PUT` cost dominates. |
| **Small-object explosion** | Thousands of small session files cost more in `LIST` overhead and per-object metadata than they add in information density. | `ctxlake maint`'s compaction step — see [architecture.md](architecture.md). |
| **Snapshot fan-out egress** | Every agent's cache refresh fetches the current briefing blob — `N` GETs of the same object per interval. | A conditional GET against the pointer means most cycles cost a cheap `304`, not a full transfer. |
| **No cross-key atomicity** | A snapshot publish is two writes with no transaction between them; a claim's proposal and promotion are two separate keys. Scaling never buys a multi-key transaction that doesn't exist on any backend here. | Doing more single-object-atomic operations, faster and cheaper — not a bigger primitive. |
| **Clock skew** | A fleet has no shared clock. | Every expiry check reads the store's own `Date` response header, never a local clock — one extra header read, no scaling cost. |

## The honest ceiling: about 50 agents

Put the fan-in numbers next to the latency floor and the answer is plain: somewhere
around 50 concurrently active agents polling `live/` at these intervals, `O(N)`
(post-fan-in) request volume and CAS contention on the roster snapshot stop being "a
few dollars a day" and start being an operational concern.

**ctxlake does not autoscale past this ceiling.** The documented path is to move
`live/` specifically — roster and intents — onto DynamoDB or Redis, built for exactly
this access pattern (small objects, high write rate, conditional writes with no
per-vendor asterisks), while `sessions/` and `snapshot/` stay unchanged on the lake.

## Next steps

- [architecture.md](architecture.md) — where the knobs referenced above live, and
  their individual blast radius
- [storage.md](storage.md) — the CAS matrix this entire cost model assumes
- [coordination.md](coordination.md) — what a roster heartbeat costs, from the user's side
