# Memory — from a finished session to something the fleet believes

Three tiers turn a sealed session into something the next agent can use. **The default
needs no LLM API key.** Only the third tier does, and it is optional.

| Tier | What it produces | Needs a model? | On by default? |
|---|---|---|---|
| **0 — structural digest** | files touched, commands and exit codes, tests, commits, duration, cost, friction signals | No | Yes |
| **1 — agent self-handoff** | a note attached to its own session. **Not a belief.** | No — the agent that did the work writes it, cache-warm, on a key you already pay for | Yes |
| **2 — batch extraction** | candidate claims → four gates → promoted → snapshot → briefing | Yes | No — needs `[summarize.batch]` configured |

Tier 0 is pure arithmetic over captured events, so it cannot hallucinate. A line like
*"abandoned after 4 failed `cargo test -p oxidant-connect` runs"* is often the most useful
in a briefing, costs nothing, and is never wrong. `ctxlake maint` writes one `digest.json`
per sealed session, reading `tool.exit_code` regardless of which runtime produced it.

> **Tier 1 produces notes, not beliefs.** An agent summarizing its own session is
> self-reporting. A handoff is *episodic* — "here is what I did and what failed" —
> attached to its session and attributed to it. It never enters the belief layer. At turn
> end a hook returns a one-time nudge and the agent calls `fleet_handoff`; Claude Code
> uses `Stop`, Cursor `stop`, Hermes `on_session_end`. **This nudge is not wired yet**,
> so `tier 1 nudges fired: 0` is expected today.

> **Almost all the value is episodic and almost all the danger is semantic.** History,
> briefings and handoffs are derived mechanically and cannot mislead a fleet. Everything
> below can. That asymmetry is why the belief layer is off by default.

> **`convention` asks for 1 observation today, not 2, and that is a concession.**
> Two independent observations is the right bar — it is what separates a convention the
> codebase has from one agent's opinion. But the count it compares against subtracts
> sessions that were *told* a related claim, via `injected_context`, and **nothing
> populates that field yet**. So the count cannot exceed 1 from a single session and the
> bar was not strict, it was unreachable: on a real fleet all 41 conventions sat as
> permanent candidates, including the most useful claims in it.
>
> An unreachable gate protects nothing; it discards the category. What still stands
> between a convention and a context window: the contradiction gate, the provenance
> gate, the quarantine kill switch, and attribution on read — a promoted convention
> shows its observer, its session, its count of 1, and the standing "verify before
> relying on these" note. The threshold returns to 2 once `injected_context` is
> populated and the count means something.

## Turning on Tier 2

