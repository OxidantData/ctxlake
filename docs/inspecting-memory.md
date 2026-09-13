# Inspecting memory — reading what the fleet knows, and pruning it

Memory is only worth trusting if you can look at it. [Memory](memory.md) describes how a
sealed session becomes something the fleet believes; this page is the operator's side of
that: how to read each tier, how to find a belief that has gone wrong, and how to take it
out of circulation.

Everything here reads the **published snapshot** — the same artifact an agent reads. That
is deliberate. These commands cannot show you a different fleet memory than the one in
use, so *"why did an agent see that?"* and *"what does the CLI say?"* can never drift
apart.

## The three ways in

| Tier | What it holds | Read it with |
|---|---|---|
| **0 — session digests** | files touched, commands and exit codes, friction, duration, tokens and cost | `ctxlake sessions` |
| **1 — agent handoff notes** | a note the agent wrote about its own session | nothing yet — the nudge is not wired, so no note has ever been written |
| **2 — claims** | what the fleet believes, and why it believes it | `ctxlake claims` |

Tier 0 and Tier 2 answer different questions, and the useful work happens between them.
A claim tells you *what the fleet believes*. A session tells you *what actually happened*.
Each knows about the other, so you can start from either end.

## Tier 0 — what actually happened

```sh
ctxlake sessions                 # every sealed session, newest first
ctxlake sessions --limit 50
ctxlake sessions <id-prefix>     # one session, in full
```

The list is one line of identity and one line of summary per session:

```text
51 session(s) in fleet myteam

  6706db76  cc-01            github.com/OxidantData/ctxlake   2026-09-13T04:05:24
            edited crates/ctxlake-maint/src/gate.rs 8x · touched gate.rs, extract.rs, snapshot.rs +6 more
```

Any unambiguous prefix opens one session in full — and a prefix that matches exactly one
session opens it too, so you rarely need to type more than eight characters:

```text
session   6706db76-fa58-4072-a8cf-bfbe14babca7
agent     cc-01
repo      github.com/OxidantData/ctxlake
branch    feat/promotion-gate
ended     2026-09-13T04:03:10.503Z
duration  6m 48s
turns     14
outcome   clean
tokens    284913 in / 12044 out  ($1.8420)

friction
  `cargo test -p ctxlake-maint` failed 2 times
  `crates/ctxlake-maint/src/gate.rs` touched 8 times

files touched (9)
  crates/ctxlake-maint/src/gate.rs
  crates/ctxlake-maint/src/extract.rs
  ...

commands (23)
  [FAIL] cargo test -p ctxlake-maint
  [FAIL] cargo test -p ctxlake-maint
  [ ok ] cargo test -p ctxlake-maint

claims resting on this session (4)
  01M2CF25W4XR [convention/promoted] The promotion gate re-checks quarantine ahead of the four gates
  ...
```

Two details are worth knowing:

- **A command with an unknown exit code prints `?`, not `ok`.** Sessions captured before
  transcript enrichment (v0.1.9) carry no exit codes at all, and rendering those as
  successes would invent a result that was never observed.
- **Friction is the reason to open a session** rather than read its summary line. It is
  derived arithmetic — a repeated failure, a file edited over and over — so it cannot be
  wrong, and it is usually the most informative thing in the record.

The last block is the join: **the claims resting on this session**. A session no claim
cites says so plainly — Tier 0 recorded it, Tier 2 concluded nothing from it — which is
itself useful when you are wondering why a piece of work left no trace in the briefing.

## Tier 2 — what the fleet believes

```sh
ctxlake claims --status promoted               # what agents are actually being told
ctxlake claims --status contested              # the human review queue
ctxlake claims --status candidate --explain    # and which gate is blocking what
```

Each claim prints under its id, with its observer, date, independent-session count and
confidence. The id is not decoration — it is what `--retire` takes.

`--explain` names which of the [four gates](memory.md#promotion-four-gates-in-order)
rejected a candidate and why, which is the fastest way to answer *"why isn't this
promoting?"* without reading the gate's source.

> `--explain` is a read-only, best-effort evaluator, not a second implementation of the
> gate. The contradiction and independence checks need data it does not carry, and it
> says so rather than guessing.

## Grooming — finding a belief that has gone wrong

Two commands find the problem, one acts on it:

```sh
ctxlake claims --duplicates                        # pairs that may be one belief
ctxlake sessions <id>                              # the evidence each rests on
ctxlake claims --retire <claim_id> --reason "..."  # take one out of circulation
```

### A worked pass

`--duplicates` ranks promoted claims that may be saying the same thing:

```text
14 possible duplicate pair(s) among 117 promoted claims.

  [73% overlap]
    01M2C81811Q7 [convention] Dark theme is set in browser context via localStorage.setItem('app.theme', 'dark')
    01M2CJ12T2FR [convention] Dark theme is set via localStorage.setItem('app.theme', 'dark')
```

Open the session each one cites. Here the first also records *where* the setting applies;
the second is the same fact with less in it. Retire the weaker one, and name the survivor
in the reason:

```sh
ctxlake claims --retire 01M2CJ12T2FR \
  --reason "duplicate of 01M2C81811Q7, which states the same convention and also records the browser context it applies in"
```

`--retire` takes any unambiguous prefix. An ambiguous one is reported, never resolved by
taking the first match — claim ids are ULIDs, so claims minted in the same millisecond
share a long prefix, and silently retiring the wrong one would leave no trace that it
happened.

### Retiring appends; it does not delete

A `Retired` event goes into `claims/events/` alongside the `Promoted` event that put the
claim there. The claim stops being agent-visible at the next `ctxlake maint` pass — only
`promoted` claims are ever read — but **the record of having believed it survives**, along
with your reason for stopping.

That is why `--reason` is required. A claim that vanished without trace would be
indistinguishable from one that was never made, and six months later *"which of these two
did we keep, and why"* is exactly the question you will be asking.

> **Retiring is not quarantining.** `--retire` acts on one claim you have judged wrong or
> redundant. [`ctxlake quarantine`](memory.md#quarantine-the-kill-switch) acts on an
> *agent* whose claims you no longer trust as a class, and leaves its capture running.

### Why duplicates are reported and not merged

Measured on a real fleet, word overlap does not separate a duplicate from a distinction.
Two claims about TLS sharing **48%** of their words are different facts — one names the
Secret type, the other the mount path. Two about a stylesheet sharing **46%** are one fact
stated twice. The distinctions score at least as high as the duplicates, so no threshold
divides them, and any automatic merge would fuse things that are not the same. Fusing two
real beliefs into one wrong one is worse than leaving a duplicate, so the command ranks
the pairs and leaves the judgement where it can actually be made.

New duplicates are prevented at extraction instead: the extractor is shown what the fleet
already believes and asked to repeat a known claim character-for-character rather than
reword it, so a second observation merges onto the existing claim as corroboration.
`--duplicates` is for the backlog that predates that.

## When there is nothing to see

Every command here reads the local snapshot mirror. If none has synced yet it says so
rather than reporting an empty fleet — run `ctxlake maint --once`, or wait for the sync
daemon's next refresh.

An empty `--status promoted` on a fleet you expect claims from usually means Tier 2 is
off or still in shadow: see [Turning on Tier 2](memory.md#turning-on-tier-2).

## Next steps

- [Memory](memory.md) — the tiers, the claim types, and the four promotion gates
- [Reference](reference.md) — every flag these commands take
- [Architecture](architecture.md) — where digests, claims and snapshots live in the store
