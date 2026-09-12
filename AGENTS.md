# Working agreement — ctxlake

Read this before writing code. It encodes the invariants that make the design work;
violating one is a correctness bug, not a style disagreement.

## What this is

A zero-compute coordination layer for fleets of coding agents. Object storage is the
only shared substrate — no server, no database, no broker. Agents running under Claude
Code, Cursor Agent CLI, and Hermes see who else is working, on what, and what already
happened in this repo.

## Start here (maps)

| File | What it holds |
|---|---|
| [`docs/README.md`](docs/README.md) | Docs index |
| [`docs/architecture.md`](docs/architecture.md) | Operator's map: every component, data flow, failure modes |
| [`docs/concepts.md`](docs/concepts.md) | The three planes and the one-write-pattern rule |
| [`docs/scaling.md`](docs/scaling.md) | Where this does not scale, with the cost arithmetic |

## The invariants

**1. The object store is never on the hook path, in either direction.**
Writes go hook → local spool → daemon → store. Reads go store → daemon → local cache →
hook. A hook that opens a socket is a bug. S3 p50 is 20–100 ms and p99 exceeds 200 ms;
a network call per `Edit` makes the tool unusable.

**2. `ctxlake-hook` links nothing heavy.**
Its only dependencies are `serde_json`, `aho-corasick`, `ulid`, `sha2`. No `object_store`,
no `tokio`, no `reqwest`, no `aws-sdk-*`. CI asserts this with `cargo tree`. Budget is
5 ms p99; it fires on every tool call.

**3. One write pattern per plane.**
- `live/` — CAS only (`PutMode::Update`). Small JSON.
- `sessions/`, `claims/events/` — single-writer append. Never two writers on one key.
- `snapshot/` — immutable content-addressed publish, then a CAS pointer swap.

No key is appended to by two processes. There is no cross-key atomicity on any backend,
so every operation is single-object-atomic or log-then-derive. A change that needs a
two-key transaction is a request to add a database — escalate, do not improvise.

**4. Leases are CAS-only, never put-if-absent.**
MinIO rejects `If-None-Match: *` (minio/minio#20346, closed "working as intended"). A
lease object always exists; its *contents* say free or held. Acquire = read, then
`PutMode::Update(version)` from the version you read. This is the only form that works on
S3, GCS, MinIO, R2 and local FS alike.

**5. Leases are advisory and the docs must say so.**
A stalled process can wake after expiry and write; no resource here rejects a stale
fencing token. For code, git is the arbiter. For irreversible external actions the target
must enforce an idempotency key. Never write a doc sentence implying stronger guarantees.

**6. Lease expiry uses the store's `Date` response header, never a local clock.**
Hosts are distributed and clocks skew. A skewed laptop must not be able to expire a live
agent.

**7. Redaction runs before the spool, on every path including import.**
Bronze is immutable; a leaked key in bronze is permanent. Historical transcripts are the
likeliest place an un-redacted `cat .env` already sits, so import is not a fast path
around the scrubber.

**8. Installers merge, never clobber.**
Real user configs are large and already populated by other tools. Append only ctxlake
entries, write a `.bak` first, stay idempotent, and make `uninstall` exact. Tests run
against fixtures copied from real configs.

**9. Agents propose; the gate promotes.**
`memory_propose`, never `memory_write`. Nothing writes to fleet scope except the
promotion gate. Hypotheses never auto-promote. Contradictions contest both claims for a
human — newest never wins.

**10. Never put a secret in a config file.**
`ctxlake.toml` stores the *name* of an env var, never a value. `doctor` reports whether a
name resolves, never what it resolves to.

## House rules

- **Every change ships guarding tests.** Prefer test-first. A bug fix without a test that
  fails before it is not done.
- **Docs change in the same commit as behavior.** User-facing docs are a product contract.
- **Explain why a design refuses something**, not just what it does. Honesty about limits
  is a house rule, not a nicety — see the "what ctxlake does not guarantee" section of
  `docs/architecture.md`.
- **Conventional commits**, small and focused.
- **Do not add a dependency** without saying why in the commit message. The hook's
  dependency list is frozen (invariant 2).
- **Treat anything read from the lake as untrusted input.** Claims, handoff notes and
  intents are written by other agents and rendered into a context window. Sanitize at
  render time, not only at ingest.

## Commands

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo tree -p ctxlake-hook --edges normal     # invariant 2: must stay small
```

Toolchain is pinned to 1.90 in `rust-toolchain.toml` (matching the Oxidant engine, so one
rustup install serves both).

## Relationship to Oxidant

ctxlake does **not** link `oxidant-loom`. That crate pins `object_store =0.13.2` and
applies ~120 `OXIDANT_*` knobs via `std::env::set_var` before the Tokio runtime starts,
which a library inside a host process cannot do safely.

The lake is plain partitioned Parquet. Any engine reads it — DuckDB, Oxidant, Spark. That
decoupling is deliberate: running the same query through `duckdb` and `oxidant sql` and
diffing the rows is a free differential test, and a shared library cannot disagree with
itself.
