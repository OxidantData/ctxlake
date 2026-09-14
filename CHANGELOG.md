# Changelog

## v0.1.11

Four extraction defects, all found by running the v0.1.10 inspection commands against a
live lake rather than by reading the code.

> **Upgrading:** `ctxlake update`. Nothing to reconfigure, and **no re-extraction is
> forced** — see the last section.

### Claims citing anything the agent said were always dropped

`build_fenced_transcript` labels an envelope with no `message_id` — a prompt or an
assistant line — with its `event_id`, because that is the only id it has.
`build_resolvable_index` indexed `message_id` only. So the model was shown an id, cited
it correctly, and the claim was discarded as unresolvable.

This fired for **every claim whose evidence was something said rather than a tool call**,
which is where conventions and preferences live. Not random loss — it selectively
discarded the reflective claims and kept the mechanical ones.

Two functions computing the same value separately and disagreeing, so the fix is one
`citable_id()` that both call. **Measured on the live lake: 7 drops in a pass became 0.**

The test reads the id back out of the real rendered transcript instead of asserting what
the model is probably shown. That distinction is the whole point: every other citation
test hands `claim_from_raw` a message id the index was built with, so both halves agreed
with each other and disagreed with the prompt — the same shape as the hand-written
`tool_result` fixture that hid the v0.1.9 capture bug.

### A session of acknowledgements is empty, however many there are

The emptiness guard compared a sum, and a sum of scraps clears any threshold given enough
scraps: six lines reading `ok` score 12, exactly the bar meant to reject one line reading
`ok`. Such a session reached the model, came back as plain English rather than JSON,
failed, and was retried on every pass thereafter — one model call per cycle, forever, for
a session that cannot produce a claim.

Every claim has to cite a message, so the question is whether any *one* message is worth
citing. The guard now takes the longest line, not the total.

### Corroboration was capped at "recent enough and short enough"

Two ceilings sat in front of the known-claims preamble, and the second is a correctness
bug rather than a limit:

- Only the newest **60** known claims were shown. On a 135-claim fleet, **75 were
  invisible**, so a session re-observing one of them had nothing to match against.
- Every claim shown was cut at 240 characters, under an instruction to **REPEAT ITS TEXT
  CHARACTER FOR CHARACTER**. That is not a weaker instruction, it is an impossible one —
  the full text is not in the context. A model that complies produces text that no longer
  matches, so `find_existing_claim_id` finds nothing and mints a fresh claim. **Truncation
  manufactured exactly the duplicates the preamble exists to prevent.**

The preamble is now bounded by bytes (64 KiB) rather than by a claim count, and an
individual claim is shown whole or dropped. Those two rules go together: a size budget is
only safe if nothing inside it gets cut.

### Diagnostics that could actually be read

Following the drop rate to its cause meant reading source, because `7 DROPPED` named no
cause — and the `tracing::warn!` that would have named one goes nowhere: **nothing in
this workspace installs a `tracing_subscriber`**, so all four tracing calls in extraction
write to a subscriber that does not exist.

So the counts ride on the line that actually prints, and a failure now names its session:

```text
tier 2: 4 session(s) extracted, 20 of 27 claim(s) kept
        (7 DROPPED: 7 unresolvable citation, 0 bad claim_type)
```

An unknown `claim_type` and an unresolvable citation call for opposite fixes. A failure
retried every pass with no way to tell which session it is, is a bill with no address —
`ctxlake sessions <id>` now takes what the summary prints.

### No forced re-extraction, on measurement

All of the above change extraction, which is the textbook reason to bump
`EXTRACTOR_VERSION` and have every host re-extract its history. That was measured rather
than assumed: **two full 50-session re-extraction passes produced +2 claims each and zero
new corroboration**, for the largest model spend of any pass so far.

The reason is structural. Re-extracting a session re-reads a transcript whose claims are
already on file, and the extractor is explicitly told to propose only what is not already
recorded — so it correctly returns almost nothing. Corroboration needs a **different**
session observing the same fact, which re-extraction cannot manufacture by construction.

