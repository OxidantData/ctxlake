# Getting started — from install to first briefing

ctxlake keeps a fleet of coding agents in sync through object storage alone. This page
takes you from nothing to an agent session that opens knowing what the rest of the
fleet is doing.

> **Status: pre-alpha.** Commands and flags will change. Nothing here writes to your
> agents' behavior without you asking for it, and `ctxlake uninstall` is exact.

## What you need

- An object store bucket: S3, GCS, MinIO, Cloudflare R2 — or a local directory to try
  it out. See [storage.md](storage.md).
- At least one of: Claude Code, Cursor Agent CLI, Hermes.
- Credentials your machine already resolves — ctxlake does not manage cloud
  credentials; it uses the standard chain (environment, profile, instance role).

## 1. Install

All four ways install the same two binaries (`ctxlake`, `ctxlake-hook`).

> **Pre-alpha: no release has been cut yet.** `cargo install --git` is the only path
> that works today; the rest describes what the release workflow will produce once a
> tag is pushed.

```sh
# Homebrew (macOS and Linux)
brew install oxidantdata/tap/ctxlake

# curl | sh — verifies the downloaded archive against the release's SHA256SUMS
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/OxidantData/ctxlake/main/packaging/install.sh | sh

# cargo install — builds from source, works today
cargo install --git https://github.com/OxidantData/ctxlake ctxlake-cli
cargo install --git https://github.com/OxidantData/ctxlake ctxlake-hook
```

Prebuilt `.tar.xz` archives are also on the
[Releases page](https://github.com/OxidantData/ctxlake/releases) for
`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, and
`aarch64-unknown-linux-gnu` — reach for one directly to pin an exact version or install
somewhere the script's assumptions don't fit.

## 2. Point it at a store

```sh
ctxlake init --store s3://my-bucket/ctxlake --fleet myteam
```

`--fleet` is the boundary of who sees whom. Everyone sharing a fleet sees each other's
sessions and leases, so it should map to a team genuinely collaborating, not to an
entire company.

To try it locally first, with no cloud account at all:

```sh
ctxlake init --store file://~/ctxlake-demo --fleet local
```

Everything works against a local directory — leases, roster, briefings. What you lose
is a second machine joining — use this to evaluate, not to run a real fleet.

## 3. Check your backend

```sh
ctxlake doctor
```

Executes each conditional-write primitive against your bucket and detects which agent
runtimes are installed:

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

Object stores differ in which conditional writes they support — MinIO rejects the
put-if-absent wildcard, GCS uses generation numbers instead of ETags — so run `doctor`
before `install`, not after.

## 4. Backfill what you already have

```sh
ctxlake import --all --since 90d
```

Your runtimes have been recording sessions all along; importing them means the first
briefing you see already knows what has happened in your repos. Fidelity differs per
runtime — see [import.md](import.md). Redaction runs on this path too.

## 5. Wire up your agents

```sh
ctxlake install --all          # every runtime doctor found
ctxlake install claude-code    # or one at a time
```

Installs **merge** into your existing configuration — existing hooks are preserved, a
`.bak` is written first, and re-running changes nothing. Preview with `--dry-run`;
reverse exactly with `ctxlake uninstall`.

## 6. Start the daemon

```sh
ctxlake sync
```

This is the piece that actually moves bytes: hook spool → the store, store → the local
cache your hooks and `ctxlake status` read, and this agent's own presence heartbeat.
Nothing above starts it for you. Run it once per host, or point a `systemd`/`launchd`
unit at `ctxlake sync --foreground` for a host you want to stay up reliably.

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

Start a new agent session in that repo and it opens with the same information already
in context — who is working, what they hold, and what happened here recently.

## 8. Claim something before you work on it

```sh
ctxlake claim 'crates/oxidant-loom/**' --reason "splitting the S3 cache out"
```

Now other agents see that claim in their briefing, and a pre-edit check warns them
before they touch those paths.

> **Leases are advisory.** They prevent two agents spending twenty minutes on the same
> problem, which is the expensive failure. They are not a lock: git remains the
> arbiter for code, and anything irreversible needs its own idempotency key. See
> [coordination.md](coordination.md) for exactly what is and is not guaranteed.

Release when you are done — or just end the session, which releases everything it
held:

```sh
ctxlake release --all
```

## What happens next

The `ctxlake sync` you started in step 6 ships your spooled sessions to the lake and
refreshes the local cache your hooks read — that part needs to actually be running
somewhere, on at least one host, or nothing above step 6 reaches the lake.

Maintenance is different. `ctxlake maint` performs every batch job there is —
compaction, Tier 0 digests, snapshot building, and (once you configure a model) claim
extraction and the promotion gates. **Nothing runs it for you.** It takes the
fleet-wide maintenance lease, does one pass, and exits.

```sh
ctxlake maint --once
```

Point a cron entry or systemd timer at it, on one host or on all of them — whichever
wins the lease does the work and the rest exit 0 immediately. Every step it runs is
also idempotent by content ([coordination.md](coordination.md)), so even a genuinely
concurrent run costs redundant work, not corrupted output. If nobody ever runs it,
capture and coordination keep working exactly as before; the lake just stays as fresh
as the last pass.

### Memories, when you want them

Nothing above needs a model. Session history, briefings, digests, and friction signals
are derived arithmetically. Durable *memories* — claims extracted across sessions and
shared with the fleet — are the one part that does, and they stay invisible to agents
until you deliberately turn them on. See [summarization.md](summarization.md) for the
full walkthrough: run maintenance on a timer, point it at a model, read what it
extracts with `ctxlake claims --status candidate --explain`, and only then let agents
see it.

> **Tier 1 is not wired yet.** The design has the agent that just did the work write
> its own handoff note at turn end, no key needed — but no hook fires the nudge today,
> so `ctxlake doctor` reporting `tier 1 nudges fired: 0` is expected, not a fault in
> your install. Tier 0 digests and Tier 2 extraction both work.

## Next steps

- [cli.md](cli.md) — every command and flag, including `ctxlake sync`/`maint`/`claims`
- [adopting.md](adopting.md) — how this fits alongside what you already run
- [concepts.md](concepts.md) — the three planes, and why each exists
- [architecture.md](architecture.md) — every component, for when you need to debug one
- [storage.md](storage.md) — backend configuration and the capability matrix
