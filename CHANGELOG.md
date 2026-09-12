# Changelog

## v0.1.1

Three things: the lease concept is gone, the sync daemon can survive a reboot, and the
docs are half the size.

### Breaking

- **`ctxlake claim` and `ctxlake release` are removed**, along with the `fleet_claim` and
  `fleet_release` MCP tools, the `held_leases` field on `fleet_status`, and the
  `collision_policy` config key. There is no advisory-lock surface any more.
- **`ctxlake sync`'s flags became subcommands.** `--foreground` is now `sync run
  --foreground`; `--status` and `--stop` are `sync status` and `sync stop`. Bare
  `ctxlake sync` still daemonizes, as before.

### The lease is gone, internals included

Every unit of batch work was already idempotent *by content* — compaction writes to a
directory named for the hash of its inputs, digests are claimed per session with a
create-if-absent marker, the snapshot is content-addressed and published by a pointer
swap. That is a stronger property than mutual exclusion and it needs no coordination, so
the lease was belt-and-braces on top of work that was already safe, costing every user a
whole concept to learn: holders, TTLs, expiry, clock skew, stealing.

Two `ctxlake maint --once` runs launched at the same moment against one store now both
exit 0, compute an identical snapshot hash, and one publishes while the other reports
"already current". `crates/ctxlake-store/tests/no_lease_regression.rs` fails the build if
the abstraction comes back.

### `ctxlake sync` survives a reboot

The daemon used to re-exec itself, which worked until the machine restarted — and because
the hook only ever writes to the local spool, capture kept working and the only symptom
was a briefing that quietly stopped updating.

```sh
ctxlake sync install        # systemd user unit (Linux) / LaunchAgent (macOS)
ctxlake sync start | stop | restart | status
ctxlake sync delete
ctxlake init --daemon       # init, install and start in one step
```

`start`/`stop`/`restart`/`status` mean the same thing whether or not a service is
installed. On Linux, `ctxlake sync install` and `ctxlake sync status` both check
`loginctl enable-linger` and say so — without lingering, systemd does not bring the
daemon back after a reboot.

v0.1.0 shipped unit templates invoking `ctxlake sync run --foreground`, a subcommand that
did not exist; anyone who used them got a unit that crash-looped on a usage error. Fixed,
and pinned by a test that parses the exact command line the templates contain.

`ctxlake` now exits **78** (`EX_CONFIG`) when `ctxlake.toml` is missing or unusable, so
`RestartPreventExitStatus=78` stops systemd retrying something no restart can fix.
Everything else still exits 1.

### Docs

Twenty pages became nine plus an index; 3,823 lines became 1,857, a little over half.
Rationale explaining rejected alternatives moved out of the user-facing pages, so what is
left is tables, diagrams and commands. Three hand-authored SVG diagrams replace prose
walls, and mermaid diagrams render instead of shipping their source as plain text.

Every `docs/<page>.md` pointer in the codebase was repointed — 104 of them across 37
files, including two in `ctxlake --help` and the `Documentation=` line in the systemd
unit.

### Also

- `ctxlake import --runtime hermes` reads `~/.hermes/state.db` — full replay, not
  live-capture-only as v0.1.0's docs claimed. It opens the running agent's database
  read-only and immutable, and recovers compaction markers live capture cannot see.
- `ctxlake init` creates the directory for a `file://` store instead of failing on the
  first command in the docs.
- The `curl | sh` installer no longer depends on GNU tar's member-matching rules, which
  made it fail on Linux only.

### Known gap

`ctxlake maint` runs compaction, Tier 0 digests and the snapshot publish. **Tier 2
extraction and the four promotion gates are implemented and tested but not called by the
chain**, so configuring `[summarize.batch]` currently produces no claims. Tiers 0 and 1 —
everything the briefing's history and handoff blocks are built from — work fully. See
[memory.md](https://ctxlake.oxidantdata.com/memory.html#turning-on-tier-2).

## v0.1.0

First release.
