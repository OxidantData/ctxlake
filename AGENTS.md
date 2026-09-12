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

**4. Shared mutable state is CAS, never put-if-absent.**
MinIO rejects `If-None-Match: *` (minio/minio#20346, closed "working as intended"), so
anything that must be *updated* by more than one writer — the roster, the snapshot
pointer — reads the object and then `PutMode::Update(version)` from the version it read.
`PutMode::Create` is correct only where "someone got here first" is the answer you
wanted: idempotency markers, which have no holder and nothing to steal.

**5. There is no lease, and nothing may reintroduce one.**
Every unit of batch work is idempotent *by content*: compaction writes to a directory
named for the hash of its inputs, digests are claimed per session with a create-if-absent
marker, the snapshot is content-addressed and published by a pointer swap. That is a
stronger property than mutual exclusion and it needs no coordination, so two hosts
running `ctxlake maint` at the same minute is fine and supported.
`crates/ctxlake-store/tests/no_lease_regression.rs` fails the build if a `lease` module,
a `live/leases/` key, or the surrounding vocabulary comes back. If you think you need a
lock, you have found work that is not idempotent — fix that instead.

**6. Expiry uses the store's `Date` response header, never a local clock.**
Hosts are distributed and clocks skew. A skewed laptop must not be able to decide that a
live agent's presence entry has aged out.

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

## Hard-won facts

Each of these cost real effort to establish and several contradict what the documentation
of the system in question implies. Do not re-derive them, and do not "correct" them back.

**`object_store` 0.14's `LocalFileSystem` has no `PutMode::Update`.** It returns
`NotImplemented` (`src/local.rs:399`). `ctxlake_store::local_cas::CasLocalFileSystem`
wraps it to provide real CAS via `std::fs::File::lock`; `backend::build` uses it for
`file://` automatically. Without it the CAS suite — the test that proves this design —
would have nowhere to run by default.

**MinIO is not on Docker Hub any more.** `minio/minio` returns 401 with zero listed tags
and `bitnami/minio:latest` 404s. CI pulls from `quay.io/minio/minio`. It also cannot be a
service container, because the image needs `server /data` as its command and GitHub service
containers cannot set one.

**Cursor double-encodes `tool_output`.** It is a JSON *string* containing
`{"output":…, "exitCode":N}`. Its `cwd` key exists but arrives empty — the real path is
`workspace_roots[0]`. `duration` is a float, so `as_u64()` silently yields nothing. And
every payload carries `user_email`, which must never reach the envelope. See
`crates/ctxlake-hook/tests/fixtures/cursor-verified/README.md`.

**Hermes supports Claude Code-compatible shell hooks.** Its stdin is
`{hook_event_name, tool_name, tool_input, session_id, cwd, extra}` and `exit 2` blocks, so
one binary serves all three runtimes. But `result` and `duration_ms` are nested under
`extra`, not top-level. Verified against Hermes's own source; see
[`docs/runtimes/hermes.md`](docs/runtimes/hermes.md).

**Claude Code sends `prompt`, not the documented `user_input`.** The published hooks
reference says `user_input`; the binary sends `prompt`. Reading only the documented name
captured no prompt text at all — every prompt event hashed the empty string while the
hook fired, events reached the spool, and nothing reported a problem. `SessionStart`
likewise sends `source`, not `startup_reason`. See
`crates/ctxlake-hook/tests/fixtures/claude-code-verified/`.

**The spool is partitioned by runtime, not fleet/agent.** A hook knows its runtime for
certain; `fleet_id`/`agent_id` come from the environment and may be unset, and a path built
from an unset value cannot be found later.

## How these were found, and the lesson

Every bug in the list above **failed silently**. No panic, no error — just a field quietly
missing from every event of one runtime. Two rules follow, and they are not optional:

**A fixture written from the same source as the implementation agrees with it by
construction.** This has now happened on all three runtimes. The Cursor `duration` bug
survived review because its hand-written fixture used the integer `42` while the real
payload sends `1089.021` — the test passed and agreed with the bug. Claude Code's prompt
text was never captured at all, because both the adapter and its fixtures were written
from a reference that names a field the binary does not send.

Neither was findable from inside the repo. Every test passed. **Where a third party's wire
format is involved, run the real thing and look at what arrives.** Capturing a payload
costs minutes; these cost a working feature each, silently, for as long as nobody looked.

**Verify that a regression test bites.** Break the code, watch it go red, restore, watch it
pass. And if your editing tool reformats the file, confirm the mutation actually applied —
a silently-missed mutation makes a useless test look verified. That has happened here more
than once.

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

## The docs standard

User docs and contributor rationale are different documents. Ours conflated them: 46% of
`docs/` was dense prose, `cli.md` ran 576 lines for thirteen commands, and nearly every
decision explained itself in place. A reader looking for a flag had to read an argument.

**User docs answer "what do I do".** Contributor docs answer "why is it like this". When
you want to write down why a design refuses something, it goes in a code comment or here
— not on a page someone reads to get started.

Concretely, for anything under `docs/`:

| Page kind | Shape | Ceiling |
|---|---|---|
| Landing | What it is in two sentences, install, one example | ~40 lines |
| Quickstart | The happy path only. No alternatives, no rationale | ~80 lines |
| Guide | One task, start to finish | ~120 lines |
| Reference (CLI, config) | Tables. One row per flag, one example per command | as needed, but tabular |
| Concepts | Only what a user must hold in their head to use it correctly | ~100 lines |

Rules that follow from that:

- **A table beats a paragraph.** If you are describing a set of things — flags, events,
  fields, backends — it is a table.
- **One example beats three sentences about the example.**
- **Cut every "we chose X because Y" from user pages.** The user did not choose.
- **Do not narrate the implementation's history.** What a review found, which bug a
  design avoids, what an earlier version did — none of it belongs on a user page.
- **Keep honest limits, shortened.** "Advisory", "not verified", "needs a live install to
  confirm" stay. Trimming rationale is the goal; trimming an operational fact a user
  would otherwise hit by surprise is a regression, and `docs/checks/regression_checks.py`
  fails on some of these deliberately.

The test: open a page and ask what a reader is trying to do. If the next paragraph does
not help them do it, it belongs somewhere else or nowhere.

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
