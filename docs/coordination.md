# Coordination — presence, intents, and leases

This is the control plane: small JSON objects under `live/`, written with
compare-and-swap, that answer "who is around right now," "what did they say they're
about to touch," and — for the one resource that actually needs mutual exclusion,
`ctxlake maint` itself — "who is doing maintenance right now."

| Object | Key | Question it answers |
|---|---|---|
| Roster entry | `live/agents/<agent_id>.json` | Who is around right now? |
| Intent | `live/intents/<agent_id>.json` | What did an agent *say* it's about to touch? |
| Lease | `live/leases/<resource_key>.json` | Who holds this resource, and until when? |

Roster and intents are announcements — an agent writes its own, and nobody else's
consent is required. A lease is different: it is contended, and CAS is what decides
who wins.

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

## Leases

`ctxlake claim <resource>` acquires a lease at `live/leases/<resource_key>.json`
(`resource_key(repo, resource)` — see [layout.md](layout.md)), and `ctxlake maint`
takes one on itself at the fixed key `live/leases/_maintenance` before running the
maintenance chain (below). Every lease, whatever it's guarding, is the same primitive
and the same state machine:

```mermaid
stateDiagram-v2
    [*] --> Free: lazily created on first touch\n(unconditional PUT, not put-if-absent — storage.md)
    Free --> Held: CAS PutMode::Update(version)\ncontents: {status: held, owner, expires_at}
    Held --> Held: renew before expiry\nCAS from the version last read
    Held --> Free: explicit release\nCAS write back to free
    Held --> Expired: now (store's Date header) > expires_at
    Expired --> Held: any agent may CAS-acquire\n(advisory — no fencing check on the old holder)
```

**AGENTS.md invariant 4: leases are CAS-only, never put-if-absent.** MinIO rejects
`If-None-Match: *` outright ([minio/minio#20346](https://github.com/minio/minio/issues/20346)),
so "does this key exist yet" can never be the acquire primitive on every backend
ctxlake supports. A lease object always exists once it has been touched once; only its
*contents* say free or held, and every transition after that first touch — acquire,
renew, release, steal — is `PutMode::Update(version)` against the version you last
read. See [storage.md](storage.md) for the bootstrap story (the one unconditional
`PUT` this design needs, and why racing it is harmless) and the full backend matrix.

**AGENTS.md invariant 5: leases are advisory, and the docs must say so.** No backend
here can reject a write from a process whose lease already expired — there is no
server-side fencing token to hook into. A process that stalls past its own TTL and
wakes up can still act on stale state, and another agent can still CAS-steal an
expired lease out from under it. For code, git is the real arbiter of what happened;
for anything irreversible, the target system needs its own idempotency key, which
ctxlake cannot supply. Never write a doc sentence implying stronger guarantees than
that — a lease reduces the odds of two agents duplicating twenty minutes of work, it
does not prevent it.

**AGENTS.md invariant 6: expiry reads the store's `Date` header, never a local
clock.** Hosts are distributed and clocks skew; a skewed laptop must not get to decide
someone else's lease is expired.

See [cli.md](cli.md) for `ctxlake claim`/`ctxlake release`'s actual flags and output.

## Why maintenance doesn't lean on its lease for correctness

`ctxlake maint` takes `live/leases/_maintenance` before running (below), which is
enough on its own to keep at most one host "doing maintenance" at a time. But it is
belt-and-braces, not the thing making the chain safe — every step it runs is also
idempotent by content, which is worth spelling out because it is the reason a bug in
the lease (clock skew, a steal, an operator running `ctxlake maint` by hand on two
hosts at once) degrades to redundant work rather than corruption:

- **Compaction** writes into `sessions/compacted/gen=<hash-of-the-sealed-session-set>/`.
  Two hosts compacting the same sessions compute the same hash and write the same
  bytes to the same path; different session sets land in different directories.
  Neither run can corrupt the other's output, and a reader listing a `gen=` directory
  directly never sees a half-written one. What content-addressing does *not* buy on
  its own: `sessions/compacted/.../_COMPACTED`, the pointer that says which `gen=` is
  current, is a plain `store.put` — not CAS — because it's fully recomputed from a
  fresh listing every run rather than accumulated (see
  `ctxlake_store::layout::sessions_compaction_marker`'s doc). Two genuinely concurrent
  runs that saw different sealed-session sets can finish in either order, so the
  *pointer* can momentarily move backward to an older, smaller, but still complete
  generation — never a corrupt or partial one — until the next compaction pass
  recomputes it forward again. That's the one place in this chain where the
  maintenance lease is still pulling real weight today, not just redundant caution.
- **Extraction** claims `claims/extracted/<session_id>` with `PutMode::Create` before
  extracting a session. Exactly one host wins that create; everyone else's identical
  attempt fails and moves on. This is a marker, not a lease — no holder, no TTL, no
  renewal, no stealing, nobody waits on it. **Except on MinIO**, where `Create` fails
  outright rather than failing only on conflict — even on a key that has never
  existed (the same gap [storage.md](storage.md) documents for leases) — so a
  MinIO-backed fleet re-attempts extraction on every run instead of getting the
  one-winner behavior above, until that's worked around. `ctxlake doctor`'s
  put-if-absent probe surfaces this ahead of time.
- **The snapshot** is content-addressed and published by swapping a pointer: the blob
  is written once under a hash of its own content, and only then does a CAS update
  point `latest.json` at it (see [concepts.md](concepts.md)).

None of this makes the lease pointless — it's still the reason an operator sees "one
host is doing maintenance" instead of every host's cron entry spending redundant
compute and PUT traffic every cycle, and it's the only thing standing between two
racing compactions and a stale `_COMPACTED` pointer today. It does mean a bug in the
lease degrades to *cost*, not *correctness*: nothing here can corrupt sealed data or
promote a claim twice.

## Next steps

- [architecture.md](architecture.md) — the maintenance chain and every timing knob
- [cli.md](cli.md) — `ctxlake claim`/`release`/`maint`'s actual commands and output
- [scaling.md](scaling.md) — why naive roster polling is O(N²) and what fan-in fixes
- [storage.md](storage.md) — the CAS matrix leases and roster writes depend on
