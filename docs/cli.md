# cli — the `ctxlake` binary

Every command below is real: this page describes `crates/ctxlake-cli`, not a plan for
it.

```text
ctxlake init        point at a store, write ctxlake.toml
ctxlake doctor      the trust-building command — run this before install
ctxlake install     merge hooks into a runtime, never clobber
ctxlake uninstall   remove exactly what install added
ctxlake status      who's active, on what
ctxlake config      print the resolved config
ctxlake sync        run (or check, or stop) the daemon
ctxlake maint       run the maintenance chain — safe from any number of hosts at once
ctxlake claims      review candidate / contested / promoted claims
ctxlake quarantine  stop one agent's claims from promoting
```

Every subcommand accepts `--config <path>` to point at a `ctxlake.toml` somewhere other
than the default (`$XDG_CONFIG_HOME/ctxlake/ctxlake.toml`, or
`~/.config/ctxlake/ctxlake.toml` if `XDG_CONFIG_HOME` is unset) — useful for running
more than one agent identity from one host.

## `ctxlake init`

```sh
ctxlake init --store <url> --fleet <id> [--agent-id <id>] [--force]
```

1. **Verifies the store is reachable** — a real `put`/`get` round trip, not just a URL
   parse.
2. **Writes `ctxlake.toml`** — see [config.md](config.md) for the full shape.

```sh
$ ctxlake init --store s3://my-bucket/ctxlake --fleet myteam
wrote /Users/you/.config/ctxlake/ctxlake.toml
  store:     s3://my-bucket/ctxlake
  fleet_id:  myteam
  agent_id:  your-hostname

Next: ctxlake doctor
```

`--agent-id` defaults to a sanitized hostname if omitted, stable across re-running
`init` on the same host. `--force` is required to overwrite an existing config —
without it, `init` refuses and changes nothing.

To try ctxlake with no cloud account at all:

```sh
ctxlake init --store file:///tmp/ctxlake-demo --fleet local
```

Roster and status work unchanged against a local directory. What you lose is a second
machine's ability to join — see [storage.md](storage.md).

## `ctxlake doctor`

The trust-building command — run before you let this tool touch a real config.

```text
$ ctxlake doctor
store   s3://my-bucket/ctxlake
  put-if-absent                ok     a second Create was correctly rejected
  compare-and-swap             ok     Update succeeded against the version we just wrote
  conflict detection           ok     a stale version was correctly rejected
  conditional GET               ok    If-None-Match correctly reported unchanged
  list                          ok    saw both scratch objects among 2 listed
  delete                        ok    object was gone immediately after delete

runtimes
  claude-code  wired      (2 other entries)   /Users/you/.claude/settings.json
  cursor       found, not wired               /Users/you/.cursor/hooks.json
  hermes       not found

daemon
  no cache at /Users/you/.ctxlake/cache/myteam/roster.json yet — daemon not running, or hasn't completed a first refresh
  spool backlog: 0 file(s), 0 bytes — /Users/you/.ctxlake/spool
  process: not running — `ctxlake sync` starts it

maintenance
  never — ctxlake-maint has not published a snapshot yet

summarization
  mode: agent
  tier 1 nudges fired: 0 session(s)
```