So the version stays put and these fixes earn their keep on sessions yet to be sealed.
The measurement lives at the constant, because the next person to change extraction will
face the same question and deserves the data rather than the intuition.

## v0.1.10

Two halves of the same problem: the fleet was accumulating duplicate beliefs, and there
was no way to look at the memory it had accumulated.

> **Upgrading:** `ctxlake update`. Nothing to reconfigure.

### Duplicate claims — prevented, not merged

Three extraction passes over the same session produced three near-identical claims about
the same fact. The extractor was shown nothing about what the fleet already believed, so
every pass reworded the same observation into a new claim, and each landed at
`independent_count = 1` instead of corroborating the first.

Extraction now shows the model the promoted and contested claims already on file and
asks it to **repeat a known claim character-for-character** when it sees the same fact
again, so the second observation merges onto the existing claim. Prevention is 92%
effective on new claims.

An earlier version of that instruction offered the model a choice — *either omit the
known claim, or repeat it*. A live probe showed it choosing omit and returning
`{"claims": []}`, which discarded the corroboration entirely. The instruction is now
repeat-never-omit.

**Merging the existing backlog is not viable, and that is a finding rather than a
limitation.** Measured on a real lake, word overlap does not separate a duplicate from a
distinction: the *distinct* TLS pair scores 48% (one names the Secret type, the other the
mount path) while the *duplicate* stylesheet pair scores 46%. No threshold divides them.
So `ctxlake claims --duplicates` reports the pairs, ranked, and leaves the judgement
where it can actually be made.

### Every tier is now inspectable

Tier 0 had no command at all — 51 session digests in the lake with no way to read one —
and `ClaimEvent::Retired` existed in the event log with no CLI path, so a duplicate could
be found and not acted on.

```sh
ctxlake sessions                                    # what the fleet has done
ctxlake sessions <id>                               # one session, in full
ctxlake claims --retire <claim_id> --reason "..."   # remove one from what agents read
```

`ctxlake sessions <id>` prints the repo and branch, duration, outcome, token usage and
cost, the friction signals, every file touched, and every command with its exit code —
then **the claims resting on that session**, by id. That last line is the join between
what happened and what the fleet concluded from it, and the fastest route to a claim
worth retiring.

A command whose exit code is unknown prints `?` rather than `ok`: a session captured
before transcript enrichment has no exit codes, and showing those as successes would
invent a result.

`--retire` appends a `Retired` event rather than deleting. The claim stops being
agent-visible at the next maintenance pass; the record of having believed it, and the
reason for stopping, stay in `claims/events/`. A claim that vanished without trace would
be indistinguishable from one that was never made — which is exactly the history you want
when the same wrong belief comes back. `--reason` is required for that reason.

### Three bugs the new commands found on a real lake

- **`ctxlake claims --status promoted` reported an empty lake against 117 promoted
  claims.** It read only the `claims.json` mirror, which the maintenance chain stopped
  writing once the snapshot gained a `claims` table — the same orphaned-reader shape as
  `history.json` in v0.1.9, where every reference was a reader. It now reads the
  snapshot, the same artifact the agent reads, with the mirror as a fallback for caches
  written by an older ctxlake.
- **`--duplicates` printed claim ids that `--retire` could not resolve.** Claim ids are
  ULIDs, so claims minted in the same millisecond share a long prefix: eight characters
  matched 94 claims. Ids now print at twelve characters everywhere, from one constant.
- **Friction rendered as raw JSON.** It now decodes into the digest's own `Friction`
  type and calls the same `headline()` the briefing uses, so there is one renderer to
  keep correct instead of two.

### Also

- Session digest columns decode into the types the maintenance chain wrote them from. A
  shape this reader cannot parse costs that column and nothing else, so a digest written
  by a newer ctxlake cannot make `sessions` unusable on an older one.
- The `convention` promotion threshold drops from 2 independent sessions to 1. This is a
  concession, documented as one: `injected_context` is still never populated, so
  `independent_count` can never exceed 1 and a threshold of 2 meant `convention` could
  never promote at all. Restore it to 2 once independence lineage is real.
