# Adding it to what you already run

Adopting ctxlake migrates nothing. One merge-in-place install, and you start with
history rather than an empty lake.

## Your existing installs keep working

`ctxlake install` only ever **appends**. It reads your current configuration, adds its
own entries, and leaves everything else byte-for-byte intact.

- existing hook entries are preserved; ctxlake's are added alongside them
- a `.bak` is written before any change
- re-running `install` changes nothing — it is idempotent
- `ctxlake uninstall` removes exactly what it added, and nothing else
- `--dry-run` prints the diff first

> **ctxlake does not assume it is the only hook consumer.** If another tool already has
> hooks registered — an observability agent, a formatter, a policy gate — both fire
> independently. Neither proxies the other. Coexistence is supported and tested.

All three runtimes are MCP clients, so `ctxlake install` also adds one stdio server
entry. No service to stand up, no port, no auth — the server runs as a child process of
the agent. Where a runtime cannot inject context automatically, MCP is the fallback: the
agent calls `fleet_status()` itself. See [Reference](reference.md#mcp-tools).

## Day one

```sh
ctxlake init --store s3://my-bucket/ctxlake --fleet myteam
ctxlake doctor                      # backend capability matrix + runtimes detected
ctxlake import --all --since 90d    # backfill from disk; redacts as it goes
ctxlake install --all               # merge hooks into every runtime found
ctxlake status                      # and the next session opens with a real briefing
```

## Importing history

```sh
ctxlake import --runtime hermes --since 90d   # one runtime, a time window
ctxlake import --all --since 90d              # every runtime with a reader
ctxlake import --dry-run                      # count and classify, write nothing
ctxlake import --source /path/to/state.db     # a non-default location
```

Import is resumable and idempotent — events are deduplicated by content hash, so a
second run imports nothing and an interrupted run resumes where it stopped. It calls no
LLM, so it costs nothing beyond object-storage requests.

> **Only `--runtime hermes` is wired up today.** Claude Code and Cursor are described
> below because the readers are specified and their sources verified, but passing
> `--runtime claude-code` says so and exits non-zero rather than accepting the flag and
> reporting a successful import of nothing.

| Runtime | Source | Fidelity | What you get |
|---|---|---|---|
| Hermes | `~/.hermes/state.db` (SQLite) | **Full replay** — *implemented* | Messages, structured tool calls, per-session tokens and cost, titles, compaction markers |
| Claude Code | `~/.claude/projects/<slug>/<session>.jsonl` | **Full replay** — planned | Messages, `cwd`, `gitBranch`, the `uuid`/`parentUuid` DAG, per-model cost and tokens, file snapshots, the runtime's own session title |
| Cursor | `~/.cursor/chats/<workspace>/<session>/` | **Metadata + prompts** — planned | `cwd`, title, timestamps, user prompts |

This asymmetry is a property of how each runtime stores its data, not a gap we intend to
close. An imported session is indistinguishable from a captured one, except that it is
flagged imported so it never appears in the roster as live.

> **Cursor's `store.db` is deliberately not decoded.** It is an opaque blob store —
> `(id TEXT, data BLOB)` with a `schemaVersion` field that exists because the format
> changes. A reverse-engineered parser would break silently on a Cursor update and
> import plausible-looking garbage, and bronze is immutable, so that garbage would be
> permanent. You get real prompts and real timing for past sessions, but no
> reconstructed tool-call detail.

### Hermes imports *more* than Hermes captures

Everywhere else on this page import is the lossy path. Hermes runs the other way.

It has no compaction hook — no `pre_compact` event exists for a shell hook to subscribe
to — so a live-captured Hermes session has a permanent hole where the moment its context
was discarded should be. The database records the fact afterwards (`compacted = 1`), so
import emits a compaction marker where live capture cannot: one per contiguous run of
compacted rows, stamped at the run's last message, because that instant is where the
discarded region ends.

Import gates on `schema_version` and the columns it needs, failing loudly if Hermes
changes shape rather than importing a silently truncated view of your history.

`state.db` belongs to a running process, so import opens it through the SQLite URI
`file:<path>?mode=ro&immutable=1` — read-only makes a write impossible rather than
merely unintended, and `immutable=1` removes the locking entirely, so there is nothing
for the running agent to contend on. The cost, stated rather than discovered: under WAL
with un-checkpointed frames, an immutable read sees a slightly older view of a session
still in progress. Import is idempotent, so the next run picks up the rest.

## Redaction runs on import too

Imported events use the same envelope, the same deduplication, and the **same
redaction** as live capture.

> **Import is not a fast path around the scrubber.** Historical transcripts are the
> likeliest place an un-redacted secret already sits — a `cat .env` from six months ago,
> a `printenv`, a curl that echoed its own headers. Every imported event passes the same
> literal-marker matching, entropy heuristic and path denylist, and anything that trips a
> rule is quarantined rather than stored. See [Security](security.md).

## Your conventions seed the belief layer

| Source | Imported as |
|---|---|
| `CLAUDE.md`, `AGENTS.md` | `convention` claims at repo scope |
| `.cursor/rules/*` | `convention` claims at repo scope |
| Hermes `MEMORY.md` / `USER.md` | `preference` claims at agent scope |

These arrive attributed to a human and **promoted on arrival** — they skip the evidence
gate, because a person asserting something is stronger evidence than two agents agreeing
about it. They carry an imported marker, so a later contradiction is attributed
correctly rather than blamed on an agent. The fleet-context block is useful on day one
instead of after three weeks of extraction.

Files like these are read into *every* session, so content hashing keeps them from
dominating storage and skewing frequency counts: a 24 KB `AGENTS.md` read by two hundred
sessions is stored once.

## What it does not do

- **It does not change how your agents behave by default.** Collision checks warn; they
  do not block until you ask them to.
- **It does not route your traffic anywhere.** Hooks write to a local spool; a daemon
  moves that to your own bucket.
- **It does not require an LLM.** See [Memory](memory.md).
- **It does not need every machine onboarded at once.** An agent without ctxlake is
  invisible to the roster; the others still coordinate. Value scales with coverage.

## Next steps

- [Getting started](getting-started.md) — the install itself
- [Runtimes](runtimes.md) — per-runtime behaviour and honest gaps
- [Security](security.md) — redaction rules, quarantine, and the threat model
