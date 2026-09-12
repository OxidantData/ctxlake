# Changelog

## v0.1.2

Tier 2 actually works now, the daemon runs maintenance itself, and `ctxlake sync
install` refuses a host that cannot do the job.

### Tier 2 runs end to end

`ctxlake maint` ran compact → digest → publish and stopped. `extract::run` and
`gate::run` were complete, tested, and called by nothing, so configuring a model
produced no claims and an empty `ctxlake claims` list was indistinguishable from
"extraction found nothing worth claiming". The chain is now
compact → digest → extract → gate → snapshot.

Two properties that are easy to get wrong, each pinned by a test: the gate runs
**even with no model configured** (claims also arrive from the `memory_propose`
MCP tool), and it runs **before** the snapshot publish (reversing them leaves a
just-promoted claim showing as `candidate` for a whole cycle).

**New providers: OpenRouter and Gemini**, alongside Anthropic, OpenAI-compatible
and Ollama. `provider = "openrouter" | "gemini"` in `[summarize.batch]`.

Four bugs that each made extraction silently produce nothing, all found within
minutes of the first live call against a real model and none of them visible to
the test suite:

- The transcript sent to the model contained **no tool calls** — on a real session
  `content` holds the user's prompt and nothing else, so the model was shown the
  question and never the work.
- The transcript **never named the session**, while the schema demanded a
  `session_id` per citation. The model supplied the only id-shaped string it could
  see and every claim was dropped as unresolvable.
- The OpenAI-compatible path sent **no `response_format`**, so the model wrapped
  its JSON in a ```json fence and the parser rejected it.
- A transient provider failure **discarded the session permanently**: the
  extraction marker was written before the call and never released on failure.

### Maintenance moved into the daemon

Getting started used to tell you to point cron or a systemd timer at `ctxlake
maint` — on a machine where `ctxlake sync install` had just set up a service.
The daemon now runs the chain itself every 5 minutes. Cron is gone from the docs.

A failed cycle logs and retries; a `[summarize.batch]` whose key cannot be
resolved is caught before the daemon starts, so it exits 78 and the unit declines
to restart it.

### `ctxlake sync install` checks the host first

It refuses, and writes no unit, when the store is unreachable, a CAS probe fails,
a configured model's key is unset, or the key resolves but the provider rejects a
live call. Missing runtime hooks are a warning, not a blocker. `--skip-checks`
overrides.

**`ctxlake doctor` now calls the provider for real.** A revoked key, a typo'd
model name and an account over quota all resolve an environment variable
perfectly and then fail hours later inside a maintenance log.

### AWS credentials

ctxlake now reads `~/.aws/credentials`, following the AWS CLI's own order:
environment, then the credentials file (`AWS_PROFILE`, else `default`), then an
instance or container role. Previously it read only the environment and fell
through to EC2 instance metadata, so a laptop with a working `aws` CLI waited ~16
seconds for a confusing timeout — and a **supervised daemon**, which has no shell,
would have failed on every cycle.

`doctor` now prints which identity it resolved and tells a `403` apart from a
network error. SSO, `credential_process` and assume-role profiles are still
declined rather than half-read, since they expire under a long-running daemon, but
`doctor` names the mechanism.

### Also

- A cross-fleet leak: `extract::list_sealed_sessions` had no fleet filter, so in a
  store holding two fleets one team's transcripts became evidence for another
  team's claims.
- `ctxlake maint` prints a readable summary instead of a `Debug` dump.

### Verified

Against a real AWS S3 bucket and a real model, not fixtures: all six CAS probes
pass on S3; a session captured through the real hook is sealed, digested,
compacted and published; and the same session yields 4 extracted claims, each
citing real evidence, 4 promoted by the gate and correctly withheld from agents
under `mode = "shadow"`.

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
