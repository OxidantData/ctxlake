# `ctxlake import` — backfilling history already on disk

Every runtime keeps its sessions locally. Import reads that history into the lake so a
new install starts with months of context instead of nothing.

```sh
ctxlake import --all --since 90d          # every detected runtime
ctxlake import --runtime claude-code      # just one
ctxlake import --project ~/code/myrepo    # just one project
ctxlake import --dry-run                  # count and classify, write nothing
```

Import is resumable and idempotent — events are deduplicated by content hash, so running
it twice imports nothing the second time, and an interrupted run resumes where it
stopped.

## Fidelity differs by runtime

| Runtime | Source | Fidelity | What you get |
|---|---|---|---|
| Claude Code | `~/.claude/projects/<slug>/<session>.jsonl` | **Full replay** | Everything below |
| Cursor | `~/.cursor/chats/<workspace>/<session>/` | **Metadata + prompts** | `cwd`, title, timestamps, user prompts |
| Hermes | — | **Live capture only** | Plugin captures from install onward |

This asymmetry is a property of how each runtime stores its data, not an implementation
gap we intend to close. The sections below explain each case.

## Claude Code — full replay

The JSONL transcript is structured and complete. Every record carries `cwd`,
`gitBranch`, `sessionId`, `uuid` and `parentUuid` (the full message DAG), and a
timestamp. Beyond the messages themselves:

- **`cost-state`** — `totalCostUSD`, per-model usage, lines added and removed, total tool
  duration. Cost attribution arrives already computed, so there is nothing to re-derive
  and no telemetry exporter to stand up.
- **`file-history-snapshot`** — file snapshots, which gives artifact tracking without
  walking git history.
- **`ai-title`** — a session title the runtime already generated, so digests get a
  headline without an LLM.
- **`system`** records carry hook context, tool-use ids, and durations.

Everything a live hook would have captured is present, so an imported Claude Code
session is indistinguishable from a captured one — except that it is flagged as
imported, so it is never mistaken for a live session in the roster.

## Cursor — metadata and prompts

Cursor stores each session as `meta.json` (working directory, title, created and updated
timestamps) plus `prompt_history.json`, alongside a `store.db`.

Import reads the first two. It deliberately does not read `store.db`.

> **Why we do not decode `store.db`.** It is an opaque blob store — a single table of
> `(id, data BLOB)` with an encoded payload and a `schemaVersion` field that exists
> precisely because the format changes. Decoding it would mean reverse-engineering a
> format we do not control, in a decoder that would break silently on a Cursor update
> and quietly stop importing conversations without erroring. Session metadata and
> prompts are the durable subset, so that is what import promises.

You still get, for every past Cursor session: which project it ran in, when it ran, how
long it lasted, its title, and what the user asked for. That is enough for the "what has
been tried in this repo" half of a briefing. What you do not get is the assistant's
responses and tool calls from *before* you installed ctxlake; from install onward, live
capture records everything.

## Hermes — live capture only

Hermes's plugin captures from the moment it is installed. Whether a useful on-disk
history exists to backfill depends on your Hermes installation; run `ctxlake doctor` to
see what was detected. See [runtimes/hermes.md](runtimes/hermes.md).

## Redaction runs on import too

Import uses the same pipeline as live capture: same envelope, same deduplication, and
**the same redaction**.

> **Import is not a fast path around the scrubber.** Historical transcripts are the
> likeliest place an un-redacted secret is already sitting — a `cat .env` from six months
> ago, a `printenv` in a debugging session, a curl that echoed its own headers. Those
> transcripts were written before you were thinking about this. Every imported event
> passes through the same literal-marker matching, entropy heuristic, and path denylist
> that live capture uses, and anything that trips a rule is quarantined rather than
> stored. See [security.md](security.md).

Bronze is immutable. A secret written into it is permanent, which is why scrubbing
happens on the way in rather than in a later pass.

## Conventions become claims

Import also reads the memory files you already maintain — `CLAUDE.md`, `AGENTS.md`,
`.cursor/rules/*` — and files them as claims attributed to a human and promoted on
arrival. See [adopting.md](adopting.md) for why those skip the evidence gate.

One detail worth knowing: files like these are read into *every* session, so without
deduplication they would dominate storage and skew every frequency count in the lake.
Content hashing handles it — a 24 KB `AGENTS.md` read by two hundred sessions is stored
once.

## What import costs

Reading is local and cheap. The write side is one upload per batch, and the storage is
negligible — a year of a five-agent fleet is a couple of gigabytes before compaction.
Import does not call an LLM, so it costs nothing beyond object-storage requests, even
when the belief layer is enabled.

## Next steps

- [adopting.md](adopting.md) — the rest of the day-one path
- [security.md](security.md) — redaction rules, quarantine, and the threat model
- [runtimes/cursor.md](runtimes/cursor.md) — Cursor specifics
- [layout.md](layout.md) — where imported sessions land in the bucket
