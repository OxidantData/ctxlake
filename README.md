# ctxlake — a zero-compute context lake for agent fleets

Keep a fleet of coding agents in sync through object storage alone. No server, no
database, no broker. An agent starting anywhere in the fleet knows who else is working,
on what, and what already happened in this repo.

**Status: pre-alpha.** Interfaces will change.

```sh
ctxlake init --store s3://my-bucket/ctxlake --fleet myteam
ctxlake doctor                      # probe the backend, detect installed runtimes
ctxlake import --all --since 90d    # backfill the history already on your disk
ctxlake install --all               # merge hooks into Claude Code, Cursor, Hermes
ctxlake status                      # who is working on what, right now
```

## The problem

Two agents edit the same file. Two agents spend twenty minutes solving the same problem.
A third re-derives something the first learned yesterday and threw away. The expensive
failure is not the merge conflict — git handles that — it is the wasted work and the
lost knowledge.

## The approach

Object storage is the only shared state. Coordination uses compare-and-swap on
objects — the same primitive Delta Lake and Iceberg use for their commits — so there is
nothing to operate and nothing to pay for when idle.

Three planes, each with exactly one write pattern:

| Plane | What it holds | Write pattern | Latency |
|---|---|---|---|
| **Control** (`live/`) | who is doing what now | compare-and-swap | 20–100 ms |
| **Data** (`sessions/`) | what happened | single-writer append | async |
| **Serving** (`snapshot/`) | what is believed | immutable + pointer swap | microseconds, local |

The object store is never on the hook path in either direction — writes go to a local
spool, reads come from a local cache. Your agent never waits on a network call.

## Runtimes

| | Claude Code | Cursor Agent CLI | Hermes |
|---|---|---|---|
| Mechanism | command hook | command hook | Python plugin |
| Capture | yes | yes | yes |
| Briefing injection | yes | yes | see docs |
| Collision warning | yes | yes | see docs |
| MCP tools | yes | yes | yes |

Installs merge into your existing config. Nothing is clobbered, and ctxlake coexists with
other hook consumers.

## Backends

S3, GCS, MinIO, Cloudflare R2, and the local filesystem. They differ in which conditional
-write primitives they support — MinIO rejects `If-None-Match: *`, GCS uses generation
numbers — so `ctxlake doctor` executes each primitive against your bucket and prints a
pass/fail matrix rather than assuming.

## Documentation

Start with [`docs/getting-started.md`](docs/getting-started.md). The full index is in
[`docs/README.md`](docs/README.md). If you are debugging,
[`docs/architecture.md`](docs/architecture.md) is an operator's map of every component.

## Does it need an LLM?

Not for coordination. Session digests are derived mechanically from tool calls, and
handoff notes are written by the agent that did the work, on the key it already uses.
An API key is needed only for the optional belief layer that extracts durable claims
across sessions — see [`docs/summarization.md`](docs/summarization.md). Local models
via Ollama are a first-class option; no transcript has to leave the host.

## License

AGPL-3.0-or-later, with commercial licenses available — see [`LICENSE`](LICENSE),
[`COMMERCIAL.md`](COMMERCIAL.md), [`NOTICE`](NOTICE).
