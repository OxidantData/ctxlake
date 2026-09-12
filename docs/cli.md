# cli — the `ctxlake` binary

Every command below is real: this page describes `crates/ctxlake-cli`, not a plan for
it. Adoption is the product here — a coordination tool nobody can install without
reading source code coordinates nothing — so this is written for someone who has never
seen ctxlake before and is deciding, right now, whether to trust it with their config
files.

```text
ctxlake init        point at a store, write ctxlake.toml
ctxlake doctor      the trust-building command — run this before install
ctxlake install     merge hooks into a runtime, never clobber
ctxlake uninstall   remove exactly what install added
ctxlake status      who's active, on what, holding what
ctxlake claim       acquire a lease on a resource
ctxlake release     give one back
ctxlake config      print the resolved config
```

Every subcommand accepts `--config <path>` to point at a `ctxlake.toml` somewhere other
than the default (`$XDG_CONFIG_HOME/ctxlake/ctxlake.toml`, or `~/.config/ctxlake/ctxlake.toml`
if `XDG_CONFIG_HOME` is unset). This is how the tests in this crate run three fictitious
agents against one `file://` store without touching your real home directory, and it is
useful for the same reason on a real host that runs more than one agent identity.

## `ctxlake init`

```sh
ctxlake init --store <url> --fleet <id> [--agent-id <id>] [--force]
```

Three things happen, in order, and any failure in the middle leaves nothing behind:

1. **A real round trip against the store** — a `put`, not just a URL parse. "Verify
   the store is reachable" means exactly that: can this process write a byte and read
   it back, right now. This is deliberately lighter than `doctor`'s full capability
   matrix (below) — `init`'s job is "can we talk to this bucket at all," not "which
   conditional-write primitives does it support."
2. **Provisioning the two well-known lease keys** this crate defines up front:
   `live/leases/_maintenance` (the lease `ctxlake maint` contends for) and
   `live/leases/_claim_provision` (a lock `ctxlake claim` uses internally to
   serialize the first-ever provisioning of every *other* lease — see the box
   below). Every other lease is keyed by an arbitrary resource string a human
   hasn't typed yet ([`claim`](#ctxlake-claim), below), which is exactly why only
   these two fixed keys can be provisioned up front; `init` must run — once, before
   any `ctxlake claim` ever touches this store — for that lock to exist at all.
3. **Writing `ctxlake.toml`** — see [config.md](config.md) for the full shape.

`--agent-id` is optional. Omitted, it falls back to a sanitized hostname
(`Vamsis-MacBook-Pro.local` becomes `vamsis-macbook-pro-local`), which is at least
*stable across re-running `init` on the same host* — AGENTS.md's own knob table calls
agent-id stability "operator-assigned," and a fresh random id every run would defeat
that on the one axis a default can actually help with.

`--force` is required to overwrite an existing config. Without it, `init` refuses and
changes nothing — the same "never clobber silently" reasoning AGENTS.md invariant 8
applies to hook installs applies here: a config file is something a human wrote on
purpose, and a tool that overwrites it by default is a tool nobody should run twice.

```sh
$ ctxlake init --store s3://my-bucket/ctxlake --fleet myteam
wrote /Users/you/.config/ctxlake/ctxlake.toml
  store:     s3://my-bucket/ctxlake
  fleet_id:  myteam
  agent_id:  your-hostname

Next: ctxlake doctor
```

To try ctxlake with no cloud account at all:

```sh
ctxlake init --store file:///tmp/ctxlake-demo --fleet local
```

Leases, roster, and status all work unchanged against a local directory. What you lose
is a second machine's ability to join — see [storage.md](storage.md) on why `file://`
is a dev/single-host backend, not a fleet one.

## `ctxlake doctor`

The trust-building command — the one meant to answer "can I believe this tool" before
you let it touch a real config. Two sections, and a policy for what actually fails the
command:

