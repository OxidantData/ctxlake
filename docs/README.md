# ctxlake documentation

Zero-compute coordination for fleets of coding agents. Object storage is the only
shared substrate — no server, no database, no broker.

New here? Start with [getting-started.md](getting-started.md). Debugging something?
[architecture.md](architecture.md) is an operator's map of every component.

## User guides

| Doc | What it covers |
|---|---|
| [getting-started.md](getting-started.md) | Install, init, doctor, wire up a runtime, first briefing |
| [adopting.md](adopting.md) | "I already use Claude Code, Cursor and Hermes — can I just start?" |
| [import.md](import.md) | Backfilling the history already on your disk, and its per-runtime fidelity |
| [concepts.md](concepts.md) | The three planes and the one-write-pattern-per-plane rule |
| [coordination.md](coordination.md) | Roster and intents, and why concurrent maintenance needs no lock |
| [summarization.md](summarization.md) | The three tiers; why the default needs no LLM key; configuring one if you want it |
| [memory.md](memory.md) | Claims, the four promotion gates, scopes, attribution, shadow mode |
| [cli.md](cli.md) | Command reference |
| [config.md](config.md) | `ctxlake.toml` reference |
| [mcp.md](mcp.md) | The MCP tool surface |

## Runtimes

| Doc | What it covers |
|---|---|
| [runtimes/claude-code.md](runtimes/claude-code.md) | Hook mapping, injection, blocking, coexistence |
| [runtimes/cursor.md](runtimes/cursor.md) | `hooks.json` v1, event mapping, import fidelity |
| [runtimes/hermes.md](runtimes/hermes.md) | The Python plugin, event mapping, known gaps |

## Deployment and operations

| Doc | What it covers |
|---|---|
| [storage.md](storage.md) | S3, GCS, MinIO, R2, local — and the CAS capability matrix |
| [layout.md](layout.md) | Bucket layout reference |
| [scaling.md](scaling.md) | Where this does not scale, with the cost arithmetic |
| [security.md](security.md) | Redaction, quarantine, prompt injection, IAM policy, threat model |
| [troubleshooting.md](troubleshooting.md) | Symptom → what to check → why |

## Contributor internals

| Doc | What it covers |
|---|---|
| [architecture.md](architecture.md) | Component inventory, data flow, failure modes, what this does not guarantee |
| [../AGENTS.md](../AGENTS.md) | Working agreement and the invariants |
