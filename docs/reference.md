# Reference — commands, configuration, MCP tools

Everything here is real: this describes the shipped binaries, not a plan for them. Two
exceptions are flagged inline — `ctxlake install hermes` wires hooks but not MCP.

## Commands

| Command | What it does |
|---|---|
| `ctxlake update` | Update to the latest release, however this copy was installed |
| `ctxlake init` | Point at a store, write `ctxlake.toml` |
| `ctxlake doctor` | Execute every store primitive against your bucket; detect runtimes |
| `ctxlake import` | Backfill history already on disk |
| `ctxlake install` | Merge hooks into a runtime, never clobber |
| `ctxlake uninstall` | Remove exactly what `install` added |
| `ctxlake status` | Who is active, on what |
| `ctxlake config` | Print the resolved config |
| `ctxlake sync` | Run the daemon, or install it as a service so it survives a reboot |
| `ctxlake maint` | Run the maintenance chain — safe from any number of hosts at once |
| `ctxlake claims` | Review candidate / contested / promoted claims |
| `ctxlake quarantine` | Stop one agent's claims from promoting |
| `ctxlake briefing` | Render the session briefing, or write it to the cache the hook reads |
| `ctxlake mcp` | The stdio MCP server; `ctxlake install` registers it with each runtime |

Every subcommand accepts `--config <path>` to point at a `ctxlake.toml` elsewhere than
`$XDG_CONFIG_HOME/ctxlake/ctxlake.toml` (or `~/.config/ctxlake/ctxlake.toml` when
`XDG_CONFIG_HOME` is unset) — useful for running more than one agent identity per host.

### `ctxlake update`

```sh
ctxlake update [--check]
```

Updates `ctxlake` **and** `ctxlake-hook` to the latest release. `--check` reports
whether a newer one exists and changes nothing.

It infers how this copy was installed from where the binary lives, rather than
remembering anything at install time — nothing writes a receipt, and a remembered one
would be wrong the moment someone moved the file.

| Where the binary is | What `update` does |
|---|---|
| Under a Homebrew prefix | `brew update && brew upgrade oxidantdata/tap/ctxlake` |
| `~/.cargo/bin` | `cargo install --git ... --tag <latest> --force` (compiles) |
| Anywhere else | Downloads the release archive, verifies it against `SHA256SUMS`, replaces both binaries |
| A `target/{debug,release}` directory | Refuses — that is a checkout, not an install |

**A package manager's files are never written to directly.** Dropping a new binary into
a Cellar leaves Homebrew's manifest describing a file that is no longer there:
`brew list --versions` reports the old version and the next `brew upgrade` silently
overwrites the update.

**The service unit is re-rendered before the restart.** Two things in a unit go stale on
an upgrade, and neither is visible until the daemon is already dead: the binary path,
when a package manager's version-stamped directory is deleted, and the pinned `PATH`,
when a provider binary was installed after `sync install` ran. Refreshing costs nothing
when neither is true.

**Replacement is by rename, never in place.** `ctxlake-hook` fires on every tool call in
every live agent session on the machine; writing over it in place would mean some
session's hook executes a half-written file. A rename is atomic, and a process that
already has the old binary open keeps it — which is also why `update` restarts the sync
daemon afterwards, if one is installed.

### `ctxlake init`

```sh
ctxlake init --store <url> --fleet <id> [--agent-id <id>] [--force] [--daemon]
             [--llm <provider>] [--llm-model <m>] [--llm-key-env <VAR>]
             [--llm-base-url <url>] [--summarize-mode <mode>]
```

| Flag | Meaning |
|---|---|
| `--store` | Required. Verified by a real put/get round trip, not a URL parse |
| `--fleet` | Required. The boundary of who sees whom |
| `--agent-id` | This machine's identity within the fleet. Defaults to a sanitized hostname, stable across re-runs on the same host. **Two hosts sharing one id merge into a single roster entry and a single claim author**, so give each machine its own |
| `--force` | Required to overwrite an existing config; without it `init` refuses and changes nothing |
| `--daemon` | Also install and start the sync daemon as a service, so it survives a reboot — equivalent to `ctxlake sync install` afterwards |
| `--llm` | Configure Tier 2 with `claude-cli`, `anthropic`, `openrouter`, `gemini`, `openai-compatible` or `ollama`, and **verify it with a real call before writing the config**. `claude-cli` needs no key: it uses the `claude` binary's own subscription |
| `--llm-model` / `--llm-key-env` / `--llm-base-url` | Override the provider's defaults. The config stores the env var's *name*, never a key |
| `--summarize-mode` | `none` · `agent` · `batch` · `both` · `shadow`. Defaults to `shadow` |

