# Getting started

From nothing to an agent session that opens knowing what the rest of the fleet is doing.

> **Status: pre-alpha.** Commands and flags will change. Nothing here changes your
> agents' behaviour without you asking, and `ctxlake uninstall` is exact.

You need a bucket (S3, GCS, MinIO, R2 — or a local directory to try it), at least one of
Claude Code, Cursor Agent CLI or Hermes, and credentials your machine already resolves via
the standard chain. ctxlake manages no cloud credentials of its own.

## 1. Install

> **No release has been cut yet.** `cargo install --git` is the only path that works
> today; the rest describes what the release workflow will produce once a tag is pushed.

```sh
cargo install --git https://github.com/OxidantData/ctxlake ctxlake-cli
cargo install --git https://github.com/OxidantData/ctxlake ctxlake-hook

# once released:
brew install oxidantdata/tap/ctxlake
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/OxidantData/ctxlake/main/packaging/install.sh | sh
```

Prebuilt `.tar.xz` archives for `{aarch64,x86_64}-apple-darwin` and
`{x86_64,aarch64}-unknown-linux-gnu` land on the
[Releases page](https://github.com/OxidantData/ctxlake/releases).

## 2. Point it at a store

```sh
ctxlake init --store s3://my-bucket/ctxlake --fleet myteam
ctxlake init --store file://~/ctxlake-demo --fleet local   # no cloud account needed
```

`--fleet` is the boundary of who sees whom. Map it to a team genuinely collaborating,
not to a company. The `file://` form runs roster and briefings fine; what you lose is a
second machine joining, so use it to evaluate, not to run a fleet.

## 3. Check your backend — before you install

```sh
ctxlake doctor
```

```text
store   s3://my-bucket/ctxlake   (region us-west-2)
  put-if-absent / compare-and-swap / conflict detection / conditional GET / list   ok
runtimes
  claude-code   found   ~/.claude/settings.json   (13 existing hook entries)
  cursor        found   ~/.cursor/hooks.json      (8 existing hook entries)
  hermes        not found
```

`doctor` executes each conditional-write primitive against your real bucket. Backends
differ — MinIO rejects put-if-absent, GCS uses generation numbers instead of ETags — so
run it first, not after. See [storage.md](storage.md).

## 4. Backfill what you already have

```sh
ctxlake import --all --since 90d
```

Your runtimes have been recording all along, so the first briefing already knows your
repos. Fidelity differs per runtime and redaction runs here too — see
[adding-it.md](adding-it.md).

## 5. Wire up your agents

```sh
ctxlake install --all          # every runtime doctor found
ctxlake install claude-code    # or one at a time
```

Installs **merge**: existing hooks are preserved, a `.bak` is written first, and
re-running changes nothing. Preview with `--dry-run`; reverse with `ctxlake uninstall`.

## 6. Start the daemon

```sh
ctxlake sync
```

The piece that moves bytes: spool → store, store → the local cache your hooks read, and
this agent's heartbeat. **Nothing above starts it for you.** Run it once per host, or
point a `systemd`/`launchd` unit at `ctxlake sync --foreground`.

## 7. See the fleet

```sh
ctxlake status
```

```text
fleet myteam · 2 agents active · roster 4s old

  cc-01    claude_code  oxidant/Oxidant  kan-112   14m
           "migrating the Glue catalog off the CLI shell-out"
  cur-02   cursor       oxidant/Oxidant  main       3m
           "writing tests for oxidant-pipelines expectations"
```

Start a session in that repo and it opens with the same information in context.

## 8. Schedule maintenance

`ctxlake maint --once` does every batch job there is — compaction, Tier 0 digests,
snapshot building, and (once you configure a model) claim extraction and the promotion
gates. **Nothing runs it for you**, and nothing needs to coordinate it: point a cron entry
or systemd timer at it on one host or all of them. If nobody ever runs it, capture keeps
working; the lake just stays as fresh as the last pass.

> **Tier 1 is not wired yet.** No hook fires the turn-end nudge today, so
> `ctxlake doctor` reporting `tier 1 nudges fired: 0` is expected, not a fault in your
> install. Tier 0 digests and Tier 2 extraction both work.

Nothing so far needs a model. Durable *memories* are the one part that does, and they stay
invisible to agents until you deliberately turn them on — see [memory.md](memory.md).

## Next steps

- [how-it-works.md](how-it-works.md) — the three planes, in one picture
- [reference.md](reference.md) — every command, flag and config key
- [adding-it.md](adding-it.md) — how this fits alongside what you already run