- Test fixtures can now express a claim's evidence and a session's files, commands and
  friction. Without those columns no test could exercise a reader that renders them —
  the same gap that let the v0.1.9 capture bug survive review.

## v0.1.9

The memory layer produced nothing and delivered nothing. Measured on a live lake
before this release: 26 sessions sealed, 26 digests, **0 claims**, and a briefing
containing three agent names. `docs/memory.md` advertised seven Tier 0 outputs and
delivered one.

Five independent breaks, in a chain where **every component passed its own tests** —
each was tested against fixtures that populated the fields the next stage read.

> **Upgrading:** run `ctxlake update`, then **`ctxlake install <runtime>` on every
> host**, even where hooks are already current — that step now also registers the MCP
> server, which was never wired anywhere. Flip `[summarize] mode` from `shadow` on all
> hosts together: the flag is baked into the published snapshot, so a host left on
> `shadow` serves the fleet a withheld one.

### Capture could not see what the digest reads

No hook payload carries token usage, git branch, or an exit code — and 842 real
envelopes proved none carried tool output under the key the adapter read. The fixture
that "verified" that key was hand-written from documentation, so fixture and code agreed
with each other and disagreed with the runtime.

Fixed by reading the **session transcript** instead of chasing a field name. It carries
tool output, the `is_error` failure flag, `message.usage`, `gitBranch`, and —
decisively — `bashEditDiff`, the file edits made through a shell command, which are most
real edits and which no hook can attribute. The join key already existed: the hook writes
`message_id` from the runtime's tool-use id.

### Digests reached nobody

`fleet_history` read a `history.json` that **nothing in the repository ever wrote**.
Every reference to it was a reader or a test. The snapshot now carries a `sessions`
table, and `fleet_history` reads it — deleting the orphan rather than inventing a
producer.

### An agent could be told things but not ask

`ctxlake mcp` served six working tools and `install` never registered it. `mcpServers`
was empty on a live machine while 38 promoted claims sat in the lake. `install` now wires
it, merged into `~/.claude.json` — 112 KB of live state — with everything outside
`mcpServers.ctxlake` verified untouched: 2,298 leaf values before, 2,302 after, 0 lost.

### Extraction was unmeasurable

A successful `{"claims": []}` marked a session done forever, so no prompt or parser fix
could be evaluated against existing history. Markers now record an extractor version and
are fleet-scoped. `0 claim(s) proposed` no longer means both "found nothing" and
"discarded everything" — the summary reports kept versus returned.

Three rounds of real failures then showed the model was right each time: it was being
sent empty transcripts. Sessions that render to a header, to a page of bare id markers,
or to a single prompt reading `ok` are now skipped before the call rather than paid for.

### The five claim types had no definitions

Every promoted claim came back `outcome` — episodic, restating what the digest already
records. The prompt named the types in a schema line and defined none of them. With
definitions, examples, and a warning that an `outcome` restating the digest is duplicate
noise, `convention` became the plurality of new claims. Claims also now carry the session
they came from, so a memory can be followed rather than only read.

### `convention` promotion, and an honest concession

The independence threshold for `convention` drops from 2 to 1. Two is the right bar, but
it is compared against a count that subtracts sessions carrying `injected_context` — a
field **nothing populates** — so it was unreachable, and all 41 conventions on a live
fleet sat as permanent candidates. An unreachable gate discards a category rather than
protecting it. The contradiction gate, provenance gate, quarantine switch and
attribution-on-read all still apply. Restore the 2 once `injected_context` is populated;
both echo-case tests and the threshold's own doc say so in place.

### Other fixes

- `SubagentStop` was wired by the installer and rejected by the adapter — 39 events
  dropped to a log nothing reads. A test now calls the adapter with every wired event.
- One session's extraction failure ended the whole maintenance chain, skipping
  compaction, digests, the gate and the snapshot.
- The snapshot pointer is fleet-scoped; two fleets publishing in turn each served the
  other's artifact half the time.
- Digests are recomputed on a schema bump instead of skipped on existence.
- `MAX_FIELD_BYTES` 32 KiB → 1 MiB on the hook path (measured: the cap was never the
  binding cost; process spawn is).