```text
$ ctxlake doctor
store   s3://my-bucket/ctxlake
  put-if-absent                ok     a second Create was correctly rejected
  compare-and-swap             ok     Update succeeded against the version we just wrote
  conflict detection           ok     a stale version was correctly rejected
  conditional GET              ok     If-None-Match correctly reported unchanged
  list                         ok     saw both scratch objects among 2 listed
  delete                       ok     object was gone immediately after delete

runtimes
  claude-code  wired      (2 other entries)   /Users/you/.claude/settings.json
  cursor       found, not wired               /Users/you/.cursor/hooks.json
  hermes       not found

daemon
  no cache at /Users/you/.local/share/ctxlake/cache/myteam/roster.json yet — daemon not running, or hasn't completed a first refresh
  spool backlog: 0 file(s), 0 bytes — /Users/you/.ctxlake/spool
```

### Backend: every primitive is *executed*, never assumed

Each row above is a real request against your actual bucket, via
[`ctxlake_store::probe`](../crates/ctxlake-store/src/probe.rs) — a scratch key under
`_meta/probe/<caller_id>/`, cleaned up whether the probe passes or fails. Backends
genuinely differ (see [storage.md](storage.md)'s capability matrix), so a **failing**
row prints what it means, not just a red cross:

- `put-if-absent` failing is **expected on MinIO** ([minio/minio#20346](https://github.com/minio/minio/issues/20346))
  and changes nothing — leases and roster heartbeats are CAS-only by design (AGENTS.md
  invariant 4) and never depend on put-if-absent working.
- `compare-and-swap` or `conflict detection` failing means leases and the roster fan-in
  cannot work correctly on this backend. Capture — writing sessions — is unaffected;
  this breaks `live/`, not `sessions/`.
- `conditional GET` failing means roster polling costs a full `GET` every cycle instead
  of a cheap 304 — works, scales worse (see [scaling.md](scaling.md)).
- `list` failing means the roster fan-in and `ctxlake maint` can't do their jobs; again,
  capture does not need `list`.

### Runtimes: coexistence is reported as a fact, not a problem

`doctor` detects Claude Code (`~/.claude/settings.json`), Cursor
(`~/.cursor/hooks.json`), and Hermes (`~/.hermes/config.yaml`), and reports one of:
**not found** (no config file at that path), **found, not wired** (the file exists,
ctxlake hasn't installed into it yet), or **wired**, with a count of hook entries that
belong to some other tool. That count is informational, never a warning — a config
already carrying a dozen entries from other tools is the normal, expected case
`ctxlake install` is built to merge into (AGENTS.md invariant 8).

### The LLM key, if one is configured

If `[summarize]` mode needs batch extraction (`batch`, `both`, or `shadow` —
see [config.md](config.md)), `doctor` prints whether `[summarize.batch].api_key_env`'s
named environment variable resolves — `yes` or `no`, **never the value**. There is no
code path in this crate that reads that variable and prints it; AGENTS.md invariant 10
is enforced by there being nothing to print.

### The daemon and the spool

Neither of these can be checked by asking a running process (there isn't one yet to
ask, in this wave) — `doctor` instead reports what the filesystem shows: the local
cache's freshness (`~/.local/share/ctxlake/cache/<fleet_id>/roster.json`'s mtime, if it
exists at all) and the spool's backlog (file count and total bytes under
`$CTXLAKE_SPOOL_DIR`, or `~/.ctxlake/spool` if that's unset — the exact root
`ctxlake-hook` itself appends to, **not** fleet-scoped: the hook has no reliable
`fleet_id` at capture time, so it partitions by runtime only, and this has to watch
the same root or it reports an empty spool regardless of what the hook actually
wrote). A missing cache or a growing spool are reported plainly, in the same language
[architecture.md](architecture.md)'s failure-mode table uses, rather than guessed at.

### Exit code

`doctor` exits non-zero **only when the store cannot be written to and read from at
all** — capture (`hook -> spool -> daemon -> sessions/`) needs nothing but a plain
`put`. Every other gap above — no put-if-absent, no CAS, no conditional `GET` — degrades
*coordination*, not capture, and is loud in the printed report without flipping the exit
code. A CI job that gates on `ctxlake doctor` is gating on "can this store be written
to," which is the honest thing to gate a pipeline on.

## `ctxlake install` / `ctxlake uninstall`

```sh
ctxlake install claude-code [--dry-run]
ctxlake install cursor
ctxlake install hermes
ctxlake install --all              # every runtime doctor would report as "found"
ctxlake uninstall <runtime>|--all
```

This is the most important correctness property in this crate, because it is the one
place ctxlake writes into a file it did not create. **Every guarantee below is
load-bearing, not aspirational** — it is what the round-trip test suite in
`crates/ctxlake-cli/src/hooks/` asserts against fixtures shaped like real,
already-populated configs (a Claude Code `settings.json` with other hook events and a
`statusLine` already set; a Cursor `hooks.json` whose events already point at another
tool via long shell commands with env-var prefixes):

- **Every pre-existing entry survives.** Another tool's matcher group on the same
  Claude Code event, another plugin's command on the same Hermes event, an unrelated
  top-level key like `model` or `statusLine` — none of it is touched.
- **A `.bak` is written before any change**, holding whatever was on disk immediately
  before this run — not the pre-ctxlake original if you've run `install` before and
  edited the file since. A no-op run (nothing changed) does not even touch the `.bak`.
- **Re-running `install` changes nothing** — byte for byte. And if `fleet_id` or
  `agent_id` changes in `ctxlake.toml` between two runs, the second run *replaces*
  ctxlake's own stale entry with a fresh one rather than leaving two behind: install is
  idempotent against a fixed config and self-correcting against a changed one.
- **`uninstall` removes exactly what `install` added.** Detection is a stable marker,
  but not a bare substring search: a command line counts as ctxlake's own only when a
  whitespace-delimited token is exactly `ctxlake-hook` (or ends `/ctxlake-hook`)
  *and* is followed two tokens later by one of the three real runtime args
  (`claude_code`, `cursor`, `hermes`) — the exact tail `ctxlake-hook <event>
  <runtime>` this crate always writes. That survives a user's own wrapper around
  the line (`timeout 5 nice -n 19 env ... ctxlake-hook PostToolUse claude_code
  2>>...`) while refusing to touch a foreign tool that merely *mentions*
  `ctxlake-hook` — as an audit tool's own argument, say — which a bare substring
  match would have deleted. An event array or `hooks` object that `install` had to
  create from nothing is removed on the way out too, rather than left behind as an
  empty, meaningless shell.
- **A malformed existing file is refused, not guessed at.** If `~/.claude/settings.json`
  isn't valid JSON (or `~/.hermes/config.yaml` isn't valid YAML), `install` and
  `uninstall` both error without writing anything — no `.bak`, no partial merge. Fixing
  or removing the broken file by hand is the safe next step, not a tool that tries to
  patch around invalid syntax in a file it doesn't fully understand.
- **`CTXLAKE_FLEET_ID` and `CTXLAKE_AGENT_ID` reach the hook process.** None of the
  three hook schemas has an `env` key, so these are set as a shell-style prefix on the
  command line itself: `env CTXLAKE_FLEET_ID=myteam CTXLAKE_AGENT_ID=cc-01 ctxlake-hook
  PostToolUse claude_code`. Without this, `ctxlake-hook` falls back to
  `unconfigured-fleet`/`unconfigured-agent` placeholders (see
  `crates/ctxlake-hook/src/hostinfo.rs`) and every event lands mis-attributed.

`--dry-run` computes the exact change and prints a line-level diff without writing
anything. The diff is a plain LCS line-diff, not `diff -u`. For the two JSON runtimes,
the whole file is fully re-serialized (2-space indent) rather than surgically
patched, so unrelated formatting can shift even though every key, value, and
ordering of untouched entries is preserved — comments have no place in JSON, so
there is nothing beyond whitespace this could lose. Hermes' YAML is handled more
narrowly: only the top-level `hooks:` block is regenerated and spliced back into the
original text, so a comment, anchor, or alias anywhere *else* in `~/.hermes/config.yaml`
survives byte-for-byte; a comment or anchor placed *inside* the `hooks:` block itself
does not, for the same reason a full YAML-aware round trip would be its own project
(`serde_yaml`, the crate this parses with, has no concept of either once parsed —
there is no comment or anchor left for a real diff to preserve, whichever way the
file is written back). In every case the guarantee is about *entries*, not
whitespace or formatting, and it is disclosed here rather than silently.

`--all` targets every runtime whose config file `doctor` would report as **found**
(`found, not wired` or `wired`) — a runtime with no config file at all is not touched,
since there is nothing to merge into and nothing to imply about a runtime you don't
actually use.

### What gets installed, per runtime

| Runtime | File | Shape | Events |
|---|---|---|---|
| Claude Code | `~/.claude/settings.json` | `hooks.<event>` is an array of matcher groups; ctxlake appends its own group with no `matcher` (runs for every tool) | `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PreCompact`, `Stop`, `SubagentStop`, `SessionEnd` |
| Cursor | `~/.cursor/hooks.json` | `hooks.<event>` is a flat array of `{command}` | `beforeSubmitPrompt`, `beforeShellExecution`, `beforeReadFile`, `beforeMCPExecution`, `afterFileEdit`, `stop` |
| Hermes | `~/.hermes/config.yaml` | `hooks.<event>` is a flat list of `{command}`, shell hooks per [runtimes/hermes.md](runtimes/hermes.md) — never the Python plugin in `adapters/hermes/` | `on_session_start`, `pre_llm_call`, `pre_tool_call`, `post_tool_call`, `post_llm_call`, `on_session_end` |

Hermes' `pre_tool_call` is the one entry that also gets `timeout: 5` and
`fail_closed: false` — it is the sole event where `fail_closed` does anything at all
(per [runtimes/hermes.md](runtimes/hermes.md)), and Hermes logs a warning if the flag is
set anywhere else. Left `false`: a collision check is advisory, and a hook crash must
never wedge a session.

Each event name is passed as `ctxlake-hook`'s first argument exactly as shown in the
table above — the runtime's own vocabulary for that event, matching the contract in
`crates/ctxlake-hook/src/main.rs` (argv[1] event, argv[2] runtime id).

> **Hermes capture does not work yet.** `ctxlake install hermes` writes correct,
> real shell-hook commands into `~/.hermes/config.yaml`, but `ctxlake-hook` itself
> does not yet normalize a live Hermes payload — `crates/ctxlake-hook/src/adapters/mod.rs`
> rejects `Runtime::Hermes` outright, and `main.rs`'s own module doc still describes
> Hermes as the in-process Python plugin (`adapters/hermes/`), not a process
> `ctxlake-hook` gets spawned for. Until that lands, every wired Hermes event spawns
> `ctxlake-hook`, which captures nothing and appends a line to
> `~/.ctxlake/hook-errors.log`. `install` prints this as a warning rather than
> refusing to run, since the config it writes is already correct for the moment
> normalization does land — but until then, do not expect `sessions/` to gain any
> Hermes data from this.



## `ctxlake status`

```sh
ctxlake status
```

```text
fleet myteam · 2 agent(s) active · roster 4s old (/Users/you/.local/share/ctxlake/cache/myteam/roster.json)

  cc-01  claude_code  github.com/OxidantData/ctxlake  14m
           touching crates/oxidant-catalog-glue/src/lib.rs
           "migrating the Glue catalog off the CLI shell-out"

leases (live, fetched just now — not cached)
  crates/oxidant-loom/**  held by cc-01  "splitting the S3 cache out"
```

Two sections, two different freshness stories, printed as two different stories rather
than one blended view:

- **The roster is read from the local cache** — the same file `ctxlake sync` refreshes
  on a poll interval for the hook and MCP server to read (AGENTS.md invariant 1: no
  network on that path). `status` prints the cache's age and says so explicitly once
  it's over a minute old, rather than implying a live view.
- **Leases have no cache yet** — nothing publishes a merged `leases.json` the way
  `roster::build` does for the roster — so this section is a live `LIST` against
  `live/leases/` right now, labeled `(live, fetched just now — not cached)` so the two
  sections' staleness is never confused with each other.

If the cache has never been written (`ctxlake sync` hasn't run, or hasn't completed its
first refresh), `status` says exactly that rather than printing an empty roster that
looks like "nobody's here."

## `ctxlake claim` / `ctxlake release`

```sh
ctxlake claim <resource>... [--reason <text>] [--ttl <seconds>] [--exclusive]
ctxlake release <resource>... | --all
```

`<resource>` is any string a human would type to describe what they're about to touch —
a path glob, a package name, a migration id. Each one is hashed independently via
`ctxlake_core::hash::resource_key(repo, resource)` into its own lease
(`live/leases/<hash>.json`); the repo half comes from `git remote origin`'s URL when
available, falling back to the git toplevel path and then the raw working directory, so
the same string in two different repos never collides.

**A refusal always names the current holder and their reason** — "someone else has it"
is useless output, and this crate treats that as a correctness property, not a nicety:

```text
$ ctxlake claim 'crates/oxidant-loom/**'
refused: crates/oxidant-loom/**: held by cc-01 ("splitting the S3 cache out") until 2026-09-12 5:48:31 +00:00:00
Error: 1 of 1 resource(s) could not be claimed
```

`--ttl` defaults to the fleet-wide 5-minute lease TTL (AGENTS.md's knob table). `--reason`
is free text, rendered plainly wherever it's shown — never interpreted as a template,
a path, or a command (a lease's `reason`, like a claim or handoff note, is untrusted
input from another agent's operator — AGENTS.md's house rule on rendering anything
read from the lake). "Plainly" is not "unfiltered," though: every value another
agent chose — a lease's `holder`/`reason`, a roster entry's `task`/`paths`/`repo` —
is passed through a sanitizer before it reaches your terminal, which strips control
characters (ANSI escapes included — nothing another agent's `--reason` writes can
clear your screen or repaint a line), the Unicode bidi-override and zero-width
characters that can visually reorder or hide text, and caps length so one huge value
cannot flood the output. That sanitizing is the whole reason "plainly" is a safe
promise to make about text you did not write.

`--exclusive` is the only thing "exclusive" can mean for a primitive that is already
single-holder by construction (a lease is never held by two agents at once, with or
without the flag): it changes what happens when you ask for *several* resources in one
call. Without it, `claim` is best-effort per resource — some may succeed, some may
refuse, independently. With `--exclusive`, it's all-or-nothing: if any requested
resource is already held, every lease this call did manage to acquire is released again
before it exits non-zero, so a partial claim never sits there silently after the command
reports failure.

`ctxlake release <resource>` only releases a lease this agent currently holds — trying
to release someone else's is refused, not forced:

```text
$ ctxlake release crates/oxidant-loom/**
crates/oxidant-loom/**: refused to release — held by cc-01, not cc-02
```

`ctxlake release --all` releases every lease this agent holds, found by listing
`live/leases/` and checking each one's holder — it never touches a lease held by
another agent, even one on the same resource string in a different repo.

> **Why release re-acquires before it releases.** Every `ctxlake` invocation is a fresh
> process — there is no `LeaseHandle` left over in memory from the `claim` that
> acquired it, and `ctxlake_store::lease::release` can only be called on a handle a
> successful `acquire` produced. The documented, supported path
> (`crates/ctxlake-store/src/lease.rs`'s own module doc: "the same holder re-acquiring
> after losing its in-memory handle") is exactly this case: re-acquire (trivially
> succeeds for the current holder), then release the handle that returns. If the lease
> was stolen by someone else in the instant between checking it and re-acquiring it,
> `release` says so — "was reclaimed by `<agent>` before release completed" — rather
> than silently doing nothing.

## `ctxlake config`

```sh
ctxlake config
```

Prints the resolved `ctxlake.toml`. There is nothing to mask by replacing a value with
`***`: AGENTS.md invariant 10 means this struct can never hold a secret in the first
place, only the *name* of an environment variable (`api_key_env`) — so "with secrets
elided" is true by construction, and the command says so in its own footer rather than
asking you to take it on faith.

## Next steps

- [config.md](config.md) — the full `ctxlake.toml` reference
- [getting-started.md](getting-started.md) — the same commands, in the order a first
  run actually uses them
- [coordination.md](coordination.md) — what a lease promises and what it does not
- [storage.md](storage.md) — the capability matrix `doctor` executes
