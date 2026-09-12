# Adopting ctxlake — starting from what you already run

You already use Claude Code, Cursor, or Hermes. The question is whether adopting ctxlake
means migrating anything. It does not: nothing is rerouted, nothing is reconfigured
beyond one merge-in-place install, and you start with history rather than an empty lake.

## Your existing installs keep working

`ctxlake install` only ever **appends**. It reads your current configuration, adds its
own entries, and leaves everything else byte-for-byte intact.

This matters more than it sounds. Real configs are large and already populated — a
`~/.claude/settings.json` with a dozen hook events wired up, a `~/.cursor/hooks.json`
already driving another tool. An installer that rewrote those files would destroy work
you did not know it was touching.

So:

- existing hook entries are preserved, and ctxlake's are added alongside them
- a `.bak` is written before any change
- re-running `install` changes nothing — it is idempotent
- `ctxlake uninstall` removes exactly what it added, and nothing else
- `--dry-run` prints the diff first

> **ctxlake is not the only hook consumer, and does not assume it is.** If another tool
> already has hooks registered — an observability agent, a formatter, a policy gate —
> both fire independently. Neither proxies the other. Coexistence is a supported
> configuration and a tested one.

## You start with history, not an empty lake

This is the difference between installing a tool and waiting a week to see value, and
installing a tool that immediately shows you what your fleet has been doing for months.

```sh
ctxlake import --all --since 90d
```

Fidelity differs by runtime, and [import.md](import.md) is honest about where and why.
Claude Code transcripts replay in full; Cursor yields session metadata and prompts.

## Your existing conventions seed the belief layer

You already wrote down how your projects work. None of it should have to be re-derived
from scratch by an agent reading transcripts.

| Source | Imported as |
|---|---|
| `CLAUDE.md`, `AGENTS.md` | `convention` claims at repo scope |
| `.cursor/rules/*` | `convention` claims at repo scope |
| Hermes `MEMORY.md` / `USER.md` | `preference` claims at agent scope |

These arrive attributed to a human and **promoted on arrival** — they skip the evidence
gate, because a person asserting something directly is stronger evidence than two agents
agreeing about it. They carry a marker showing they were imported, so if one later
conflicts with an extracted claim the contradiction is attributed correctly rather than
blamed on an agent.

The practical effect: the fleet-context block in a briefing is useful on day one instead
of after three weeks of extraction, and the contradiction gate has something real to
check new claims against immediately.

## Your existing MCP setup already works

All three runtimes are MCP clients, so `ctxlake install` adds one stdio server entry.
There is no network service to stand up, no port to open, and no auth to configure — the
server runs as a child process of the agent.

Wherever a runtime cannot inject context automatically, MCP is the fallback: the agent
calls `fleet_status()` itself rather than being told. See [mcp.md](mcp.md).

## What day one looks like

```sh
ctxlake init --store s3://my-bucket/ctxlake --fleet myteam
ctxlake doctor                      # backend capability matrix + runtimes detected
ctxlake import --all --since 90d    # backfill from disk; redacts as it goes
ctxlake install --all               # merge hooks into every runtime found
ctxlake status                      # and the next session opens with a real briefing
```

Run `ctxlake doctor` before `install`, not after. It tells you which conditional-write
primitives your object store actually supports and which runtimes it found, so a
misconfigured bucket surfaces immediately rather than as silent non-capture later.

## What adopting it does not do

- **It does not change how your agents behave by default.** Collision checks warn; they
  do not block until you ask them to.
- **It does not route your traffic anywhere.** There is no service in the middle. Hooks
  write to a local spool; a daemon moves that to your own bucket.
- **It does not require an LLM.** See [summarization.md](summarization.md).
- **It does not need every machine onboarded at once.** An agent without ctxlake is
  simply invisible to the roster — the others still coordinate with each other. Adoption
  can be incremental, though the value scales with coverage.

## Next steps

- [getting-started.md](getting-started.md) — install and first briefing
- [import.md](import.md) — what backfills, and at what fidelity
- [runtimes/claude-code.md](runtimes/claude-code.md) — per-runtime detail
- [coordination.md](coordination.md) — what the fleet actually shares
