# Coordination — presence, intents, and the roster

This is the control plane: small JSON objects under `live/`, written with
compare-and-swap, that answer "who is around right now, and what did they say they're
about to touch." Nothing here is a lock — there is no resource any agent "holds," and
[why nothing needed one](#why-nothing-needed-a-lock) is below.

| Object | Key | Question it answers |
|---|---|---|
| Roster entry | `live/agents/<agent_id>.json` | Who is around right now? |
| Intent | `live/intents/<agent_id>.json` | What did an agent *say* it's about to touch? |

Both are announcements — an agent writes its own, and nobody else's consent is
required. Writing one never fails and never contends, because nobody else is ever
writing to that exact key.

## Roster

Every agent's daemon (`ctxlake sync`) periodically writes its own
`live/agents/<agent_id>.json`: runtime, repo, branch, current tool if any, and an
`expires_at` computed from the store's own `Date` response header, never a local clock
— a fleet has no shared clock, and a skewed laptop must not get to decide a live peer
looks gone. This is a heartbeat: a plain CAS write against whatever version you last
read, not a contested acquire.

An agent that stops renewing simply falls off the roster once `expires_at` passes —
there is no "leave" message, only silence. `ctxlake status` and the briefing read the
roster from the local cache, refreshed by the daemon on a poll interval (see
[architecture.md](architecture.md)), so a peer that joined moments ago may not show up
yet. That is the staleness floor, not a bug.

Polling every peer's roster key individually is `O(N²)` across a fleet — see
[scaling.md](scaling.md) for why production fleets fan heartbeats into one merged
roster snapshot instead.

## Intents

Before starting non-trivial work, an agent can write `live/intents/<agent_id>.json`
naming the resource it's about to touch — a file glob, a package, a migration name.
This is a declaration, not a reservation: writing it never fails and grants no
exclusivity. Its only job is to let another agent's briefing say "cc-02 declared it's
about to touch `crates/foo/**`" before that agent starts the same work, so the warning
lands at decision time instead of as a merge conflict later.

`collision_policy` in `ctxlake.toml` controls what happens when two agents' declared
intents overlap — `warn` (default) notes it in the briefing and in `ctxlake status`;
`block` additionally lets the hook decline to proceed. Either way this is advisory: it
changes what an agent's own hook chooses to do when it notices an overlap, nothing more
— see [config.md](config.md).

## Why nothing needed a lock

Every batch job ctxlake runs is idempotent by content, so there is nothing to protect
with mutual exclusion:

- **Compaction** writes into `sessions/compacted/gen=<hash-of-the-sealed-session-set>/`.
  Two hosts compacting the same sessions compute the same hash and write the same
  bytes to the same path; different session sets land in different directories.
  Neither can corrupt the other, and a reader following the `_COMPACTED` marker never
  sees a half-written generation.
- **Extraction** claims `claims/extracted/<session_id>` with `PutMode::Create` before
  extracting a session. Exactly one host wins that create; everyone else's identical
  attempt fails and moves on. This is a marker, not a lock — no holder, no TTL, no
  renewal, no stealing, nobody waits on it.
- **The snapshot** is content-addressed and published by swapping a pointer: the blob
  is written once under a hash of its own content, and only then does a CAS update
  point `latest.json` at it (see [concepts.md](concepts.md)).

So concurrent `ctxlake maint` runs across every host in a fleet are fine, on purpose —
there is no "primary" host to designate and nothing to coordinate between them. If two
runs do overlap, the cost is redundant work computing the same answer twice, never
corrupted output.

## Next steps

- [architecture.md](architecture.md) — the maintenance chain and every timing knob
- [scaling.md](scaling.md) — why naive roster polling is O(N²) and what fan-in fixes
- [storage.md](storage.md) — the CAS matrix roster writes depend on
