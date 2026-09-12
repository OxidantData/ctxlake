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

## 6. See the fleet

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

## 7. Claim something before you work on it

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

Nothing you have to run. A background daemon ships your spooled sessions to the lake and
refreshes the local cache your hooks read. Compaction and digests run opportunistically
on whichever machine picks up the maintenance lease; if no agent is running, nothing
happens and nothing breaks.

The belief layer — durable claims extracted across sessions — is off until you configure
it. See [summarization.md](summarization.md).

## Next steps

- [adopting.md](adopting.md) — how this fits alongside what you already run
- [concepts.md](concepts.md) — the three planes, and why each exists
- [architecture.md](architecture.md) — every component, for when you need to debug one
- [storage.md](storage.md) — backend configuration and the capability matrix
