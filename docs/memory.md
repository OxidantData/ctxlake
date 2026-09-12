# Memory — the claim model

The episodic layer records what happened. The memory layer records what is *believed*,
and those two have completely different risk profiles. A transcript is an immutable fact;
a belief is an inference that can be wrong, can be shared, and can quietly become a
fleet-wide consensus that contains no information.

This page is the claim model. For the pipeline that produces claims, see
[summarization.md](summarization.md).

> **Almost all the value is episodic and almost all the danger is semantic.** Session
> history, briefings, and handoffs cannot mislead a fleet — they are derived mechanically.
> Everything on this page can. That asymmetry is why the belief layer is off by default
> and why `mode = "shadow"` exists.

## What a claim is

An atomic proposition with evidence attached.

| Field | Meaning |
|---|---|
| `claim` | One proposition. Not a paragraph, not two facts joined by "and". |
| `claim_type` | `environment` / `convention` / `outcome` / `preference` / `hypothesis` |
| `subject` | The entities it is about — a repo, a service, a command |
| `scope` | `agent` / `repo` / `fleet` |
| `observed_by` | Which agent formed it, or `human` for imported conventions |
| `evidence` | `(session_id, message_id, excerpt_hash)` citations |
| `evidence_count` | How many sessions support it |
| `independent_count` | How many of those did **not** read a related prior claim |
| `status` | `candidate` / `promoted` / `contested` / `retired` |
| `confidence` | Derived, not self-reported — see calibration below |

> **No evidence, no claim.** An extraction that cannot cite a specific
> `(session_id, message_id)` is dropped, not stored with low confidence. This single rule
> removes most hallucinated memory, because a model that invents a belief usually cannot
> invent a citation that resolves.

Claims are stored as an append-only event log, not as mutable rows — `proposed`,
`promoted`, `contested`, `retired` are all appends, and current state is the fold. See
[layout.md](layout.md) and [concepts.md](concepts.md).

## Claim types drive policy

The type is not a label; it decides how a claim gets promoted and how long it lives.

| Type | Example | Promotion | TTL |
|---|---|---|---|
| `environment` | "staging SSH listens on 2222" | 1 observation | 30d, re-verify |
| `outcome` | "the Glue migration passed CI at abc123" | 1 observation, immutable, timestamped | none |
| `convention` | "this repo uses `just`, not `make`" | 2 **independent** observations | 180d |
| `preference` | "prefers terse output" | human approval | 365d |
| `hypothesis` | "the flake is a colima scheduling artifact" | **never auto-promotes beyond agent scope** | 14d |

That last row is the one that protects a fleet. A hypothesis is an agent's opinion.
Promoting opinions into shared memory is how you turn several independent signals into
one correlated signal without noticing — and the fleet then looks more confident than it
has any right to be.

## Scopes, and which direction they flow

```text
agent scope   →   repo scope   →   fleet scope
 (what I saw)     (what this       (what we all
                   project does)    believe)
```

- **Writes go to agent scope only.** Nothing writes to repo or fleet scope except the
  promotion gate.
- **Reads walk up the chain**, and the scope is labelled in what the agent sees.
- `ctxlake` has no API that writes a promoted claim. Agents call `memory_propose`; there
  is deliberately no `memory_write`.

## Independence: the gate that matters most

Two agents agreeing looks like corroboration. It usually is not.

If agent B's session had agent A's claim injected into it, and B then asserts something
derived from it, that is **one observation with two reporters**. Counting it as two is how
a fleet talks itself into confidence.

So every envelope records which claims were injected into that session
(`injected_context`), and promotion counts only the evidence sessions that did *not*
already read a related claim:

```text
independent_count = sessions supporting the claim
                  − sessions that had a related claim injected into them
```

Thresholds read `independent_count`, never `evidence_count`.

> **Read-lineage cannot be retrofitted.** By the time you want to know whether a
> consensus was independent, the sessions are gone. This is why `injected_context` is
> captured from the first commit, before anything reads it.

**Context fencing** is the other half: injected claims are wrapped in a delimiter, and
extraction strips those spans before it runs. Without it an agent rediscovers what it was
just told and files it as a fresh observation — the same echo, one step earlier.

## Contradiction: both sides contest, newest never wins

When a candidate conflicts with a promoted claim on the same subject, **both** move to
`contested` and a human resolves it.

Newest-wins is tempting and wrong. The newer claim is not better evidence — it is just
later. Silently overwriting is how a fleet gaslights itself: the old belief disappears
with no record that anything disagreed.

```sh
ctxlake claims --status contested
```

## Attribution on read — never flatten

Retrieval is only half of it; rendering is the rest. A peer's belief must never reach a
context window as bare fact:

```text
## Fleet context (peer observations — verify before relying on these)

- [cc-03, 2026-09-09, 2 independent sessions, conf 0.81]
  `cargo test --workspace` needs RUSTFLAGS=-D warnings or the clippy gate fails later.

- [cc-01, 2026-09-10, 1 session, conf 0.55, CONTESTED]
  The reattachable-exec test is flaky under colima.
```

An agent reading *"1 session, contested"* behaves differently from one reading a bare
assertion. That difference is the entire safety margin, and it costs a few tokens.

## Quarantine: the kill switch

```sh
ctxlake quarantine <agent_id>
```

One flag per agent. Its claims stop promoting, its already-promoted claims move to
`contested`, and **its capture continues** — you want the record of the failure, not a
gap where it used to be. Reversible, auditable, one line.

The signal to reach for it: a rising contradiction rate from one agent. That usually means
its extraction has drifted, and it is the early warning that lets you act before the pool
is polluted rather than after.

## Confidence is derived, not claimed

A model asserting "confidence 0.9" is asserting nothing checkable. So confidence comes
from track record instead:

1. An agent emits a prediction; extraction files it as a `hypothesis` with a resolution
   date.
2. Reality arrives as an `outcome` claim — from CI, from a benchmark, from a deploy.
3. A resolver joins hypothesis to outcome and scores it.
4. Future claims from that agent are discounted or trusted accordingly.

This only works because provenance and independence were kept from the start. It is the
payoff that justifies the discipline on the rest of this page.

## Anti-goals

- Do not auto-promote hypotheses beyond agent scope. Ever.
- Do not let newest-wins resolve a contradiction.
- Do not extract from injected context.
- Do not skip `injected_context` capture because it looks like overhead.
- Do not put the lake on the inference path — if it is unreachable, agents run degraded,
  not broken.

## Next steps

- [summarization.md](summarization.md) — how claims get produced, and the three tiers
- [coordination.md](coordination.md) — the live layer, which has none of this risk
- [security.md](security.md) — why anything read from the lake is untrusted input
