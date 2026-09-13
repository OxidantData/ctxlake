# ctxlake documentation

Zero-compute coordination for fleets of coding agents. Object storage is the only shared
substrate — no server, no database, no broker.

New here? Start with [Getting started](getting-started.md). Debugging something?
[Architecture](architecture.md) is an operator's map of every component.

| Doc | What it covers |
|---|---|
| [Getting started](getting-started.md) | Install, init, doctor, wire up a runtime, first briefing |
| [How it works](how-it-works.md) | The three planes, and why nothing needs a lock |
| [Adding it](adding-it.md) | Adopting it alongside what you already run, and backfilling history |
| [Memory](memory.md) | The three summarization tiers, claims, and the four promotion gates |
| [Inspecting memory](inspecting-memory.md) | Reading each tier, and retiring a claim that has gone wrong |
| [Runtimes](runtimes.md) | Claude Code, Cursor and Hermes — event mapping and honest gaps |
| [Storage](storage.md) | Backends, the CAS matrix, bucket layout, and the cost arithmetic |
| [Reference](reference.md) | Every command, flag, config key and MCP tool |
| [Architecture](architecture.md) | Component map, failure modes, knobs, what this does not guarantee |
| [Security](security.md) | Redaction, quarantine, prompt injection, IAM, threat model |

[AGENTS.md](../AGENTS.md) in the repository is the working agreement and the invariants.
