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

One flag per agent, reversible, auditable, one line: its claims stop promoting, its
already-promoted claims move to `contested`, and **its capture continues** — you want the
record of the failure, not a gap where it used to be.

The signal to reach for it: a rising contradiction rate from one agent. That usually means
its extraction has drifted, and it is the early warning that lets you act before the pool
is polluted rather than after — `crate::calibrate::contradiction_rates` computes exactly
this ratio (contested / (promoted + contested)) per agent from the current claim log, so
an operator can watch it rise before reaching for the flag rather than discovering the
pool is polluted afterwards.

**The real kill switch lives in the gate, not in a CLI flag — and as of this wave the two
are not yet the same thing.** `crate::calibrate::quarantine`/`unquarantine` append
`AgentQuarantined`/`AgentUnquarantined` events to the same claim event log everything else
here lives in; `crate::gate::run_gate` checks that log *before* any of the four gates (a
fifth, separate lever, not one of them — a quarantined agent's candidate never even
reaches the evidence check), and `crate::gate::run` demotes that agent's already-`Promoted`
claims to `contested` on *every* run, not only the run right after quarantine was flagged —
a standing invariant re-checked every time, not a one-time reaction. Un-quarantining is
real and reversible but is not a rollback: a claim already demoted to `contested` stays
there for a human to review, exactly like any other contradiction. Lifting quarantine only
restores the *ability* to promote again.

> **Known gap: `ctxlake quarantine <agent_id>`, the CLI command, does not call any of
> this yet.** It writes a marker under `claims/quarantine/` and edits the *local* fleet
> cache mirror directly — bookkeeping an operator can read back, but `gate::run_gate` and
> `gate::run` never consult either of those, only the `AgentQuarantined` event log above.
> Today there is no command-line entry point to the kill switch that actually stops
> promotion; running the documented command demotes the local cache view of an agent's
> claims without touching what the gate will do on its next run. Wiring the CLI command to
> append the real event (or retiring the marker-file path entirely) is the natural next
> step, and until it lands, treat the two as separate mechanisms rather than assuming the
> command name implies the effect described above.

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

`crate::calibrate::resolve` is that join: among the `outcome` claims on a hypothesis's own
`subject`, observed no earlier than the hypothesis itself, **already `Promoted` by the
gate**, and **observed by a different agent than the hypothesis**, the earliest is the
answer. The last two of those are enforced by `resolve` itself, not left to a caller to
remember: without them, `memory_propose("outcome", ...)` lets any agent manufacture its
own track record — propose a hypothesis, then propose an agreeing "outcome" under its own
identity, and score itself `Correct` with no gate ever having compared either to reality.
Requiring a different observer closes that off, because an agent's identity is fixed by
its adapter for the life of a session, not a field a tool call can set; requiring
`Promoted` means the "outcome" had to survive real evidence and provenance checks first,
not merely exist. Whether the matched outcome *agrees* with the hypothesis is its own
proxy — see the honest-limitations section below for exactly what it does and does not
catch, and why it is deliberately not the same check the contradiction gate uses despite
the resemblance. A hypothesis whose resolution date has passed with no matching outcome at
all is `Expired`,
which is deliberately **not** the same outcome as `Incorrect`: an unanswered question must
never lower a score the way a wrong answer does, or the calibration loop would punish an
agent for questions nobody ever answered. `crate::calibrate::score_agent` keeps that
distinction all the way through — `expired_count` is tracked for visibility but never
enters the Brier-style score, which is mean squared error over `{0, 1}` (`correct` → 0,
`incorrect` → 1) across resolved predictions only.

That score is deliberately hard to over-trust on a small sample: an agent's true score is
blended with a neutral prior weighted by a fixed pseudo-count (Bayesian shrinkage), so
three resolved predictions — even three-for-three — cannot swing an agent's trust nearly
as far as three hundred can. `AgentScore::low_sample` flags the raw case explicitly on top
of that, so nothing downstream can present "1.00" from three data points as if it meant
what "1.00" from three hundred would mean. Every score is itself an event —
`ClaimEvent::CalibrationScored`, carrying the cumulative total as of that moment — in the
same append-only log everything else in this page lives in, for the identical reason: an
agent's track record must be exactly as auditable and replayable as the claims it is
scoring.

