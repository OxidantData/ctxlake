# Getting started — from install to first briefing

ctxlake keeps a fleet of coding agents in sync through object storage alone. This page
takes you from nothing to an agent session that opens knowing what the rest of the fleet
is doing.

> **Status: pre-alpha.** Commands and flags will change. Nothing here writes to your
> agents' behavior without you asking for it, and `ctxlake uninstall` is exact.

## What you need

- An object store bucket: S3, GCS, MinIO, Cloudflare R2 — or a local directory to try it
  out. See [storage.md](storage.md).
- At least one of: Claude Code, Cursor Agent CLI, Hermes.
- Credentials your machine already resolves. ctxlake does not manage cloud credentials;
  it uses the standard chain (environment, profile, instance role).

## 1. Install

```sh
brew install oxidantdata/tap/ctxlake
```

Or build from source:

```sh
cargo install --git https://github.com/OxidantData/ctxlake ctxlake-cli
```

## 2. Point it at a store

```sh
ctxlake init --store s3://my-bucket/ctxlake --fleet myteam
```

`--fleet` is the boundary of who sees whom. Everyone sharing a fleet sees each other's
sessions and leases, so it should map to a team that is genuinely collaborating, not to
an entire company.

To try it locally first, with no cloud account at all:

```sh
ctxlake init --store file://~/ctxlake-demo --fleet local
```

Everything works against a local directory — leases, roster, briefings. What you lose is
the ability for a second machine to join, which is the whole point, so use this to
evaluate rather than to run a real fleet.

## 3. Check your backend before going further

```sh
ctxlake doctor
```

This does two things. It **executes** each conditional-write primitive against your
bucket and reports pass or fail, and it detects which agent runtimes are installed.

```text
store   s3://my-bucket/ctxlake   (region us-west-2)
  put-if-absent         ok
  compare-and-swap      ok
  conflict detection    ok
  conditional GET       ok        304 on unchanged
  list / delete         ok

runtimes
  claude-code   found   ~/.claude/settings.json      (13 existing hook entries)
  cursor        found   ~/.cursor/hooks.json         (8 existing hook entries)
  hermes        not found
```

> **This runs the primitives rather than assuming them.** Object stores differ in which
> conditional writes they support — MinIO rejects the put-if-absent wildcard, GCS uses
> generation numbers instead of ETags — and a backend that silently lacks one would
> surface much later as leases that never hold. Run `doctor` before `install`, not after.

## 4. Backfill what you already have

```sh
ctxlake import --all --since 90d
```

Your runtimes have been recording sessions all along. Importing them means the first
briefing you see already knows what has happened in your repos, instead of starting
empty. Fidelity differs per runtime — see [import.md](import.md).

Redaction runs on this path too, so a secret sitting in a six-month-old transcript does
not get written into the lake.

## 5. Wire up your agents

```sh
ctxlake install --all          # every runtime doctor found
ctxlake install claude-code    # or one at a time
```

Installs **merge** into your existing configuration. Existing hooks are preserved, a
`.bak` is written first, and re-running changes nothing. Preview the change with
`--dry-run`, and reverse it exactly with `ctxlake uninstall`.

## 6. Start the daemon

```sh
ctxlake sync
```

This is the piece that actually moves bytes: hook spool → the store (`sessions/`),
store → the local cache your hooks and `ctxlake status` read, and this agent's own
presence heartbeat. Nothing above starts it for you — without a running `ctxlake sync`
somewhere, sessions sit in the local spool and never reach the lake. Run it once per
host, or point a `systemd`/`launchd` unit at `ctxlake sync --foreground` for a host you
want to stay up reliably. Check on it any time with `ctxlake sync --status`, and stop
it with `ctxlake sync --stop`.

## 7. See the fleet

```sh
ctxlake status
```

```text
fleet myteam · 2 agents active · roster 4s old

  cc-01    claude_code  oxidant/Oxidant  kan-112   14m
           holds crates/oxidant-catalog-glue/**
           "migrating the Glue catalog off the CLI shell-out"

  cur-02   cursor       oxidant/Oxidant  main       3m
           no leases
           "writing tests for oxidant-pipelines expectations"
```

Start a new agent session in that repo and it opens with the same information already in
context — who is working, what they hold, and what happened here recently.

## 8. Claim something before you work on it

```sh
ctxlake claim 'crates/oxidant-loom/**' --reason "splitting the S3 cache out"
```

Now other agents see that claim in their briefing, and a pre-edit check warns them before
they touch those paths.

> **Leases are advisory.** They prevent two agents spending twenty minutes on the same
> problem, which is the expensive failure. They are not a lock: git remains the arbiter
> for code, and anything irreversible needs its own idempotency key. See
> [coordination.md](coordination.md) for exactly what is and is not guaranteed.

Release when you are done — or just end the session, which releases everything it held:

```sh
ctxlake release --all
```

## What happens next

The `ctxlake sync` you started in step 6 ships your spooled sessions to the lake and
refreshes the local cache your hooks read — that part needs to actually be running
somewhere, on at least one host, or nothing above step 6 reaches the lake.

Maintenance is different: `ctxlake maint` (compaction, digests, and — once configured —
claim extraction) is **optional**. Point a cron entry or systemd timer at it on every
host, on one host, or on none — whichever host's `ctxlake maint` acquires the
fleet-wide maintenance lease does the work, every other one exits immediately, and if
nobody ever runs it, coordination (leases, roster, `ctxlake status`) keeps working
exactly the same; you just don't get compaction or the belief layer.

By default, every session gets a **Tier 1** handoff — the agent that just did the work
writes its own one-line "here's what I did and what's next" note, no LLM key required
(docs/summarization.md). The belief layer — durable claims extracted *across* sessions
— is a separate, optional Tier 2 that needs an LLM configured; review what it produces
with `ctxlake claims --status candidate --explain`, and reach for
`ctxlake quarantine <agent_id>` if one agent's claims start looking wrong. See
[summarization.md](summarization.md).

## Next steps

- [cli.md](cli.md) — every command and flag, including `ctxlake sync`/`maint`/`claims`
- [adopting.md](adopting.md) — how this fits alongside what you already run
- [concepts.md](concepts.md) — the three planes, and why each exists
- [architecture.md](architecture.md) — every component, for when you need to debug one
- [storage.md](storage.md) — backend configuration and the capability matrix
