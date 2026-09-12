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

## Where this lives, and what's still a placeholder

`ctxlake-maint` (wave 3) implements the pipeline this page describes:

- **The event log** — `crates/ctxlake-maint/src/claims.rs`. `Proposed` / `Promoted` /
  `Contested` / `Retired` / `Superseded` are separate append objects under
  `claims/events/` (see [layout.md](layout.md)); `fold()` replays them into current
  state, and nothing anywhere overwrites one of these events in place. The one exception
  is `claims/fleet/<claim_id>.json`, which the gate *does* overwrite as a claim's status
  changes — safe specifically because `gate::run` *requires* a
  `ctxlake_store::lease::LeaseHandle` for `lease_maintenance` as a parameter (checked
  against its key at runtime), not merely a comment saying the caller ought to hold one,
  so there is never a second writer to race. `publish_fleet_state` and
  `list_fleet_claims` are `pub(crate)`: nothing outside this crate can reach
  `claims/fleet/` directly, only through the gate or the shadow-mode-aware read path
  below.
- **Extraction** — `crates/ctxlake-maint/src/extract.rs`. Context fencing, the
  provider trait over `reqwest` (`anthropic` / `openai-compatible` / `ollama`), and
  "no evidence, no claim" all live here. Citation verification is real: an
  `excerpt_hash`, and each citation's own `observed_at`, are always computed from the
  session's own captured content, never taken from what a model claims. A raw claim
  that matches an existing one on file (same `claim_type`, `subject`, and normalized
  claim text — `find_existing_claim_id`) reuses that claim's `claim_id` instead of
  minting a new one, so corroborating evidence from a later session actually
  accumulates onto the same claim rather than creating an indistinguishable sibling —
  this is what makes independence checking (below) mean anything across real
  extraction runs, not only in a hand-built test fixture.
- **The four gates** — `crates/ctxlake-maint/src/gate.rs`. "No evidence, no claim" is
  enforced as a floor beneath every claim type in `evidence_precheck`, including a
  human-approved `preference` — human approval is an *additional* requirement, not a
  substitute for having any evidence at all. Provenance checks each evidence citation's
  own `observed_at` against its own session's window, never the claim's single
  first-proposed timestamp against every cited session — a claim's evidence can, and
  for `convention`'s two-independent-session bar routinely will, span sessions on
  different days. Independence (`compute_independent_count`) joins evidence sessions
  against `injected_context` and is the authoritative threshold check; the earlier
  evidence check is a cheap pre-filter on raw evidence, not the real gate — see that
  module's doc comment for why the order in this page's list and the order of
  enforcement aren't quite the same thing. Shadow mode is enforced in
  `claims::claims_visible_to_agents` and `claims::read_promoted_for_agents`, and
  because `list_fleet_claims`/`publish_fleet_state` are `pub(crate)`, those two
  functions are the only way anything *outside this crate* can read a fleet-scope
  claim. That guarantee does not extend to `claims::list_events`/`claims::fold`
  themselves, which stay `pub` and shadow-unaware on purpose — the gate needs the full,
  unfiltered state to run at all, in shadow mode included, since shadow mode changes
  who may *read* a promoted claim, not whether one gets promoted. Anything rendering a
  claim into an agent's context window must go through the two agent-facing functions,
  never `fold`/`list_events` directly; see `claims_visible_to_agents`'s doc for why
  that boundary is enforced by convention there, not by the type system.
- **The read side** — `crates/ctxlake-mcp/src/{memory,snapshot}.rs` (wave 4).
  `memory_search`/`memory_timeline` read `<cache_root>/<fleet_id>/snapshot.bin` —
  the local, byte-for-byte mirror of `ctxlake-maint::snapshot::publish`'s SQLite
  artifact `ctxlake sync`'s cache leg already produces — and never the object
  store, never the event log directly (AGENTS.md invariant 1). `memory_search`
  combines FTS5 lexical matching with brute-force cosine over each claim's
  256-dim embedding, both filtered to `visible_to_agents = 1` at the SQL level:
  shadow mode's "reads nothing" guarantee is therefore a property of the query,
  not a mode flag this crate checks and might get wrong — see `snapshot.rs`'s
  module doc. `memory_propose` mirrors `ClaimEvent::Proposed`'s exact wire shape
  (byte-for-byte, without `ctxlake-mcp` depending on `ctxlake-maint` — see
  `wire.rs`'s doc for why that's a deliberate second definition of the same
  format, not an oversight) and enforces "no evidence, no claim" at the level of
  an individual citation, not only "the array is non-empty": a citation missing
  either `session_id` or `message_id` is rejected before it ever reaches the
  spool.
- **The briefing's fleet-context block** —
  `crates/ctxlake-cli/src/briefing.rs` (wave 4). The third block of a session's
  briefing, alongside live agents and recent sessions, reads the exact same
  local snapshot through `ctxlake_mcp::memory::briefing_claims` rather than a
  separate implementation — one attribution renderer, one sanitizer, one
  shadow-mode gate, used everywhere a claim reaches a context window. Empty in
  shadow mode and on a fresh install, for the identical structural reason
  `memory_search` is.

Two things this wave deliberately does not claim to have solved:

- **Contradiction detection is a proxy, not semantics.** There is no NLI model here to
  tell "confirms" from "contradicts" apart. The gate treats "same subject, high cosine
  similarity or lexical overlap, different claim text" as a conflict worth a human's
  attention. That is a coarse recall-favoring heuristic, not a classifier — it will
  send some genuine corroboration to `contested` for a human to wave through, and that
  false-positive rate is the intentional trade against the alternative (silently
  trusting two similar-but-different claims to agree).
- **Confidence is a placeholder formula**, not the resolver described above. It climbs
  with `independent_count` and is bounded, which is all today's gate logic depends on
  it for. The hypothesis-to-outcome resolver that would make confidence mean "this
  agent's track record" is future work.
- **`memory_search`'s vector search runs over a stand-in embedding, because
  nothing anywhere in this codebase computes a real one yet.** Extraction's
  `Provider` trait (`crates/ctxlake-maint/src/extract.rs`) has no `embed()`
  method today, so every claim proposed anywhere — by extraction or by
  `memory_propose` — carries `embedding: None`. Rather than ship no vector
  search until a future wave adds a real embedder, `ctxlake-mcp::snapshot` uses
  a deterministic, dependency-free hashing-trick bag-of-words fingerprint as
  both the query vector and the fallback for any claim with no stored
  embedding — which, today, is every claim, so the comparison is at least
  internally consistent. It is a lexical proxy, not a semantic one, and a real
  embedder landing later will need its own matching query-side embedder before
  a stored real vector and a hash-derived query vector could be compared
  meaningfully — see `snapshot.rs`'s module doc for the honest limitation
  stated once, in code, rather than silently.

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