The payoff lands in `crate::gate::derive_confidence`: a newly-promoted claim's confidence
is still a structural function of `independent_count` (unchanged), scaled by the observing
agent's `trust_multiplier` — neutral (`1.0`) for an agent with no resolved track record
yet, above `1.0` for a proven-reliable one, below `1.0` for a proven-unreliable one. There
is no self-reported confidence anywhere on a proposed claim for this to silently fall back
to — the schema simply carries no such field — so the only way an agent's assertions carry
more or less weight is by earning it.

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
- **Calibration** — `crates/ctxlake-maint/src/calibrate.rs`. `resolve` joins due
  `hypothesis` claims to the `outcome` claims that answer them; `score_agent` turns
  resolved predictions into a shrinkage-adjusted, low-sample-flagged `AgentScore`,
  appended to the same event log as `ClaimEvent::CalibrationScored`; `derive_confidence`
  (in `gate.rs`) folds that score into a promoted claim's confidence. `quarantine` /
  `unquarantine` append `AgentQuarantined` / `AgentUnquarantined` events that
  `run_gate`/`run` consult directly — see the "Quarantine" section above for exactly
  what each one does and does not do. `require_maintenance_lease` (factored out of
  `gate::run`) is the one check every writer in both modules shares, so quarantine,
  calibration, and promotion can never interleave a write to the same log.

One thing this wave deliberately does not claim to have solved:

- **Contradiction detection is a proxy, not semantics.** There is no NLI model here to
  tell "confirms" from "contradicts" apart. The gate treats "same subject, high cosine
  similarity or lexical overlap, different claim text" as a conflict worth a human's
  attention. That is a coarse recall-favoring heuristic, not a classifier — it will
  send some genuine corroboration to `contested` for a human to wave through, and that
  false-positive rate is the intentional trade against the alternative (silently
  trusting two similar-but-different claims to agree).
- **`calibrate::resolve`'s agreement check is a *different* proxy from the contradiction
  gate's, on purpose, not the same one reused.** An earlier version of this module did
  reuse `find_contradiction`'s exact threshold check and read a hit as *agreement*
  instead of *conflict* — which inverted the one thing a bag-of-words overlap score
  cannot do: tell "X" from "not X" apart. A negation shares nearly every word with what
  it refutes, so that version scored a hypothesis flatly contradicted by its own outcome
  (`"the flake is a colima scheduling artifact"` vs. `"the flake is **not** a colima
  scheduling artifact"`) as `Correct`. That direction of error is the dangerous one:
  it inflates the very `trust_multiplier` this whole module exists to keep honest, on
  exactly the agents whose predictions are most reliably wrong, with nothing downstream
  able to tell the difference from an earned score. `outcome_agrees` now requires
  identical claim text, or embedding similarity with no lopsided negation between the two
  texts, before it will call anything `Correct`; everything else — including high
  *lexical* overlap alone — resolves `Incorrect`.
- **The residual risk runs in both directions, and the honest one is now the milder
  one.** An outcome phrased very differently from a hypothesis it actually confirms, with
  no embedding on either side, still misreads as `Incorrect` — that costs an agent
  unearned credit, which a later, better-worded outcome can still recover, and it is the
  direction this design deliberately biases toward (see above). The negation guard is
  itself only a word-list check, not real negation parsing: a refutation that avoids
  every listed negation word and cue ("the scheduler theory turned out to be wrong",
  say, with no embedding available to catch the semantic reversal) can still slip through
  as an unwarranted `Correct` if its lexical overlap were ever read as agreement again —
  which is exactly why this function no longer reads lexical overlap as agreement at
  all, only as a signal it explicitly refuses to trust in that direction. Watch this in
  shadow mode like every other proxy on this page, not as a settled classifier.
- **Extraction does not yet ask the model for a resolution date.** `resolves_at` exists on
  `ProposedClaim`/`ClaimState` and `calibrate::resolve` reads it, but `extract.rs`'s
  `RawClaim` — the schema the LLM's structured output is parsed into — has no such field
  today, so every hypothesis extraction currently produces has `resolves_at: None`. That
  degrades gracefully rather than silently: a hypothesis with no resolution date simply
  never `Expired`s on the calendar, it can still resolve early the moment a matching
  `outcome` claim lands (see `calibrate::resolve`'s doc) — but the "the pool doesn't fester
  with stale, permanently-unresolved hypotheses" property this page describes needs a real
  date, and wiring the extraction prompt/schema to produce one is the natural next step.

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