## v0.1.8

### `sync install` did not replace a running daemon on Linux

Upgrading a Linux host left the *old* daemon running. `install` rewrote the unit,
reported success, and the previous process kept going — with the previous binary,
which the installer had already replaced on disk. On a real host mid-upgrade:
`/proc/<pid>/exe` pointed at a path marked `(deleted)`, `ctxlake sync status` said
`active`, and the machine spent an hour writing to a `live/` layout the rest of the
fleet had moved off in v0.1.7.

The cause was one word. `install` ran `systemctl --user enable --now`, and `--now`
means *start* — `systemctl start` against an already-active unit does nothing. It now
runs `enable` and `restart` separately: `restart` starts a stopped unit and replaces a
running one, which is what "install this and run it" has to mean. `--no-start` still
enables without starting.

macOS was never affected: launchd's `bootout` + `bootstrap` genuinely replaces the
process, which is why the same upgrade worked there and silently did not on Linux.

**If you are on v0.1.7 on a Linux host, run `systemctl --user restart ctxlake-sync`
once** — the unit is already correct, only the process is stale. From v0.1.8 on,
`sync install` and `ctxlake update` both handle it.

## v0.1.7

> **Upgrading: run `ctxlake sync install` once on every host after updating.**
>
> This release partitions `live/` by fleet, so every host has to be on it before the
> roster means anything — a host still on v0.1.6 writes to the old location and is
> invisible to upgraded ones, and vice versa. Nothing is lost either way: `live/` is
> disposable state that rebuilds within one heartbeat.
>
> `ctxlake sync install` is also what pins `PATH` into the unit, which is what makes
> `provider = "claude-cli"` and `provider = "ollama"` resolvable to a daemon. From
> v0.1.7 on, `ctxlake update` re-renders the unit itself and this step disappears.
>
> Once every host is upgraded, `ctxlake maint --prune --dry-run` shows the old `live/`
> keys, and without the flag removes them.


### `--fleet` was not actually a boundary

`docs/getting-started.md` calls `--fleet` "the boundary of who sees whom". It was a
label. `live/agents/<agent_id>.json` and `live/roster.json` were flat, and
`roster::build` took no fleet id at all — so on a bucket holding two fleets:

- **`ctxlake status` reported another fleet's agents as its own.** Seen on a live lake:
  "fleet oxidantdata-dev · 3 agent(s) active", one of which belonged to a fleet called
  `demo`. It reached the briefing too, so another fleet's agents were being described
  into agents' context windows.
- **Two fleets using the same `agent_id` shared a key**, silently overwriting each
  other's presence. Not a display bug but a data one.

`live/` is now partitioned: `live/fleets/<fleet_id>/agents/<agent_id>.json` and
`live/fleets/<fleet_id>/roster.json`. The build filters on the record's own `fleet_id`
as well as the prefix, so an object mis-keyed by an older build or a restored backup
cannot reintroduce the leak. Scoping also makes the listing cheaper — the roster
enumerates one fleet rather than the whole bucket, which is what `docs/storage.md`'s
O(N) fan-in arithmetic assumed all along.

### Presence never expired

`docs/how-it-works.md` said an agent that stops writing "simply ages out". Nothing
implemented that: on a live lake a host that had been off for **226 minutes** was still
listed as active. The roster now drops an intent whose `updated_at` is older than a
5-minute TTL — five missed heartbeats. A timestamp in the *future* is kept rather than
expired: there is no shared clock here, and making a live agent invisible to collision
checks is the more expensive of the two mistakes.

### `init --force` left the identity it replaced behind

Renaming a host wrote a new `live/` record and abandoned the old one, written by nobody
from then on. `init` now retires the previous identity when `--agent-id` or `--fleet`
changes, and leaves it alone when they do not — `--force` is also how people change a
model or a store URL, and deleting this host's own record on every such run would blank
it from the fleet until the next heartbeat.

### `ctxlake maint --prune`