> **`--fleet` is a real boundary in the store, not just a label.** Each fleet owns
> `live/fleets/<fleet_id>/`, so two fleets sharing a bucket never see each other's
> agents and two fleets using the same `agent_id` never share a key. Before v0.1.7 both
> were flat, and both of those went wrong.
>
> **`--force` with a different `--agent-id` or `--fleet` retires the identity it
> replaces**, deleting that agent's `live/` record. Without it the old name sits there
> written by nobody, and `ctxlake status` reported it as an active agent until the
> presence TTL expired it — which, before v0.1.7, was never.

### `ctxlake doctor`

Run this before you let ctxlake touch a real config. Every row is a real request against
your actual bucket via a scratch key that is cleaned up whether the probe passes or
fails.

```text
$ ctxlake doctor
store   s3://my-bucket/ctxlake
  put-if-absent / compare-and-swap / conflict detection   ok
  conditional GET / list / delete                         ok
runtimes
  claude-code  wired (2 other entries)   cursor  found, not wired   hermes  not found
daemon
  no cache at ~/.ctxlake/cache/myteam/roster.json yet — daemon not running, or no first refresh
  spool backlog: 0 file(s) · process: not running — `ctxlake sync` starts it
maintenance    never — ctxlake-maint has not published a snapshot yet
summarization  mode: agent · tier 1 nudges fired: 0 session(s)
```

| Row | If it fails |
|---|---|
| `put-if-absent` | **Expected to fail on MinIO**, and changes nothing — roster heartbeats are CAS-only and never depend on it |
| `compare-and-swap` / `conflict detection` | The roster fan-in cannot work on this backend. Capture is unaffected |
| `conditional GET` | Roster polling costs a full `GET` every cycle instead of a cheap 304 — works, scales worse |
| `list` | The roster fan-in and `ctxlake maint` cannot do their jobs |

It also reports whether an `api_key_env`'s named variable resolves (never its value), the
daemon's process and spool state, how long ago `snapshot/latest.json` was published (from
the store's own `last_modified`, never this host's clock), and Tier 1's nudge count.

**Exit code:** non-zero only when the store cannot be written to and read from at all.
Every other gap degrades *coordination*, not capture, and is loud in the report without
flipping the exit code.

### `ctxlake import`

```sh
ctxlake import --all [--since 90d]
ctxlake import --runtime claude-code|cursor|hermes
ctxlake import --project <path>
ctxlake import --dry-run          # count and classify, write nothing
```

Resumable and idempotent; deduplicated by content hash. Per-runtime fidelity is in
[Adding it](adding-it.md).

### `ctxlake install` / `ctxlake uninstall`

```sh
ctxlake install claude-code|cursor|hermes [--dry-run]
ctxlake install --all              # every runtime doctor reports as "found"
ctxlake uninstall <runtime>|--all
```

The one place ctxlake writes into a file it did not create, so each guarantee below is
asserted against fixtures shaped like real, already-populated configs. Every pre-existing
entry survives. A `.bak` is written before any change, and a no-op run does not touch it.
Re-running changes nothing byte for byte; a changed `fleet_id`/`agent_id` replaces
ctxlake's own stale entry rather than leaving two. `uninstall` removes exactly what
`install` added, matched by a stable marker rather than a bare substring. A malformed
existing file is refused, not guessed at — no `.bak`, no partial merge.
`CTXLAKE_FLEET_ID`/`CTXLAKE_AGENT_ID` reach the hook as a shell-style command prefix,
since no hook schema has an `env` key.

| Runtime | File | Shape | Events installed |
|---|---|---|---|
| Claude Code | `~/.claude/settings.json` | `hooks.<event>` is an array of matcher groups; ctxlake appends its own with no `matcher` | `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PreCompact`, `Stop`, `SubagentStop`, `SessionEnd` |
| Cursor | `~/.cursor/hooks.json` | `hooks.<event>` is a flat array of `{command}` | `beforeSubmitPrompt`, `beforeShellExecution`, `beforeReadFile`, `beforeMCPExecution`, `afterFileEdit`, `stop` |
| Hermes | `~/.hermes/config.yaml` | `hooks.<event>` is a flat list of `{command}` | `on_session_start`, `pre_llm_call`, `pre_tool_call`, `post_tool_call`, `post_llm_call`, `on_session_end` |

