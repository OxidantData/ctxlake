# Summarization — how a session becomes memory

Three tiers turn a finished session into something the next agent can use. **The
default needs no LLM API key at all.** Only the third tier does, and it is optional.

```text
sealed session
   │
   ├─ Tier 0  structural digest      always on, free, no LLM
   │          → fleet_history, the "previous sessions" block in the briefing
   │
   ├─ Tier 1  agent self-handoff     default, no extra key, no extra cost
   │          → a note attached to the session. NOT a belief.
   │
   └─ Tier 2  batch extraction       optional, needs a configured LLM
              → candidate claims → four gates → promoted → snapshot → briefing
```

## Tier 0 — structural. No LLM, always on.

Derived mechanically from captured events, so it cannot hallucinate:

- files touched, commands run with exit codes, tests run and their results
- commits produced, `git_sha` before and after, duration, turn count
- tokens and cost, read straight from the runtime's own accounting
- outcome: clean exit, error, or abandoned
- **friction signals** — the same command failing repeatedly, one file edited over and
  over, a session abandoned after a run of failures

That last one earns its place. A line like *"abandoned after 4 failed `cargo test -p
oxidant-connect` runs"* is pure arithmetic over tool-call exit codes. It is often the
most useful line in a briefing, it costs nothing, and it is never wrong.

## Tier 1 — the agent summarizes itself. The default.

**The cheapest summarizer is the agent that just did the work.** It is still running,
the whole session is already in its context window, and that context is cache-warm.
Asking it for a handoff costs one short turn on a key you already pay for — rather than
a separate batch job re-reading the entire transcript against a separate key.

At turn end a hook returns a one-time nudge, and the agent calls the `fleet_handoff`
MCP tool. Claude Code uses `Stop`, Cursor uses `stop`, Hermes uses `on_session_end`.

> **Tier 1 produces notes, not beliefs.** An agent summarizing its own session is
> self-reporting, which is the confirmation-bias problem in miniature. So a handoff is
> *episodic* — "here is what I did and what failed" — attached to its session and
> attributed to it. It never enters the belief layer and it never skips the gate. Only
> Tier 2 proposes claims, and even those land as candidates.

## Tier 2 — batch extraction. Optional, needs a key.

Runs inside the maintenance job over sealed sessions that have not been extracted yet,
producing candidate claims against a strict schema.

**If no LLM is configured, ctxlake runs Tiers 0 and 1 and the belief layer stays
empty.** Coordination, history, briefings, and handoffs all work unchanged. That is the
honest answer to "does this need an LLM": only for the belief layer, and only if you
want one.

Three properties worth knowing:

- **Batch, not live.** Extraction is definitionally not latency-sensitive, and batch
  processing is half the price. Results come back out of order, so they are keyed by
  request id, never by position.
- **Prompt caching.** The system prompt and claim schema are byte-identical across every
  session; only the transcript varies. The stable part sits before the cache breakpoint,
  so you pay full price for it once rather than once per session.
- **Structured output.** A malformed claim is a parse error, not a bad memory.

## Configuring the LLM

```toml
# ctxlake.toml
[summarize]
mode = "agent"              # "none" | "agent" | "batch" | "both"   (default: agent)

[summarize.batch]           # only read when mode includes batch
provider     = "anthropic"  # anthropic | openai-compatible | ollama
model        = "claude-haiku-4-5"
api_key_env  = "ANTHROPIC_API_KEY"   # the NAME of an env var, never the key itself
base_url     = ""                    # set this for ollama or a self-hosted endpoint
use_batch_api        = true
max_sessions_per_run = 50
max_input_tokens     = 8000          # per session; oldest turns truncated first
```

> **`api_key_env` holds a name, not a value.** Config files get committed. ctxlake reads
> the named environment variable at run time; `ctxlake doctor` reports whether the name
> resolves, never what it resolves to. There is no config field that accepts a key, so
> there is no way to leak one this way by accident.

### Running it entirely locally

```toml
[summarize.batch]
provider = "ollama"
model    = "qwen2.5:14b"
base_url = "http://localhost:11434"
```

No transcript leaves the host, and extraction costs nothing. This is a first-class path,
not a degraded one — if your environment cannot send transcripts to a third party, this
is the configuration you want, and nothing else in ctxlake changes.

### Turning it off

```toml
[summarize]
mode = "none"
```

Coordination, session history, briefings, and Tier 0 digests all keep working. You lose
the belief layer and nothing else.

## What it costs

Roughly 8K input and 500 output tokens per session:

| Fleet | Live requests | Batched |
|---|---:|---:|
| 40 sessions/day | ~$0.42/day | **~$0.21/day** |
| 400 sessions/day (50 agents) | ~$4.20/day | **~$2.10/day** |

Prompt caching on the stable schema prefix reduces it further. This is still the largest
line item in the system — it scales with session count, not with how much you store —
which is why it is the one tier you can switch off.

## From candidate to memory

Extraction only ever produces *candidates*. Promotion runs four gates in order, and
failing one sends a claim to review rather than silently dropping it — you want to see
what is being rejected.

1. **Evidence** — meets the threshold for its type. `environment` needs one observation;
   `convention` needs two independent ones; `preference` needs a human; `hypothesis`
   never auto-promotes to fleet scope.
2. **Contradiction** — vector and lexical search against promoted claims on the same
   subject. A conflict sends *both* claims to `contested` for a human. Newest never wins;
   silent overwrite is how a fleet gaslights itself.
3. **Provenance** — the observing agent is real, the timestamp falls inside the session
   window, and the cited excerpt hashes resolve.
4. **Independence** — how many evidence sessions did *not* already read a related claim.
   Two agents agreeing because one read the other is one observation with two reporters,
   not corroboration. The threshold reads this number, never the raw evidence count.

Inspecting and controlling it:

```sh
ctxlake claims --status candidate --explain   # which gate rejected what, and why
ctxlake claims --status contested             # the human review queue
ctxlake quarantine <agent_id>                 # stop promoting one agent's claims
```

### Shadow mode

```toml
[summarize]
mode = "shadow"
```

Runs the whole chain — extraction, gates, snapshot — with agent reads disabled. Claims
accumulate and the gate reports, but nothing reaches a context window. This is the
setting to sit in while you read reject rates, and staying there costs nothing.

> **Sit in shadow mode longer than feels necessary.** Running extraction for weeks with
> nobody reading the output is how you find out your claims are wrong *before* they are
> in an agent's context window. The temptation to skip it is strong, and the cost of
> skipping it is a fleet that confidently believes wrong things.

## Next steps

- [memory.md](memory.md) — claims, scopes, and attribution on read
- [import.md](import.md) — seeding the belief layer from conventions you already wrote
- [config.md](config.md) — the full `ctxlake.toml` reference
- [scaling.md](scaling.md) — why extraction is the dominant cost at scale