Every row above is a real request against your actual bucket
([`ctxlake_store::probe`](../crates/ctxlake-store/src/probe.rs)), via a scratch key
cleaned up whether the probe passes or fails — backends genuinely differ (see
[storage.md](storage.md)'s capability matrix):

| Row | If it fails |
|---|---|
| `put-if-absent` | **Expected to fail on MinIO** ([minio/minio#20346](https://github.com/minio/minio/issues/20346)) and changes nothing — roster heartbeats are CAS-only by design and never depend on it. |
| `compare-and-swap` / `conflict detection` | The roster fan-in cannot work correctly on this backend. Capture (writing sessions) is unaffected. |
| `conditional GET` | Roster polling costs a full `GET` every cycle instead of a cheap 304 — works, scales worse ([scaling.md](scaling.md)). |
| `list` | The roster fan-in and `ctxlake maint` can't do their jobs. |

`doctor` also reports each detected runtime (**not found** / **found, not wired** /
**wired**, with a count of hook entries belonging to other tools — informational, never
a warning), whether an `api_key_env`'s named variable resolves (never its value, per
AGENTS.md invariant 10) when `[summarize]` needs one, the daemon's process and spool
state, how long ago `snapshot/latest.json` was last published (from the store's own
`last_modified`, never this host's clock), and Tier 1's fired-nudge count.

**Exit code:** non-zero only when the store cannot be written to and read from at all.
Every other gap — no put-if-absent, no CAS, no conditional GET — degrades
*coordination*, not capture, and is loud in the printed report without flipping the
exit code.

## `ctxlake install` / `ctxlake uninstall`

```sh
ctxlake install claude-code [--dry-run]
ctxlake install cursor
ctxlake install hermes
ctxlake install --all              # every runtime doctor would report as "found"
ctxlake uninstall <runtime>|--all
```

This is the one place ctxlake writes into a file it did not create, so every guarantee
below is load-bearing and asserted by the round-trip test suite in
`crates/ctxlake-cli/src/hooks/` against fixtures shaped like real, already-populated
configs:

- **Every pre-existing entry survives** — another tool's matcher group, another
  plugin's command, an unrelated top-level key. None of it is touched.
- **A `.bak` is written before any change.** A no-op run doesn't touch it.
- **Re-running `install` changes nothing**, byte for byte. If `fleet_id` or `agent_id`
  changes in `ctxlake.toml` between runs, the next run replaces ctxlake's own stale
  entry rather than leaving two behind.
- **`uninstall` removes exactly what `install` added** — detected by a stable marker
  (`ctxlake-hook` as a whitespace-delimited token, followed two tokens later by
  `claude_code`/`cursor`/`hermes`), not a bare substring match, so a user's own wrapper
  around the line survives while a foreign tool that merely *mentions* `ctxlake-hook`
  is left alone.
- **A malformed existing file is refused, not guessed at** — no `.bak`, no partial
  merge, if `~/.claude/settings.json` isn't valid JSON or `~/.hermes/config.yaml`
  isn't valid YAML.
- **`CTXLAKE_FLEET_ID` / `CTXLAKE_AGENT_ID` reach the hook** as a shell-style prefix on
  the command line (`env CTXLAKE_FLEET_ID=myteam ... ctxlake-hook PostToolUse
  claude_code`), since none of the three hook schemas has an `env` key. Without it,
  events land attributed to `unconfigured-fleet`/`unconfigured-agent`.

`--dry-run` prints a line-level diff without writing anything. `--all` targets every
runtime whose config file `doctor` reports as **found** — a runtime with no config
file at all is left untouched.

### What gets installed, per runtime

| Runtime | File | Shape | Events |
|---|---|---|---|
| Claude Code | `~/.claude/settings.json` | `hooks.<event>` is an array of matcher groups; ctxlake appends its own with no `matcher` | `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PreCompact`, `Stop`, `SubagentStop`, `SessionEnd` |
| Cursor | `~/.cursor/hooks.json` | `hooks.<event>` is a flat array of `{command}` | `beforeSubmitPrompt`, `beforeShellExecution`, `beforeReadFile`, `beforeMCPExecution`, `afterFileEdit`, `stop` |
| Hermes | `~/.hermes/config.yaml` | `hooks.<event>` is a flat list of `{command}` — see [runtimes/hermes.md](runtimes/hermes.md) | `on_session_start`, `pre_llm_call`, `pre_tool_call`, `post_tool_call`, `post_llm_call`, `on_session_end` |

Hermes' `pre_tool_call` also gets `timeout: 5` and `fail_closed: false` — the sole
event where `fail_closed` does anything, left `false` because a collision check is
advisory and a hook crash must never wedge a session.

Each event name is passed as `ctxlake-hook`'s first argument
(`ctxlake-hook <event> <runtime>`), matching `crates/ctxlake-hook/src/main.rs`'s
contract.

> **Hermes goes through the same binary.** `ctxlake-hook` normalizes Hermes payloads
> via `crates/ctxlake-hook/src/adapters/hermes.rs` — there is no separate Python
> plugin, so there is no second redactor to keep in sync. See
> [runtimes/hermes.md](runtimes/hermes.md).

## `ctxlake status`

```sh
ctxlake status
```

```text
fleet myteam · 2 agent(s) active · roster 4s old (/Users/you/.ctxlake/cache/myteam/roster.json)

  cc-01  claude_code  github.com/OxidantData/ctxlake  14m
           touching crates/oxidant-catalog-glue/src/lib.rs
           "migrating the Glue catalog off the CLI shell-out"
```

The roster is read from the local cache — the same file `ctxlake sync` refreshes for
the hook and MCP server to read (invariant 1: no network on that path). `status` prints
the cache's age and says so explicitly once it's over a minute old. If the cache has
never been written, `status` says exactly that rather than printing an empty roster
that looks like "nobody's here."

## `ctxlake config`

```sh
ctxlake config
```

Prints the resolved `ctxlake.toml`. There is nothing to mask by replacing a value with
`***`: this struct can never hold a secret in the first place, only the *name* of an
environment variable — see [config.md](config.md).

## `ctxlake sync`

```sh
ctxlake sync                          # daemonize (the default)
ctxlake sync --foreground             # run attached, e.g. under systemd/launchd
ctxlake sync --status
ctxlake sync --stop
```

Runs `ctxlake-sync`'s three loops under one process: hook spool → store
(`sessions/`), store → local cache (`roster.json`, `snapshot.bin`), and this agent's
own presence heartbeat. Nothing else starts it for you — without a running
`ctxlake sync` somewhere, sessions sit in the local spool and never reach the lake.
Run it once per host, or point a `systemd`/`launchd` unit at
`ctxlake sync --foreground` for a host you want to stay up reliably.

The bare form daemonizes (detached child, its own process group, stdio to
`~/.ctxlake/run/<fleet_id>.log`); `--foreground` does not. `--status`/`--stop` check
liveness against the pidfile with `kill -0` rather than trusting a stale file.
`--stop` sends `SIGTERM`, waits up to 5 seconds, and reports what happened.

> There is no `setsid` — a bare `ctxlake sync` can still receive a `SIGHUP` if its
> controlling terminal's session ends. For a host you actually care about staying up,
> run `ctxlake sync --foreground` under a real service manager instead.

## `ctxlake maint`

```sh
ctxlake maint             # loop forever, one cycle every 5 minutes
ctxlake maint --once      # run one cycle and exit — what cron/systemd should call
```

Compacts small Parquet files, runs the claims promotion gate, and publishes
`snapshot/`. **Safe to schedule on every host in the fleet, or none at all** — there is
no lock to acquire and no primary host to designate, because every step is idempotent
by content: compaction's output directory is named by a hash of its input session set,
extraction claims each session with a create-if-absent marker, and the snapshot
publish is content-addressed. See [coordination.md](coordination.md) for why. If two
runs happen to overlap, the cost is redundant work, never corrupted output. If nobody
ever runs it, capture and coordination keep working exactly as before — the lake just
stays as fresh as the last pass.

## `ctxlake claims`

```sh
ctxlake claims --status candidate --explain   # which gate rejected what, and why
ctxlake claims --status contested             # the human review queue
ctxlake claims --status promoted
```

`--status candidate` lists every proposed claim under `claims/events/`, grouped by
claim type and text, with every observing agent listed. `--explain` names which of
[memory.md](memory.md)'s four gates would reject each candidate today, and why:

```text
$ ctxlake claims --status candidate --explain
[hypothesis] the flake is a colima scheduling artifact
    observed by: cc-01
    -> rejected by the evidence gate: hypotheses never auto-promote beyond agent scope
```

This is a **read-only, best-effort evaluator**, not a second implementation of the
gate — the contradiction and independence checks need data this evaluator doesn't
carry, and it says so rather than guessing. `--status promoted`/`--status contested`
read the local fleet cache mirror, the same file `memory_search` reads.

## `ctxlake quarantine`

```sh
ctxlake quarantine <agent_id>
```

The kill switch: its claims stop promoting (`claims/quarantine/<agent_id>.json`), its
already-promoted claims move to `contested`, and **capture continues unaffected** — you
want the record of the failure, not a gap where it used to be.

```text
$ ctxlake quarantine cc-99
quarantined cc-99: its candidates stop promoting; 1 previously promoted claim(s) moved to contested in the local cache
capture is unaffected — cc-99's sessions and claim proposals keep landing in the lake
```

Reversible: there is no `ctxlake unquarantine` yet, but the marker is a plain object at
a known key.

## Next steps

- [config.md](config.md) — the full `ctxlake.toml` reference
- [getting-started.md](getting-started.md) — the same commands, in the order a first
  run actually uses them
- [coordination.md](coordination.md) — roster and intents, and what they promise
- [storage.md](storage.md) — the capability matrix `doctor` executes