Nothing ever deleted a superseded `snapshot/<hash>.sqlite`, so a fleet republishing
every five minutes accumulated one per change forever. `--prune` removes those (never
the one `latest.json` points at, and never one younger than 24h — a reader that just
resolved the pointer is about to fetch the blob it named) and the pre-fleet-scoping
`live/` keys. Sessions, claim events, digests and compaction generations are never
touched. `--dry-run` reports exactly what would go.

Separate from the chain rather than a step in it: the chain only ever adds, which is
what makes running it everywhere at once safe, and deleting should be something you ask
for rather than something a five-minute timer does.

### The identity guard cried wolf on its own placeholder

`check_no_real_identities` derives its tokens from the machine so that it protects
whoever runs it. The cost showed up here: on a Mac still named `MacBook-Pro` it matched
the docs' own anonymous placeholder, `Alices-MacBook-Pro`, and failed a clean tree.
Device-model words are now stripped from a hostname before it becomes a token, and a
token made of nothing else is dropped. Verified still catching a planted real name.


### An unusable model stopped the entire maintenance chain

v0.1.6 made an unreachable Tier 2 provider non-fatal at daemon *startup* and left the
same abort inside the cycle itself, which is arguably worse: the daemon comes up, `sync
status` reports it healthy, and compaction, digests, the gates and the snapshot never
run. Found on a live lake — the snapshot was three and a half hours stale on a bucket
that had been receiving sessions the whole time, and the only symptom was one log line
about a provider.

None of those four steps needs a model. A cycle now builds what it can, runs everything
else, and names the failure in the summary line the daemon prints each pass —
`extraction UNAVAILABLE (<reason>)`, distinct from the `extraction disabled` a fleet with
Tier 2 switched off reports. Loud was the half of the old behaviour worth keeping;
failing the cycle was not.

### `ctxlake update` re-renders the service unit

Without this, every future upgrade would need `ctxlake sync install` run by hand
afterwards — the exact "remember to do a second thing" step v0.1.6 removed from
maintenance scheduling, reintroduced one layer down. A unit goes stale on an upgrade in
two invisible ways: it names a binary path a package manager just deleted, and it pins a
`PATH` from before a provider binary existed. `update` now refreshes it from the config
path the unit itself records, then restarts. Re-rendering an unchanged unit writes
nothing.

Deliberately without re-running the pre-install checks: refusing to refresh an
already-installed unit because a store is briefly unreachable would leave it pointing at
a deleted binary, which is worse than either outcome those checks protect against.

## v0.1.6

The daemon did not run on either machine it was installed on. All four causes were
the same mistake, made in four places: **a supervised service does not inherit your
shell**, and every check that was supposed to catch that ran inside one.

### The daemon crash-looped, and nothing said why

`provider = "claude-cli"` resolves in the terminal that runs `ctxlake sync install` and
not under launchd, whose `PATH` is `/usr/bin:/bin:/usr/sbin:/sbin`. The daemon treated
an unbuildable Tier 2 provider as fatal, so it exited 1 before starting a single loop,
the supervisor restarted it, and it exited 1 again — fourteen times on macOS. `ctxlake
sync status` reported `spawn scheduled` / `activating` and `not running`, which is a
description rather than a diagnosis.

Four changes, each independently mutation-tested:

- **The unit pins its `PATH`**, taken from the process that installed it, so the
  pre-install check and the daemon resolve the same binaries. It already pinned `HOME`
  for exactly this reason; `PATH` was the same lesson, unlearned.
- **An unreachable model no longer stops the daemon.** Capture shipping, cache refresh
  and presence have nothing to do with a model, and taking all three down because an
  *optional* summarizer is unavailable inverts their importance — in the one mode where
  the user can least see why. It now warns, starts everything, and rebuilds the provider
  on each maintenance cycle, so a key that comes back heals without a restart.
- **`ctxlake sync status` prints why**, tailing the daemon's own output when a service
  is installed and nothing is running. The reason was always in a file; nothing said the
  file existed.
- **The unit names a stable binary path.** `exec_path` canonicalized, which on Homebrew
  pins `/opt/homebrew/Cellar/ctxlake/<version>/bin/ctxlake` — a directory `brew upgrade`
  deletes. Every upgrade would have broken the service permanently.