Hermes's `pre_tool_call` also gets `timeout: 5` and `fail_closed: false`. `--dry-run`
prints a line-level diff without writing. `--all` targets every runtime whose config file
exists; a runtime with no config file is left untouched.

> **`ctxlake install hermes` does not exist yet.** Write the `hooks:` block by hand for
> now — [Runtimes](runtimes.md) has it.

### `ctxlake status`

Read from the local cache, never the network: who is active, in which repo, on what, and
how long they have been at it. `status` prints the cache's age and says so explicitly once
it is over a minute old. If the cache has never been written it says exactly that, rather
than printing an empty roster that looks like "nobody is here."

### `ctxlake sync`

```sh
ctxlake sync run                      # daemonize (bare `ctxlake sync` does the same)
ctxlake sync run --foreground         # stay attached — what a service unit invokes
ctxlake sync start | stop | restart | status
ctxlake sync install [--no-start] [--skip-checks]   # survive reboots
ctxlake sync delete                   # remove exactly what install added
```

Runs four loops in one process: spool → `sessions/`, store → local cache, this agent's
heartbeat, and the **maintenance chain** every 5 minutes. Without it running somewhere,
sessions sit in the spool and never reach the lake — and because the hook only ever writes
locally, nothing tells you it stopped. That is why `install` exists.

**Every failure is logged and retried next tick**, including an unreachable
`[summarize.batch]` provider, which the daemon rebuilds from config on each cycle — so a
key that comes back, or a `claude` binary that gets installed, starts working without
anyone restarting anything.

Nothing about the model is allowed to stop the daemon starting. An earlier version
treated an unbuildable provider as fatal, on the reasoning that failing loudly beats
failing into a log nobody reads. That was right about the symptom and wrong about the
blast radius: capture shipping, cache refresh and presence have nothing to do with a
model, and taking all three down because an *optional* summarizer is unreachable is
strictly worse than missing some claims — especially under a supervisor, where it
becomes a restart loop reported as nothing more informative than `activating`.

`start`/`stop`/`restart`/`status` mean the same thing either way: with a service
installed they drive systemd or launchd, without one they drive a detached child process
(own process group, stdio to `~/.ctxlake/run/<fleet_id>.log`). Liveness is checked with
`kill -0` against the pidfile rather than by trusting the file; `stop` sends `SIGTERM`,
waits 5 seconds, and reports what actually happened.

| | Linux | macOS |
|---|---|---|
| Written to | `~/.config/systemd/user/ctxlake-sync.service` | `~/Library/LaunchAgents/com.oxidantdata.ctxlake-sync.plist` |
| Scope | systemd **user** unit | **LaunchAgent** |
| Starts at | login, and on `install` | login, and on `install` |
| Survives reboot | **only with lingering** (below) | yes |

The unit pins `HOME` and `PATH` rather than inheriting them. Both supervisors start jobs
with an environment that is not yours: launchd supplies its own `HOME` from the user
record and a `PATH` of `/usr/bin:/bin:/usr/sbin:/sbin`. `install` writes the values it
ran with, so the pre-install checks and the daemon resolve the same files and the same
binaries. It also writes the **stable** path to `ctxlake` — not the version-stamped one a
package manager resolves to, which the next upgrade deletes.


Both are user-scoped deliberately: the daemon reads *your* spool and writes with *your*
store credentials. A system unit or LaunchDaemon runs as root and would reach neither.

> **Linux: `sudo loginctl enable-linger $USER`.** Without lingering, systemd stops your
> user manager at logout and does not bring it back after a reboot — on a headless host,
> where a fleet agent actually runs, that means the daemon silently never returns.
> `ctxlake sync install` checks for it and `ctxlake sync status` reports it, but only you
> can set it.

`install` runs the `ctxlake doctor` checks first and **refuses** on any of these:

| Blocks the install | Why |
|---|---|
| The store is unreachable | The daemon would come up `active` and ship nothing |
| A CAS or conditional-GET probe fails | No roster fan-in, no snapshot publish |
| `[summarize.batch]` is set but its `api_key_env` is not | Nothing would ever extract, and you asked for extraction |
| The key resolves but the provider rejects a live call | A revoked key or typo'd model resolves fine and fails hours later |

