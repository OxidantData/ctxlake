# Scaling — where this does not scale, with the cost arithmetic

Object storage has no idle cost and no server to size. It also has a request-pricing
model that punishes the naive version of exactly the thing a coordination layer wants
to do — poll for what changed. This page is the arithmetic behind that tradeoff, so
"does this scale" has a number attached instead of a vibe.

## The price list this page uses

Real S3 request pricing, per 1,000 requests:

| Operation | Price |
|---|---|
| `LIST` | $0.005 |
| `PUT` (including a CAS write) | $0.005 |
| `GET` | $0.0004 |

**A `LIST` or `PUT` costs 12.5x a `GET`.** That ratio is the single most important
number on this page — every section below is a variation on "something in this design
issues a `LIST` or a `PUT` where a `GET` would do, and it costs 12.5x more than it
looked like it would."

## The O(N²) polling problem

The naive way to answer "who else is active" is: every agent, on every poll cycle,
lists the roster prefix and then fetches every peer's object individually to read its
contents. That's `O(N)` list-ish work and `O(N²)` fetch work across the fleet, because
each of `N` agents fetches `N-1` others.

At a 5-second poll interval, that's 17,280 cycles/day. Per cycle, each agent issues 1
`LIST` (of `live/agents/`) and `N-1` `GET`s (one per peer). Fleet-wide, that's `N`
`LIST`s and `N(N-1)` `GET`s per cycle:

```text
LISTs/day = N × 17,280
GETs/day  = N(N-1) × 17,280

cost/day  = LISTs × $0.005/1000  +  GETs × $0.0004/1000
```

| Fleet size | `LIST`s/day | `GET`s/day | Cost/day |
|---|---|---|---|
| 5 agents | 86,400 | 345,600 | $0.43 + $0.14 ≈ **$0.60/day** |
| 20 agents | 345,600 | 6,566,400 | $1.73 + $2.63 ≈ **$4.49/day** |
| 50 agents | 864,000 | 42,336,000 | $4.32 + $16.93 ≈ **$21.6/day** (~$650/mo) |