### Provider keys never reached the daemon either

The same hole, one level up: `export OPENROUTER_API_KEY=...` is invisible to launchd and
to a systemd user unit, so `doctor` reported green in a terminal and Tier 2 failed on
every cycle. The key cannot go in `ctxlake.toml` (which holds only the *name* of a
variable) or in the unit file (world-readable on both platforms).

`~/.config/ctxlake/env` is the third place: loaded by every `ctxlake` process at startup,
**refused unless it is mode 600**, and never overriding a variable that is already set.
`ctxlake doctor` now checks whether the daemon will see the key rather than whether this
shell does, and prints the two commands that fix it.

### `ctxlake update`

One command on every platform, replacing a table of four platform-specific incantations.
It infers how this copy was installed from where the binary lives: Homebrew and cargo
installs are handed back to the tool that owns them — writing into a Cellar desynchronizes
brew's manifest — and anything else is downloaded, **verified against `SHA256SUMS`**, and
replaced by rename rather than in place, because `ctxlake-hook` is executing in live agent
sessions while the update runs. The sync daemon is restarted afterwards, since a running
one holds the old binary open. `--check` reports and changes nothing.

### Docs

Per-platform tabs for install, store backend, model provider and daemon setup, so each
page shows one path instead of all of them. The daemon section is rewritten in plainer
language, an "If the daemon is not running" section covers the failures above, and the
stale claim that maintenance needs a cron entry is gone — it has run inside the daemon
since v0.1.4. Link text is now page titles rather than filenames.

Two new regression checks, both of which found a real defect on their first run: one
asserts the service templates pin what the docs promise they pin, the other that every
command in the reference exists and every command that exists is in the reference
(`ctxlake briefing` was undocumented).

## v0.1.5

Fixes for everything the first real installs turned up. If you are on v0.1.4,
upgrade — `ctxlake sync install` refuses to run on it whenever a keyless model
provider is configured.

### `sync install` refused a correctly configured host

`claude-cli` uses the subscription its binary is signed in to, and `ollama` is a
local endpoint, so neither has an `api_key_env`. `doctor` checked the empty
variable anyway, concluded the key was missing, and blocked the install with
`[summarize.batch] is configured but  is not set` — a blank where the variable
name belongs. Keyless providers now go straight to the live probe.

### `doctor` warned that shadow mode was not enforced. It is.

That note was written when `ctxlake-maint` was an empty scaffold and
`memory_search` consulted no mode, and it outlived both — so an operator reading
their own `doctor` output would reasonably conclude a safety property they had
been promised was not in effect.

Enforcement is at the publish point, which is stronger than a read-time check:
`snapshot::publish` builds `claims_fts` — the only index `memory_search` queries —
from rows marked `visible_to_agents`, and shadow marks none. The claims are not
there to serve, rather than present and skipped by a reader who has to remember.

### Every macOS host defaulted to the same agent id

`ctxlake init` produced `agent_id: unnamed-agent` on macOS, where `HOSTNAME` is
not exported, `COMPUTERNAME` is a Windows convention and there is no
`/etc/hostname`. `agent_id` is a host's identity in its fleet, so two hosts
sharing one merge into a single roster entry, have their claims attributed to the
same author, and let the independence gate count two separate observations as one
agent reporting twice. It now falls back to `hostname -s`.

`--agent-id` is documented in getting-started, along with what it and `--fleet`
each decide.

### Also

- A real person's machine name reached the public docs as an example. Replaced,
  and a check now fails the build on identities derived from the machine it runs
  on, so no name has to be written down to be guarded against.
- Published release artifacts are immutable. The workflow fires twice for one tag
  (a tag push and an explicit dispatch), Rust builds are not byte-reproducible, and
  the second run used to `--clobber` the first — which broke `brew install` for
  v0.1.4 with a checksum mismatch on a release that looked complete and green.

## v0.1.4

`ctxlake init` configures the model, Tier 2 can run on a Claude Code subscription
with no API key, and two bugs from v0.1.3.