A key that resolves *only in your shell* is a **warning**, not a blocker: someone running
the daemon under their own supervisor, or with a systemd drop-in carrying their own
`EnvironmentFile=`, has a working setup this cannot see — and since an unreachable
provider no longer takes the daemon down with it, being wrong in that direction costs
claims rather than capture. See
[Provider keys and the daemon](#provider-keys-and-the-daemon-config-ctxlake-env).

Missing runtime hooks are a warning, not a blocker — wiring a runtime after the daemon is
an ordinary order to do things in. `--skip-checks` overrides the whole gate for an
air-gapped host; the daemon will then start and fail at whatever you skipped.

`ctxlake init --daemon` does init, install and start in one step, with the same checks.

> **A missing or unusable `ctxlake.toml` exits 78** (`EX_CONFIG`), not 1 — no number of
> restarts writes a config file, so the unit carries `RestartPreventExitStatus=78` and
> systemd gives up immediately instead of relaunching every five seconds forever.
> Everything else exits 1, so a genuinely transient failure (an unreachable store) is
> still retried.

### `ctxlake briefing`

```sh
ctxlake briefing            # render it to stdout — what an agent would be told
ctxlake briefing --write    # write it to <cache>/<fleet_id>/briefing.json
```

The read path's expensive half. Listing the roster, folding claims, and applying
attribution all need the lake, and the hook may not touch the object store under its 5ms
budget — so rendering happens here and the hook only reads the result. `ctxlake sync`
calls `--write` on its cache loop; running it by hand is how you see exactly what your
next session will open with.

The hook **fails open** on every path: no cache, unreadable file, malformed JSON, daemon
never started — each yields a normal session with no briefing. A missing briefing costs
an agent some context; a hook that fails a session start costs you the turn.

### `ctxlake maint`

```sh
ctxlake maint             # loop forever, one cycle every 5 minutes
ctxlake maint --once      # one cycle and exit — the daemon already does this for you
```

One chain, in order:

| Step | Runs when |
|---|---|
| Compact small Parquet files | always |
| Write Tier 0 digests for newly sealed sessions | always |
| Tier 2 batch extraction | `[summarize.batch]` is configured — see [Memory](memory.md#turning-on-tier-2) |
| Promotion gate | always, even with no Tier 2 config — `memory_propose` candidates still need gating |
| Publish `snapshot/` | always, after the gate, so a claim promoted this run is in this run's snapshot |

**Safe to schedule on every host, or none** — no lock, no primary host, because every step
is idempotent by content ([How it works](how-it-works.md)). Overlapping runs cost
redundant work, never corrupted output. If nobody runs it, capture and coordination keep
working; the lake just stays as fresh as the last pass.

### `ctxlake maint --prune`

```sh
ctxlake maint --prune --dry-run   # report exactly what would go
ctxlake maint --prune             # do it
```

Deliberately separate from the chain rather than a step in it. The chain only ever
*adds*, which is what makes running it from every host at once safe; deleting is
different in kind and should be something you ask for, not something that happens on a
five-minute timer you forgot you installed.

Two categories, and nothing else:

| Removed | Why it is safe |
|---|---|
| Superseded `snapshot/<hash>.sqlite` older than 24h | Nothing ever removed these, so a fleet republishing every 5 minutes accumulated one per change forever. The blob `latest.json` points at is never a candidate, and neither is one younger than the grace window — a reader that just resolved the pointer is about to fetch the blob it named |
| `live/agents/*.json`, `live/roster.json` | The pre-fleet-scoping layout. No current version writes them |

**Sessions, claim events, digests and compaction generations are never touched.** They
are either irreplaceable history or content-addressed inputs a lagging reader may still
resolve — `--prune` is narrow so that a mistake there is impossible rather than
unlikely. Deleting old session data is a decision about your own history; do it with
your object store's own tooling, deliberately.

### `ctxlake claims`

```sh
ctxlake claims --status candidate [--explain]
ctxlake claims --status contested
ctxlake claims --status promoted
```

`--status candidate` lists every proposed claim under `claims/events/`, grouped by type
and text, with every observing agent. `--explain` names which of the four gates would
reject each candidate, and why — *"rejected by the evidence gate: hypotheses never
auto-promote beyond agent scope"*.

> **`--explain` is a read-only, best-effort evaluator**, not a second implementation of
> the gate. The contradiction and independence checks need data it does not carry, and it
> says so rather than guessing. `--status promoted`/`contested` read the local cache
> mirror, the same file `memory_search` reads.

### `ctxlake quarantine`

```sh
ctxlake quarantine <agent_id>
```

Its claims stop promoting, its already-promoted claims move to `contested`, and capture
continues. There is no `ctxlake unquarantine` yet, but the marker is a plain object at a
known key. See [Memory](memory.md) for the known gap between this command and the
gate.

## Configuration — `ctxlake.toml`

`ctxlake init` writes it, `ctxlake config` prints the resolved result, every other
subcommand reads it.

> **Nothing in this file can ever be a secret *value*** — only the *name* of an
> environment variable that resolves to one. `ctxlake doctor` reports whether a named
> variable resolves, never what it resolves to. There is no field that accepts a key, so
> there is no way to leak one here by accident, and `ctxlake config` has nothing to mask.

```toml
store             = "s3://my-bucket/ctxlake"
fleet_id          = "myteam"
agent_id          = "cc-01"

[summarize]
mode = "agent"

[summarize.batch]                   # only read when mode is batch, both, or shadow
provider              = "anthropic"
model                 = "claude-haiku-4-5"
api_key_env           = "ANTHROPIC_API_KEY"
base_url              = ""
use_batch_api         = true
max_sessions_per_run  = 50
max_input_tokens      = 8000
```

Everything below `agent_id` is optional and defaults exactly as shown. A file with just
`store`, `fleet_id` and `agent_id` is what `init` writes.

| Field | Meaning |
|---|---|
| `store` — required | `s3://bucket/prefix`, `gs://…`, `az://…` (also `abfs://`/`abfss://`), or `file:///absolute/path`. Credentials are never part of this line — see **Credentials** below |
| `fleet_id` — required | The boundary of who sees whom. Map it to a team genuinely collaborating, not to a company |
| `agent_id` — required | A stable logical identity — not a hostname, and not something that changes across restarts |

### Credentials

Resolved the way the AWS CLI resolves them, first match wins:

| Source | Notes |
|---|---|
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` (+ `AWS_SESSION_TOKEN`) | Always wins, so an explicit export is never overridden by a stale file |
| `~/.aws/credentials`, profile from `AWS_PROFILE` or `default` | Static keys only. Region comes from there or `~/.aws/config` |
| EC2 instance role / ECS task role / web identity | What `object_store` resolves on its own |

> **SSO, `credential_process` and assume-role profiles are not read.** They all mint
> *temporary* credentials that expire under a long-running daemon, and quietly handing
> a daemon credentials that expire is worse than saying so. `ctxlake doctor` names the
> mechanism when it spots one instead of reporting an unhelpful "store unreachable".

The daemon installed by `ctxlake sync install` has no shell, so it never sees an
exported variable — it reads `~/.aws/credentials` under the `HOME` the unit pins. If
your `default` profile is not the identity that owns the bucket, set `AWS_PROFILE`
somewhere the service will see it, or make the right profile the default.

`ctxlake doctor` prints which identity it resolved, on success and on failure, and
tells a `403` apart from a network error — on a machine with more than one account
configured those look identical otherwise:

```text
backend: AWS S3
  credentials: profile "default" from ~/.aws/credentials (the default, since AWS_PROFILE is not set)
  UNREACHABLE: ... 403 Forbidden ... AccessDenied
      -> this is an authorization failure, not a network one — the request was signed and refused.
```

### Provider keys and the daemon — `~/.config/ctxlake/env`

The same "a service has no shell" problem applies to LLM provider keys, and it is
easier to miss because everything looks fine from a terminal.

`ctxlake.toml` holds the **name** of an environment variable, never a key. That name has
to resolve *where the daemon runs*, and a launchd job or a systemd user unit starts with
neither your exports nor your shell rc. So:

```sh
mkdir -p ~/.config/ctxlake
touch ~/.config/ctxlake/env && chmod 600 ~/.config/ctxlake/env
echo "OPENROUTER_API_KEY=$(printenv OPENROUTER_API_KEY)" >> ~/.config/ctxlake/env
```

Every `ctxlake` process loads this file at startup, before anything else runs.

| Rule | Why |
|---|---|
| Mode must be `600` (owner-only) | A credentials file any local process can read is a published credential. ctxlake refuses to read it otherwise and says so |
| An existing variable is never overridden | `FOO=bar ctxlake maint` must mean what it says |
| Format is `NAME=value`, `#` comments, optional `export ` | Deliberately not shell. A file that looks like it supports `$(...)` and silently does not is worse than one that obviously does not |

`ctxlake doctor` checks specifically for a key that this shell has and the daemon will
not, and prints the commands above.

The daemon's `PATH` is handled differently, because it is not a secret: `ctxlake sync
install` pins the `PATH` it was invoked with into the unit file. That is what makes
`provider = "claude-cli"` and `provider = "ollama"` work under a supervisor — launchd's
default `PATH` is `/usr/bin:/bin:/usr/sbin:/sbin`, which contains no Homebrew and no
`~/.local/bin`. **If you install a provider binary after running `sync install`, run
`ctxlake sync install` again** so the unit picks up the new directory.

### `[summarize]`

| `mode` | What runs | Needs a key? |
|---|---|---|
| `none` | Tier 0 structural digests only | No |
| `agent` — **default** | Tiers 0 and 1 — the agent that did the work writes its own handoff | No |
| `batch` | Tiers 0 and 2 (batch claim extraction) | Yes |
| `both` | Tiers 0, 1 and 2 | Yes |
| `shadow` | Everything Tier 2 does, but no promoted claim ever reaches a context window | Yes |

Coordination, session history, briefings and Tier 0 digests work identically regardless
of this setting. `[summarize.batch]` is ignored under `none` and `agent`:

| Field | Meaning | Default |
|---|---|---|
| `provider` | `anthropic`, `openai-compatible`, `ollama`, `openrouter`, `gemini`, or `claude-cli` | required |
| `model` | Model name for the batch provider | required |
| `api_key_env` | **Name** of the environment variable holding the API key | required |
| `base_url` | Override for a self-hosted, `ollama`, `openrouter`, or `gemini` endpoint | provider default |
| `use_batch_api` | Use the provider's batch API — half the price, results out of order, keyed by request id | `true` |
| `max_sessions_per_run` | Sessions extracted per maintenance run | `50` |
| `max_input_tokens` | Per-session input cap; oldest turns truncated first | `8000` |

| `provider` | Wire shape | Needs `base_url`? |
|---|---|---|
| `anthropic` | Messages API | No — defaults to `api.anthropic.com` |
| `openai-compatible` | `/chat/completions` | Yes — any self-hosted gateway speaking it |
| `ollama` | `/api/chat` | No — defaults to `localhost:11434` |
| `openrouter` | `/chat/completions` (OpenAI-shaped) | No — defaults to `openrouter.ai`; value is not needing to know the URL |
| `claude-cli` | Shells out to `claude -p --output-format json` | No — uses the subscription that CLI is signed in to. Needs `claude` on `PATH`, and **no** `ANTHROPIC_API_KEY` set, since the CLI prefers a key over the subscription |
| `gemini` | `generateContent`, structured output via `responseSchema` | No — defaults to `generativelanguage.googleapis.com` |

Running entirely locally, so no transcript leaves the host — a first-class path, not a
degraded one:

```toml
[summarize.batch]
provider    = "ollama"
model       = "qwen2.5:14b"
base_url    = "http://localhost:11434"
api_key_env = "OLLAMA_API_KEY"   # unused locally, but still a name, never a value
```

## MCP tools

`ctxlake mcp` is a stdio MCP server — JSON-RPC 2.0 over newline-delimited stdin/stdout,
protocol revision `2024-11-05`. All three runtimes are MCP clients, so this is the one
surface that works identically everywhere.

`ctxlake install <runtime>` registers it. Claude Code's entry lands in `~/.claude.json`
and Cursor's in `~/.cursor/mcp.json` — **different files from their hook configs**, and
merged with the same never-clobber discipline: `~/.claude.json` is over 100 KB of live
state on a real machine, and everything outside `mcpServers.ctxlake` survives untouched.

```json
{ "mcpServers": { "ctxlake": {
    "command": "ctxlake", "args": ["mcp"],
    "env": { "CTXLAKE_FLEET_ID": "myteam", "CTXLAKE_AGENT_ID": "cc-01" } } } }
```

`fleet_id` and `agent_id` ride in `env` because the server resolves them from the
environment exactly as the hook does, and a server started by a runtime inherits none
of your shell's.

> **Hermes is not wired.** Its MCP configuration lives in `config.yaml` under a schema
> no live install has been available to check against, and guessing a config shape from
> documentation is how this project's worst bug started. `ctxlake install hermes` wires
> hooks only and says so.

> **Hooks and MCP are different directions.** Hooks let ctxlake *tell* an agent things
> at session start; MCP is the only way an agent can *ask*. A machine with hooks and no
> MCP server has a working belief layer nothing can query — which is what every install
> was until this landed. `ctxlake doctor` now reports it.

| Tool | Shape | What it does |
|---|---|---|
| `fleet_status()` | read | Who is active and what they are touching, as of the last cache refresh |
| `fleet_history(repo?, since?, limit?)` | read | Recent sessions and outcomes from the cache |
| `fleet_handoff(summary, status, next?)` | write | Leave a note for whoever picks this up next |
| `memory_search(query, scope?, subject?, claim_type?, k?)` | read | Promoted claims matching `query`, with attribution |
| `memory_propose(claim, type, subject, evidence[])` | write | File a claim **candidate**. Never promotes |
| `memory_timeline(subject, since?, limit?)` | read | What this fleet has actually tried, re: `subject` |

`limit`/`k` default to 50 (10 for `memory_search`) and clamp to a hard ceiling of 200; an
over-limit result says `"truncated": true`. `memory_propose`'s `evidence` array is capped
at 20 citations and rejected outright over that rather than trimmed.

**This process never touches the network**, for the same reason the hook does not: an
agent is synchronously waiting on it. Reads come from `~/.ctxlake/cache/<fleet_id>/`,
writes go to `~/.ctxlake/spool/mcp/<fleet_id>.ndjson` for `ctxlake sync` to apply later,
and the crate links no `object_store` and no `tokio`. Override the roots with
`CTXLAKE_SPOOL_DIR` / `CTXLAKE_CACHE_DIR`.

> **A write-shaped tool can only report that a request is queued**, never that it reached
> the fleet — this process cannot observe that round trip. Every read tool degrades
> honestly when its cache file is missing or unparseable: an empty result with a `note`,
> never an error and never a guess. `"enabled": false` means no `ctxlake maint` run has
> published a snapshot yet; a fleet in shadow mode reads `"enabled": true` with an
> always-empty result.

`memory_propose` cannot write a promoted claim: there is no `memory_write` tool, `propose`
hard-codes `scope: "agent"`, and nothing in the crate constructs a `claims/fleet/*` write.
It enforces "no evidence, no claim" itself — an empty `evidence` array is refused, as is
any citation missing a `session_id` or `message_id`.

Both directions are guarded. **On read**, zero-width and bidi control codepoints are
stripped and every field is length-bounded with a visible `…[truncated]` marker,
recursively, at render time. **On write**, every free-text argument runs through the same
`Redactor` the hook's adapters use, before the record is built. The MCP spool caps its
directory size and rotates; unlike the hook, hitting the cap returns an ordinary tool
error the agent can see. The `SessionStart` briefing's fleet-context block reuses this
same snapshot read and attribution renderer, capped at five lines.

### Protocol conformance

`initialize`, `tools/list`, `tools/call` and `ping` are the whole method surface; every
other input returns a JSON-RPC error rather than a panic or a dropped connection.

| Input | Response |
|---|---|
| Malformed JSON | `-32700` Parse error, `id: null` |
| A JSON value that is not an object | `-32600` Invalid Request |
| An object with no `method` | `-32600` if it has an `id`, otherwise dropped |
| An unknown method | `-32601` Method not found |
| `tools/call` for an unknown tool, or a missing/wrong-typed argument | `-32602` Invalid params |
| A tool refusing on its own terms (e.g. no evidence) | a normal result with `isError: true` — the session keeps going |
| A notification (`id` absent) | never answered, even for an unknown method |
| A stray `result`/`error` frame with no `method` | never answered — not addressed to this server |

**stdout carries JSON-RPC frames and nothing else.** Every diagnostic goes to stderr; a
stray `println!` would corrupt every frame after it.

## Next steps

- [Getting started](getting-started.md) — these commands in the order a first run uses
- [Memory](memory.md) — what `[summarize]` selects between
- [Storage](storage.md) — the capability matrix `doctor` executes