`ctxlake maint --once` compacts, digests, and publishes the snapshot for free, with the
promotion gate always running over it — a fleet with no LLM configured still gates
whatever `memory_propose` wrote directly. Point it at a model to also run batch
extraction, in `ctxlake.toml` ([full reference](reference.md#summarize)):

```toml
[summarize]
mode = "shadow"              # keep this for now

[summarize.batch]
provider    = "anthropic"    # or "openai-compatible", "ollama" (fully local), "openrouter", "gemini"
model       = "claude-haiku-4-5"
api_key_env = "ANTHROPIC_API_KEY"   # the NAME of an env var, never the key
```

```sh
export ANTHROPIC_API_KEY=...
ctxlake doctor                                # reports whether the name resolves. Never its value.
ctxlake maint --once                          # extracts, gates, and publishes in one pass
ctxlake claims --status candidate --explain   # what was extracted, which gate rejected what
ctxlake claims --status contested             # where claims disagree with each other
```

`mode = "shadow"` runs the whole chain with agent reads disabled: claims accumulate,
gate reports accumulate, nothing reaches a context window. Give it a couple of weeks of
real sessions and answer one question — *would I want an agent to act on this?*

> **Sit in shadow mode longer than feels necessary.** It needs your sessions, your repos
> and your judgment; no amount of documentation substitutes. Everything a wrong claim
> costs is paid later and by someone else, in a context window, as a confident assertion
> with no sign it was ever in doubt.

Then `mode = "live"`. Reverting is the same line.

## Extraction costs

Roughly 8K input and 500 output tokens per session. Batch pricing is half of live, and
the schema prefix is byte-identical across sessions so prompt caching bills it once.

| Fleet | Live requests | Batched |
|---|---:|---:|
| 40 sessions/day | ~$0.42/day | **~$0.21/day** |
| 400 sessions/day (50 agents) | ~$4.20/day | **~$2.10/day** |

The largest line item in the system, and the only one that scales with session count
rather than storage — which is why it is the tier you can switch off. Under
`mode = "none"` you lose the belief layer and nothing else.

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
| `confidence` | Derived, never self-reported |

> **No evidence, no claim.** An extraction that cannot cite a specific
> `(session_id, message_id)` is dropped, not stored with low confidence. A model that
> invents a belief usually cannot invent a citation that resolves.

Claims are an append-only event log — `proposed`, `promoted`, `contested` and `retired`
are all appends, and current state is the fold.

### The type decides the policy

| Type | Example | Promotion | TTL |
|---|---|---|---|
| `environment` | "staging SSH listens on 2222" | 1 observation | 30d, re-verify |
| `outcome` | "the Glue migration passed CI at abc123" | 1 observation, immutable, timestamped | none |
| `convention` | "this repo uses `just`, not `make`" | 1 observation *(see below)* | 180d |
| `preference` | "prefers terse output" | human approval | 365d |
| `hypothesis` | "the flake is a colima scheduling artifact" | **never auto-promotes beyond agent scope** | 14d |

A hypothesis is an agent's opinion. Promoting opinions into shared memory turns several
independent signals into one correlated one, and the fleet then looks more confident than
it has any right to be.

## Promotion: four gates, in order

<svg viewBox="0 0 720 212" role="img" aria-label="An extracted candidate claim passes four gates in order — evidence, contradiction, provenance, independence — before it is promoted and reaches a briefing. Failing any gate sends the claim to a contested queue for a human instead of dropping it. In shadow mode the final step to the briefing is cut." style="width:100%;height:auto">
  <defs>
    <marker id="mg" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto">
      <path d="M0 0 L8 4 L0 8 z" fill="var(--oxidant-text-muted)"/>
    </marker>
    <style>
      .b { fill: var(--oxidant-surface); stroke: var(--oxidant-border-strong); stroke-width: 1; rx: 6 }
      .t { fill: var(--oxidant-text); font: 500 12px var(--oxidant-font-ui) }
      .s { fill: var(--oxidant-text-muted); font: 400 10px var(--oxidant-font-ui) }
      .l { stroke: var(--oxidant-text-muted); stroke-width: 1.25; fill: none; marker-end: url(#mg) }
      .dash { stroke-dasharray: 4 3 }
    </style>
  </defs>

  <text x="8" y="34" class="s">TIER 2 PROPOSES</text>
  <text x="132" y="34" class="s">FOUR GATES, IN ORDER</text>
  <text x="608" y="34" class="s">AGENT-VISIBLE</text>

  <rect class="b" x="8" y="48" width="100" height="48"/>
  <text x="20" y="70" class="t">candidate</text>
  <text x="20" y="86" class="s">with citations</text>

  <rect class="b" x="132" y="48" width="102" height="48"/>
  <text x="144" y="70" class="t">evidence</text>
  <text x="144" y="86" class="s">by type</text>

  <rect class="b" x="250" y="48" width="102" height="48"/>
  <text x="262" y="70" class="t">contradiction</text>
  <text x="262" y="86" class="s">same subject</text>

  <rect class="b" x="368" y="48" width="102" height="48"/>
  <text x="380" y="70" class="t">provenance</text>
  <text x="380" y="86" class="s">hashes resolve</text>

  <rect class="b" x="486" y="48" width="102" height="48"/>
  <text x="498" y="70" class="t">independence</text>
  <text x="498" y="86" class="s">unexposed only</text>

  <rect class="b" x="608" y="48" width="104" height="48"/>
  <text x="620" y="70" class="t">promoted</text>
  <text x="620" y="86" class="s">attributed</text>

  <path class="l" d="M108 72 H128"/>
  <path class="l" d="M234 72 H246"/>
  <path class="l" d="M352 72 H364"/>
  <path class="l" d="M470 72 H482"/>
  <path class="l" d="M588 72 H604"/>

  <path class="l" d="M183 96 V134"/>
  <path class="l" d="M301 96 V134"/>
  <path class="l" d="M419 96 V134"/>
  <path class="l" d="M537 96 V134"/>

  <rect class="b" x="132" y="138" width="470" height="44"/>
  <text x="144" y="160" class="t">contested — a human decides</text>
  <text x="144" y="176" class="s">both sides move here; newest never wins; nothing is silently dropped</text>

  <path class="l dash" d="M660 96 V134"/>
  <rect class="b" x="608" y="138" width="104" height="44"/>
  <text x="620" y="160" class="t">briefing</text>
  <text x="620" y="176" class="s">shadow cuts this</text>
</svg>

1. **Evidence** — meets the threshold for its type, per the table above.
2. **Contradiction** — vector and lexical search against promoted claims on the same
   subject. A conflict sends *both* claims to `contested`. Newest-wins is tempting and
   wrong: the newer claim is not better evidence, it is just later, and silent overwrite
   is how a fleet gaslights itself.
3. **Provenance** — the observing agent is real, the timestamp falls inside the session
   window, and the cited excerpt hashes resolve.
4. **Independence** — the gate that matters most. If agent B had agent A's claim injected
   into its session and then asserts something derived from it, that is **one observation
   with two reporters**. So `independent_count` is the supporting sessions minus those
   that had a related claim injected into them, and thresholds read it, never
   `evidence_count`.

> **Read-lineage cannot be retrofitted.** By the time you want to know whether a consensus
> was independent, the sessions are gone. `injected_context` is captured on every envelope
> from the first commit, before anything reads it.

**Context fencing** is the other half: injected claims are wrapped in a delimiter and
extraction strips those spans, so an agent cannot rediscover what it was just told and
file it as a fresh observation.

## Scopes flow one way

```text
agent scope   →   repo scope   →   fleet scope
 (what I saw)     (what this       (what we all
                   project does)    believe)
```

Writes go to agent scope only; nothing writes repo or fleet scope except the promotion
gate. Reads walk up the chain, labelled with their scope. Agents call `memory_propose`;
there is deliberately no `memory_write`.

## Attribution on read — never flatten

A peer's belief must never reach a context window as bare fact:

```text
## Fleet context (peer observations — verify before relying on these)

- [cc-03, 2026-09-09, 2 independent sessions, conf 0.81]
  `cargo test --workspace` needs RUSTFLAGS=-D warnings or the clippy gate fails later.

- [cc-01, 2026-09-10, 1 session, conf 0.55, CONTESTED]
  The reattachable-exec test is flaky under colima.
```

An agent reading *"1 session, contested"* behaves differently from one reading a bare
assertion. That difference is the entire safety margin, and it costs a few tokens.

## Looking at what this produced

Every tier has a way in, and grooming is a real workflow rather than an afterthought:

```sh
ctxlake sessions <id>                              # Tier 0 — what actually happened
ctxlake claims --status promoted                   # Tier 2 — what the fleet believes
ctxlake claims --duplicates                        # pairs that may be one belief
ctxlake claims --retire <claim_id> --reason "..."  # take one out of circulation
```

**[Inspecting memory](inspecting-memory.md)** is the operator's guide to all of it — how
to read each tier, how a session and a claim point at each other, and why `--retire`
appends a `Retired` event instead of deleting.

## Quarantine — the kill switch

```sh
ctxlake quarantine <agent_id>
```

One agent's claims stop promoting, its already-promoted claims move to `contested`, and
**its capture continues** — you want the record of the failure, not a gap where it used to
be. Reach for it when one agent's contradiction rate rises, which usually means its
extraction has drifted. The gate re-checks quarantine ahead of the four gates on every
run; un-quarantining restores the *ability* to promote, while already-demoted claims stay
`contested` for a human.

> **Known gap: the `ctxlake quarantine` command does not reach the gate's quarantine log
> yet.** It writes a marker and edits the local cache mirror, which the gate never
> consults — so there is no command-line entry point to the kill switch that actually
> stops promotion. Treat the CLI command and the real mechanism as separate until this is
> wired up.

## Confidence is derived, not claimed

A model asserting "confidence 0.9" is asserting nothing checkable, and the schema carries
no self-reported confidence field. The designed source is track record: a `hypothesis`
resolves against a later `outcome` claim, and the agent's future claims are discounted or
trusted accordingly. Three rules keep that honest — the matching `outcome` must come from
a **different** agent; an unanswered hypothesis is `Expired`, **not** `Incorrect`; and
scores are blended with a neutral prior so three resolved predictions cannot swing trust
as far as three hundred, with a `low_sample` flag marking the raw case.

## Known limitations

- **Contradiction detection is a proxy, not semantics.** No NLI model here; the gate
  treats "same subject, high similarity, different text" as a conflict worth a human's
  attention, so some genuine corroboration lands in `contested`.
- **Confidence is a placeholder formula**, not the resolver above. It climbs with
  `independent_count` and is bounded; the resolver is future work.
- **`memory_search`'s vector search runs over a stand-in embedding.** Nothing here
  computes a real one, so every claim carries `embedding: None` and search falls back to a
  deterministic lexical fingerprint — a lexical proxy, not a semantic one.
- **The hypothesis/outcome agreement check is word overlap**, not negation parsing. A
  refutation phrased without a recognized cue can misread as agreement; the bias runs
  toward unearned credit an agent can still lose, not toward inflating a wrong agent.
- **Extraction does not yet ask the model for a resolution date**, so no hypothesis
  expires on the calendar — it can still resolve early when a matching outcome lands, but
  stale hypotheses otherwise accumulate.

Watch all of it in shadow mode before trusting it.

## Anti-goals

Never auto-promote a hypothesis beyond agent scope. Never let newest-wins resolve a
contradiction. Never extract from injected context, and never skip `injected_context`
capture because it looks like overhead. Never put the lake on the inference path — if it
is unreachable, agents run degraded, not broken.

## Next steps

- [Reference](reference.md) — `[summarize]` keys, `ctxlake claims`, the MCP tools
- [Adding it](adding-it.md) — seeding the belief layer from conventions you wrote
- [Security](security.md) — why anything read from the lake is untrusted input
