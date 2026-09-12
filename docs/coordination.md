# Coordination — roster, intents, leases

This is the control plane: three kinds of small JSON objects under `live/`, all
compare-and-swap, all with short useful lifetimes. They answer three different
questions, and it matters which one you're asking before you decide whether a
disagreement between two agents is expected behavior or a bug.

| Object | Key | Question it answers |
|---|---|---|
| Roster entry | `live/agents/<agent_id>.json` | Who is around right now? |
| Intent | `live/intents/<agent_id>.json` | What did an agent *say* it's about to touch? |
| Lease | `live/leases/<resource_key>.json` | Who currently *holds* a resource? |

Roster and intents are announcements — an agent writes its own, and nobody else's
consent is required. A lease is the one place two agents can actually contend, because
acquiring it means writing to a key someone else might also be writing to right now.

## Roster

Every agent's daemon (`ctxlake sync`) periodically writes its own
`live/agents/<agent_id>.json`: runtime, repo, branch, current tool if any, and an
`expires_at` computed from the store's own `Date` response header plus the lease TTL —
never a local clock (AGENTS.md invariant 6). This is a heartbeat, not a lock: nobody
needs permission to update their own entry, so it's a plain CAS write against whatever
version you last read, not a contested acquire.

An agent that stops renewing simply falls off the roster once `expires_at` passes —
there is no "leave" message, only silence. `ctxlake status` and the briefing both read
the roster from the local cache, refreshed by the daemon's store→cache leg on a poll
interval (see [architecture.md](architecture.md) for the read path and its cadence).
That refresh interval is the entire reason peer visibility has a floor of "however
stale your last cache refresh is" — a peer that joined ten seconds ago may not show up
yet, and that is expected, not broken.

Polling every agent's roster key independently is the O(N²) pattern
[scaling.md](scaling.md) walks through in detail — it is why a production fleet fans
heartbeats in through one merged roster snapshot instead of every agent LISTing and
GETting every peer directly.

## Intents

Before starting non-trivial work, an agent can write `live/intents/<agent_id>.json`
naming the resource(s) it's about to touch — a file glob, a package, a migration name.
This is a *declaration*, not a reservation: writing it never fails, never contends, and
grants no exclusivity. Its only job is to let another agent's briefing say "cc-02
declared it's about to touch `crates/foo/**`" before that agent starts its own work on
the same area, so the warning arrives at decision time instead of after the fact in a
merge conflict.

Because it costs nothing to write and nothing to check, intents are the cheap, coarse
layer. Leases are the expensive, precise layer, reserved for resources where an
`stale intent someone forgot to clear` false-positive would be too noisy.

## Leases

A lease is the one object in `live/` where two agents can race to write the *same key*.
Acquire is: read the current object and its version, confirm it says `free`, then write
`{status: held, owner, expires_at}` with `PutMode::Update(<version you read>)`. If
someone else acquired it in between, your write loses with `412 Precondition Failed`
and you back off — you never overwrite a lease you didn't win.

**Leases are never put-if-absent.** `resource_key(repo, resource)` (see
[`hash.rs`](../crates/ctxlake-core/src/hash.rs)) hashes whatever string a human types at
claim time — `crates/oxidant-loom/**` today, something nobody has typed yet tomorrow.
That rules out pre-seeding: `ctxlake init` cannot write a `free` lease object for a
resource that has no identity until someone runs `ctxlake claim` on it, so bootstrap
has to happen lazily, on first touch, not up front at init time.

Bootstrap is a plain, unconditional `PUT`, not a CAS write: a `GET` on
`live/leases/<resource_key>.json` that comes back `404` is followed by writing
`{status: free}` with no condition attached, then a re-read to obtain a version before
attempting the real acquire. It can't lean on put-if-absent either — MinIO rejects the
`If-None-Match: *` primitive a naive "create if it doesn't exist yet" design would
reach for ([minio/minio#20346](https://github.com/minio/minio/issues/20346), closed
"working as intended") — so the unconditional `PUT` is the only move left, and it works
only because an overwrite here is harmless: two agents racing to be first on the same
never-claimed resource may both take this branch and both `PUT`, but both write
identical content (`free`, no owner), so whichever write lands last is
indistinguishable from the other and nothing is lost by losing that race. The write
that actually matters — flipping `free` to `held` — is still the CAS update described
above, from a version just read, so two agents can never both come away believing they
hold the same lease. Bootstrap tolerates a race that acquisition does not. See
[storage.md](storage.md) for the full capability matrix behind that split.

Renewal is the same CAS write repeated before `expires_at`, at a shorter interval than
the TTL (60s renewal against a 5-minute TTL by default — see the knob table in
[architecture.md](architecture.md)). Release is a CAS write back to `free`. All three
operations are the *same* primitive; a lease has exactly one write pattern for its
entire lifecycle, which is what makes its state machine (also in architecture.md)
small enough to reason about at 2am.

### Reading a lease before you rely on it

A lease you're about to *depend on* — not just observe — should be re-read immediately
before use, not trusted from a cache. The local cache exists so the hook path never
blocks on the network (invariant 1), which means the leases your MCP tools see can be
seconds old. That staleness is fine for a warning ("cc-02 currently holds this") and
not fine for anything that treats "I hold the lease" as a safety property — see the
limits below.

## What advisory leases do not promise

This is the most important sentence in this document: **holding a lease does not
prevent anyone from writing.** No backend ctxlake targets — S3, GCS, MinIO, R2, or a
local filesystem — has a way to reject a write from a process whose lease has expired.
There is no fencing token here, because there is nothing on the other end (a file, an
API) that would check one.

Concretely:

- A process that stalls (GC pause, laptop sleep, a network partition) past its lease's
  TTL can wake up and keep writing. Nothing revokes its access; another agent may
  already have acquired the same lease in the meantime, and now two writers exist.
- **For code, git is the arbiter.** A lease that failed to prevent a collision is a
  warning that arrived late or was ignored, not a correctness bug in ctxlake — resolve
  it the way you'd resolve any conflicting edit, with a merge.
- **For an irreversible external action** — calling a paid API, sending a message,
  mutating a resource with no undo — a lease is not sufficient protection on its own. The
  target system needs its own idempotency key. ctxlake can tell two agents not to *both
  try*; it cannot stop a stale one from *succeeding* if it tries anyway.

Never read a passing test or a clean `doctor` run as evidence this doesn't apply to
your case — see [architecture.md](architecture.md)'s "what ctxlake does not guarantee"
section, which exists specifically so this isn't the only place saying so.

## Next steps

- [storage.md](storage.md) — the CAS capability matrix that makes lease acquisition
  portable across backends
- [architecture.md](architecture.md) — the lease state machine diagram and every
  timing knob (TTL, renewal interval, poll interval) with its blast radius
- [scaling.md](scaling.md) — why naive roster polling is O(N²) and what fan-in fixes
