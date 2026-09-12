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
agent calls `fleet_status()` itself. See [reference.md](reference.md#mcp-tools).

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
ctxlake import --all --since 90d          # every detected runtime
ctxlake import --runtime claude-code      # just one
ctxlake import --project ~/code/myrepo    # just one project
ctxlake import --dry-run                  # count and classify, write nothing
```

Import is resumable and idempotent — events are deduplicated by content hash, so a
second run imports nothing and an interrupted run resumes where it stopped. It calls no
LLM, so it costs nothing beyond object-storage requests.

| Runtime | Source | Fidelity | What you get |
|---|---|---|---|
| Claude Code | `~/.claude/projects/<slug>/<session>.jsonl` | **Full replay** | Messages, `cwd`, `gitBranch`, the full `uuid`/`parentUuid` DAG, per-model cost and token usage, file snapshots, and the runtime's own generated session title |
| Cursor | `~/.cursor/chats/<workspace>/<session>/` | **Metadata + prompts** | `cwd`, title, timestamps, user prompts |
| Hermes | — | **Live capture only** | The plugin captures from install onward |

This asymmetry is a property of how each runtime stores its data, not a gap we intend to
close. An imported Claude Code session is indistinguishable from a captured one, except
that it is flagged imported so it never appears in the roster as live.

> **Cursor's `store.db` is deliberately not decoded.** It is an opaque blob store —
> `(id TEXT, data BLOB)` with a `schemaVersion` field that exists because the format
> changes. A reverse-engineered parser would break silently on a Cursor update and
> import plausible-looking garbage, and bronze is immutable, so that garbage would be
> permanent. You get real prompts and real timing for past sessions, but no
> reconstructed tool-call detail. Live capture from install onward has no such gap.

Hermes backfill depends on your installation; `ctxlake doctor` reports what it detected.

## Redaction runs on import too

Imported events use the same envelope, the same deduplication, and the **same
redaction** as live capture.

> **Import is not a fast path around the scrubber.** Historical transcripts are the
> likeliest place an un-redacted secret already sits — a `cat .env` from six months ago,
> a `printenv`, a curl that echoed its own headers. Every imported event passes the same
> literal-marker matching, entropy heuristic and path denylist, and anything that trips a
> rule is quarantined rather than stored. See [security.md](security.md).

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
- **It does not require an LLM.** See [memory.md](memory.md).
- **It does not need every machine onboarded at once.** An agent without ctxlake is
  invisible to the roster; the others still coordinate. Value scales with coverage.

## Next steps

- [getting-started.md](getting-started.md) — the install itself
- [runtimes.md](runtimes.md) — per-runtime behaviour and honest gaps
- [security.md](security.md) — redaction rules, quarantine, and the threat model