### `init --llm` sets up extraction, and checks it

Configuring a model meant hand-editing `ctxlake.toml` after running the one command
whose job is writing that file — and the mistakes showed up later, in a daemon log.

```sh
ctxlake init --store s3://bucket/ctxlake --fleet myteam --llm claude-cli
ctxlake init --store s3://bucket/ctxlake --fleet myteam --llm openrouter
```

Picks a sensible default model and key variable per provider, and **makes a real
call before writing anything**. A model name that does not exist, a key that is
unset or revoked, an endpoint pointing at nothing: all refuse, and no config is
written. Override with `--llm-model`, `--llm-key-env`, `--llm-base-url`,
`--summarize-mode` (default `shadow`).

### `--llm claude-cli` — no API key

Anyone running ctxlake already runs a coding agent. For Claude Code users that
means a `claude` binary already signed in, so asking them to create an API key to
summarise their own sessions is a second bill and a second secret. This provider
shells out to `claude -p --output-format json`.

Verified end to end: a session captured through the real hook yielded 4 claims, 3
promoted by the gate, on the subscription alone. Needs `claude` on `PATH` and
**no** `ANTHROPIC_API_KEY` set — the CLI prefers a key over the subscription, so
`init` refuses rather than writing a config that 401s later.

### Fixes

- **Redaction over-matched.** `Bearer `, `hf_`, `npm_` and `SG.` were added as bare
  literals in v0.1.3 and quarantined ordinary English and ordinary code — "the API
  wants a Bearer token", `let hf_size = 32;`, `npm_config_prefix`, and `MSG.`
  (which contains `SG.`). They now require a credential-shaped token after the
  prefix, and redact just that token rather than withholding the whole value.
- **The docs site had not updated in 12 hours.** The deploy invalidated `/docs/*`
  while the site is served at the root, so CloudFront kept serving stale pages —
  ones still documenting a command removed that morning. Every deploy reported
  success throughout.

## v0.1.3

A redaction audit, and the Homebrew install path that never worked.

### Redaction caught less than it claimed

Redaction always ran in the right place — inside the hook, before the spool, on
every path including import. Probing it with real inputs rather than reading its
documentation found that several things went to the lake untouched:

- **`.env` was not on the denylist.** Only `/.hermes/.env` was, while the docs
  said `.env` reads were dropped entirely. Unless a `.env` happened to contain a
  recognisable marker like `AKIA`, it was captured in full — and most hold things
  like `DATABASE_PASSWORD=hunter2`, which no marker matches.
- Database URLs with inline passwords (`postgres://admin:hunter2@…`), Slack and
  Discord webhooks, Groq / HuggingFace / npm / xAI / SendGrid / DigitalOcean /
  Shopify tokens, Google OAuth tokens, bare `Bearer` headers, `PGPASSWORD`,
  **credit card numbers** and **US SSNs**.
- Missing deny paths: `.git-credentials`, `*.pem` / `*.key` / `*.p12`,
  `terraform.tfstate`, `~/.gnupg/`, gcloud ADC, `~/.azure/`.

All covered now, each with a test. There are two kinds of rule: a **marker** says
a credential is nearby but not where it ends, so it withholds the whole value; a
**structural** match knows its extent exactly, so it replaces just that span and
leaves the rest readable. Dropping a 5,000-line log because one line held a card
number teaches people to switch redaction off.

False positives are tested as carefully as true ones — git SHAs, order numbers,
epoch milliseconds, phone numbers, zip+4 and database URLs with no credential all
stay clean. Card detection is Luhn-gated; SSNs must be in dashed form.

Cost: +0.10ms per scrub and +0.18ms once per process, about 5% of the hook's 5ms
budget.

### `brew install oxidantdata/tap/ctxlake` works

It never had. The release workflow rendered `ctxlake.rb` and attached it, but
copying it into the tap was documented as a manual step and was never performed
for any release — so the command failed while the docs advertised it. The tap now
pulls each project's formula from its latest release on a schedule, needing no
cross-repo token, which was the original objection to automating it.

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
