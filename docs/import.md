# `ctxlake import` — backfilling history already on disk

Every runtime keeps its sessions locally. Import reads that history into the lake so a
new install starts with months of context instead of nothing.

```sh
ctxlake import --runtime hermes --since 90d   # one runtime, the last 90 days
ctxlake import --all                          # every runtime with history here
ctxlake import --runtime hermes --dry-run     # count and classify, write nothing
ctxlake import --runtime hermes --source /path/to/state.db   # a non-default location
```

Import is resumable and idempotent — events are deduplicated by content hash, so running
it twice imports nothing the second time, and an interrupted run resumes where it
stopped.

> **What is wired up today.** `--runtime hermes` is implemented and reads a real
> `~/.hermes/state.db`. Claude Code and Cursor are described below because the
> *fidelity of the data they leave on disk* is a property of those runtimes and worth
> knowing before you install anything — but their readers are not written yet, and
> passing `--runtime claude-code` says so and exits non-zero rather than accepting the
> flag and reporting a successful import of nothing. Silent success is the failure mode
> this project has been bitten by before; see AGENTS.md.

## Fidelity differs by runtime

| Runtime | Source | Fidelity | What you get |
|---|---|---|---|
| Claude Code | `~/.claude/projects/<slug>/<session>.jsonl` | **Full replay** | Everything below |
| Hermes | `~/.hermes/state.db` (SQLite) | **Full replay** | Messages, structured tool calls, per-session tokens and cost, titles, compaction markers |
| Cursor | `~/.cursor/chats/<workspace>/<session>/` | **Metadata + prompts** | `cwd`, title, timestamps, user prompts |

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

## Hermes — full replay from `state.db`

> **This page used to say Hermes was "live capture only, no importable history." That
> was wrong.** Hermes keeps a complete SQLite state database at `~/.hermes/state.db` —
> on the install this was verified against, 98 MB across 39 tables, with 22 sessions
> and 17,321 messages in it. The correction matters because the old claim told people
> their Hermes history was unrecoverable and that the only way to get context into the
> lake was to install and wait.

`ctxlake import --runtime hermes` reads four of those tables:

| Table | What import takes from it |
|---|---|
| `sessions` | the session id and the title Hermes already generated for its own UI |
| `messages` | every user, assistant and tool message, in timestamp order |
| `session_model_usage` | input/output/cache tokens and cost, per session |
| `schema_version` | reported in the import summary, so a changed shape is visible |

and maps them onto the envelope like this:

| Source | Envelope |
|---|---|
| a session's first / last message timestamp | `session_start` / `session_end` |
| `sessions.title` | `session_start`'s `content` — a digest gets a headline with no LLM call |
| `role = 'user'` | `prompt`, `role: "user"`, text in `content` |
| `role = 'assistant'` | `assistant`, `role: "assistant"`, text in `content` |
| `role = 'tool'` | `tool_call`: `tool.name` from `tool_name`, `tool.result` from `content` |
| the assistant row's `tool_calls` JSON | `tool.input`, matched to the result by `tool_call_id` |
| `messages.id` | `message_id`; the requesting assistant row becomes `parent_message_id` |
| `timestamp` (a unix epoch **float**) | `emitted_at`, as RFC 3339 with milliseconds |
| `session_model_usage` | `usage` on that session's `session_end` |
| `compacted = 1` | `compact` — see below |

Everything a live Hermes hook would have captured is present, plus the message text
that live capture does not yet carry (see [runtimes/hermes.md](runtimes/hermes.md)'s
transcript gap). Imported events are flagged `imported`, so a backfilled session is
never mistaken for a live one in the roster.

### Import recovers a compaction marker that live capture cannot

This is the interesting asymmetry, and it runs the opposite way to every other
runtime's. Hermes has **no compaction hook** — there is no `pre_compact` event for a
shell hook to subscribe to, so a live-captured Hermes session has a permanent hole
where the moment its context was discarded should be. That gap is real and
[runtimes/hermes.md](runtimes/hermes.md) documents it.

But the database records the fact afterwards: compacted messages carry `compacted = 1`.
So import emits an `EventType::Compact` where the data shows one — **one marker per
contiguous run of compacted rows, stamped at the run's last message**, because that
instant is where the discarded region ends. One per row would report a 500-message
compaction as 500 compactions.

`compacted = 1` means *this message was compacted away*, not *this message is the
compaction summary* — the latter is a separate `_compressed_summary` flag. Verified two
ways: Hermes's own tests assert compaction sets `active = 0, compacted = 1` together,
and across a real 17,329-message database every one of the 2,835 `compacted = 1` rows
also had `active = 0`, with no exceptions.

The consequence is worth stating plainly: for Hermes, an *imported* session can be
richer than a *captured* one. Everywhere else in this document import is the lossy
path.