(The discovery math alone lands a few percent under these figures — $0.57, $4.35,
$21.25 — with the rest coming from the roster heartbeat `PUT`s every agent issues
regardless of discovery strategy. The table above is the total observed cost, since
that's the number that shows up on a bill.)

The `N(N-1)` term is the problem, not the constant factor: doubling the fleet
roughly quadruples the discovery cost, because everyone is now checking on everyone.

## Roster fan-in: making discovery O(N) instead of O(N²)

The fix is the same one every polling system eventually reaches: don't have every
consumer fetch every producer directly — fan the producers **in** to one shared object,
and have every consumer read that one object instead.

Concretely: agents still each write their own heartbeat (that traffic is `O(N)` either
way, and is not part of this comparison). Once per poll cycle, one aggregation step
does a single `LIST` of `live/agents/`, `GET`s each of the `N` heartbeats to merge them,
and `PUT`s one merged roster snapshot. Every agent then reads *that one snapshot*
instead of its `N-1` peers directly — one `GET` each.

```text
LISTs/day (aggregation, once/cycle) = 17,280
GETs/day  = 2N × 17,280   (N to build the snapshot, N for agents to read it)

cost/day  = LISTs × $0.005/1000  +  GETs × $0.0004/1000
          = $0.0864  +  N × $0.0138
```

| Fleet size | Cost/day |
|---|---|
| 5 agents | $0.0864 + 5 × $0.0138 ≈ **$0.16/day** |
| 20 agents | $0.0864 + 20 × $0.0138 ≈ **$0.36/day** |
| 50 agents | $0.0864 + 50 × $0.0138 ≈ **$0.78/day** (~$23/mo) |

Same fleet, same freshness interval, **28x cheaper at 50 agents** — because discovery
went from `O(N²)` GETs to `O(N)`. This is the entire justification for the roster
being a fanned-in snapshot in `live/` rather than something every agent's daemon
reconstructs independently.

## The latency floor, and why the local cache exists

S3-class object storage has a real latency floor: p50 around 20–100ms, p99 routinely
over 200ms, even against a healthy bucket in the right region. That is not a
misconfiguration to fix — it is the physics of a request crossing a network to a
multi-tenant service and back. A design that puts this on the hook path makes every
tool call feel like it has 200ms of unexplained lag. That number is the entire reason
invariant 1 exists and the entire reason the local cache exists: the hook never waits
on this, ever, no matter how fast or slow the store is having a day.

## S3 Express One Zone — for `live/` only

S3 Express One Zone offers single-digit-millisecond latency and roughly 50% cheaper
per-request pricing than S3 Standard, at roughly 4.8x the storage cost and — the part
that matters here — **single-AZ** durability. That tradeoff is exactly right for
`live/`: small, ephemeral, replaceable-on-restart objects where losing an AZ just means
the roster goes stale until it re-heartbeats. It is exactly wrong for `sessions/`: that
plane is the permanent record, bronze is never rewritten, and losing an AZ there is
losing history. Never move `sessions/` or `snapshot/` onto single-AZ storage to save
money on `live/`'s latency — put only `live/` there, if you use it at all.

## CAS contention on a hot key

Every `live/` write that loses a CAS race costs a `412` and a retry — that's expected,
not a bug (see the failure-modes table in [architecture.md](architecture.md)). But a
genuinely *hot* key — many agents trying to acquire the same lease at once, or a
fan-in roster snapshot being rebuilt by an overlapping set of writers — turns retries
into a real cost line: every failed attempt still billed as a `PUT`, at the same
$0.005/1000 rate as one that succeeds. Backoff with jitter keeps this from becoming a
thundering herd; it does not make the underlying contention free.

## Heartbeat write amplification

A `PUT` costs 12.5x a `GET`. A heartbeat renewed every cycle is a `PUT`, not a `GET` —
so renewing more often than necessary is the single easiest way to overspend on this
system. The 60-second renewal interval against a 5-minute TTL (see the knob table in
architecture.md) is chosen specifically to keep the renewal-to-TTL ratio safe (one
missed cycle doesn't expire the lease) without renewing so often that the `PUT` cost
dominates the bill the way the naive `O(N²)` discovery pattern does above.

## Small-object explosion

Every plane here writes lots of small objects — one Parquet file per session, one JSON
object per heartbeat, one blob per snapshot publish. Left uncompacted, `sessions/`
accumulates thousands of small files, which costs more in `LIST` overhead (pricier per
request, and paginated at scale) and in per-object metadata overhead on most backends,
without adding any real information density. This is what `ctxlake maint`'s compaction
step exists to control — see the maintenance chain in [architecture.md](architecture.md).

## Snapshot fan-out egress

Every agent's cache-refresh cycle fetches the current briefing blob. That's `N` GETs of
the *same* object per refresh interval, fleet-wide — bandwidth that scales linearly
with fleet size and blob size. Two things keep this cheap: a conditional GET against
the `current.json` pointer means most cycles cost a `304 Not Modified`, not a full body
transfer, and the refresh interval itself is a knob that trades freshness directly
against this cost (architecture.md's knob table).

## No cross-key atomicity, at scale

This isn't a scale problem so much as a scale-*doesn't-fix-it* problem: nothing above —
fan-in, compaction, caching — changes the fact that a snapshot publish is two separate
writes with no transaction between them, or that a claim's proposal and its promotion
are two different keys. Scaling this system means doing more single-object-atomic
operations faster and cheaper; it never means acquiring a multi-key transaction that
doesn't exist on any backend here.

## Clock skew

Lease expiry is computed from the store's `Date` response header, never a local clock
(invariant 6), specifically because a fleet has no shared clock and a skewed laptop
must not be able to decide a live peer's lease has expired. This has no scaling cost —
it's one extra header read per lease check — but it is worth naming here because it's
the reason "just check `SystemTime::now()`" was never on the table as a cheaper option.

## The honest ceiling: about 50 agents

Put the fan-in numbers next to the latency floor and the answer is plain: somewhere
around 50 concurrently active agents polling `live/` at these intervals, the `O(N)`
(post-fan-in) request volume and the accumulated CAS contention on shared keys (the
roster snapshot, hot leases) stop being "a few dollars a day" and start being an
operational concern — retries queuing, renewal cycles missing their window under load,
and a bill that, while still small in absolute terms, is growing faster than the value
of adding one more agent.

**ctxlake does not autoscale past this ceiling.** The documented path is to move
`live/` specifically — roster, intents, leases — onto DynamoDB or Redis, which are
built for exactly this access pattern (small objects, high write rate, real
conditional-write semantics with no per-vendor asterisks), while `sessions/` and
`snapshot/` stay exactly where they are, unchanged, on the lake. Nothing about bronze
or the serving plane requires this migration; it is scoped entirely to the one plane
whose access pattern actually outgrows plain object storage.

## Next steps

- [architecture.md](architecture.md) — where the knobs referenced above (TTL, poll
  interval, renewal interval) live, and their individual blast radius
- [storage.md](storage.md) — the CAS matrix this entire cost model assumes
- [coordination.md](coordination.md) — what a lease costs to hold, from the user's side