### Opening a live agent's database

`state.db` belongs to a running process. Import opens it through the SQLite URI
`file:<path>?mode=ro&immutable=1` with `SQLITE_OPEN_READ_ONLY`:

- read-only makes a write impossible rather than merely unintended, and
- `immutable=1` is what removes the *locking* — no shared lock, no `-shm` file, nothing
  for the running agent to contend on.

The honest cost, stated here rather than discovered later: if Hermes is in WAL mode with
frames not yet checkpointed into the main file, an immutable read does not see them, so
import reads a slightly older view of a session that is still going. That is the right
trade — the alternative is taking a lock on a running agent's database — and it costs
nothing, because import is idempotent: the next run picks up what the previous one could
not see.

### Verified against one install, and it will fail loudly if that changes

Every column above was read from **one** live Hermes install's schema. Hermes ships a
`schema_version` table precisely because that shape is not frozen, and a future version
could rename `tool_calls` or move message text out of `content`.

A lenient importer would then succeed, report zero events, and look exactly like "you
have no history." So the importer requires every column it maps, by name, before it
reads a single row, and refuses with the missing columns listed and the observed
`schema_version` quoted back:

```text
Hermes's `messages` table is missing tool_calls (schema_version 12); this importer was
verified against a schema that has them, and importing without them would silently drop
history rather than fail.
```

The version number itself is reported, never compared against a constant — we have seen
one install, and a version this code has not seen is not by itself evidence of anything.
The column check is the contract.

### What Hermes has that the envelope does not carry

- **`reasoning_tokens`.** There is no envelope field for it, and folding it into
  `output_tokens` would silently change what that field means for one runtime. The
  schema rule is add a field, never reinterpret one.
- **Per-message model attribution.** `session_model_usage` is per-(session, model);
  `messages` has no model column. Attributing a message to a model from the usage rows'
  `first_seen`/`last_seen` windows would be a guess, and bronze is immutable. Usage is
  summed onto the session's one `session_end` event instead — which is also the only
  placement that does not double-count, since the digest sums `usage` across every
  event in a session.
- **`cwd` / repo / branch.** The `sessions` table has no working-directory column, so
  imported Hermes sessions have none. Live capture does get it (the hook payload carries
  `cwd`).
- **`effect_disposition`, `finish_reason`, `active`, `observed`.** No verified meaning,
  so no mapping. Rows are imported regardless of `active`/`observed`: filtering history
  on a flag nobody has verified is how an import quietly loses half a transcript.

`--since` selects **whole sessions by last activity**, never individual messages.
Importing half a session would produce a digest whose duration, turn count and friction
signals are arithmetic over a truncated transcript, which is worse than not importing
it at all.

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

This is structural, not a promise: import does not have a redactor of its own. It calls
the same `ctxlake_core::Redactor` the hook calls, through the same helpers, and appends
to the same local spool the hook appends to — the one `ctxlake sync` drains. There is no
second implementation of a security control here to drift out of step with the first.

## Conventions become claims

> **Not implemented yet.** This section describes the intended behaviour; nothing in
> `ctxlake import` reads memory files today. It is written down here because it shapes
> the design (the dedup story below is why it can work at all), not because you can run
> it.

Import also reads the memory files you already maintain — `CLAUDE.md`, `AGENTS.md`,
`.cursor/rules/*` — and files them as claims attributed to a human and promoted on
arrival. See [adopting.md](adopting.md) for why those skip the evidence gate.

One detail worth knowing: files like these are read into *every* session, so without
deduplication they would dominate storage and skew every frequency count in the lake.
Content hashing handles it — a 24 KB `AGENTS.md` read by two hundred sessions is stored
once.

## What import costs

Reading is local and cheap. Import itself never touches the object store: it writes
envelopes to the local spool, and `ctxlake sync` carries them up in batches like any
other event, so the write side is one upload per batch and the storage is negligible —
a year of a five-agent fleet is a couple of gigabytes before compaction. Import does not
call an LLM, so it costs nothing beyond object-storage requests, even when the belief
layer is enabled.

Two practical consequences: a backfill does not appear in the lake until a daemon has
run (`ctxlake sync`), and the dedup ledger lives at `~/.ctxlake/import/<runtime>.ledger`
rather than in the spool — the daemon deletes spool files once they are uploaded, so a
ledger there would be erased by its own success and the next import would replay
everything into immutable bronze a second time.

## Next steps

- [adopting.md](adopting.md) — the rest of the day-one path
- [security.md](security.md) — redaction rules, quarantine, and the threat model
- [runtimes/cursor.md](runtimes/cursor.md) — Cursor specifics
- [layout.md](layout.md) — where imported sessions land in the bucket
