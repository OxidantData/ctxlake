# Getting started

From nothing to an agent session that opens knowing what the rest of the fleet is doing.

> **Status: pre-alpha.** Commands and flags will change. Nothing here changes your
> agents' behaviour without you asking, and `ctxlake uninstall` is exact.

You need a bucket (S3, GCS, MinIO, R2 — or a local directory to try it), at least one of
Claude Code, Cursor Agent CLI or Hermes, and credentials your machine already resolves via
the standard chain. ctxlake manages no cloud credentials of its own.

## 1. Install

::: code-group

```sh [macOS]
brew install oxidantdata/tap/ctxlake
```

```sh [Linux]
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/OxidantData/ctxlake/main/packaging/install.sh | sh
```

```sh [From source]
cargo install --git https://github.com/OxidantData/ctxlake ctxlake-cli
cargo install --git https://github.com/OxidantData/ctxlake ctxlake-hook
```

:::

The installer script works on macOS too, and Linux users on Homebrew can use the
formula — the tabs are what most people on each platform want, not a restriction.

Two binaries land: `ctxlake` and `ctxlake-hook`. They are separate because the hook
fires on every tool call under a 5ms budget and links no network stack at all — see
[Architecture](architecture.md). Set `CTXLAKE_INSTALL_DIR` to choose where the script
puts them; it defaults to `~/.local/bin` and tells you if that is not on your `PATH`.

Prebuilt `.tar.xz` archives for `{aarch64,x86_64}-apple-darwin` and
`{x86_64,aarch64}-unknown-linux-gnu` are on the
[Releases page](https://github.com/OxidantData/ctxlake/releases).

### Updating later

```sh
ctxlake update
```

One command on every platform. It works out how this copy was installed and does the
right thing: Homebrew and cargo installs are handed back to the tool that owns them, and
a curl-installed binary is replaced in place after its checksum is verified against the
release. Both binaries are updated together, and the sync daemon is restarted afterwards
if you have one — a running daemon holds the old binary open, so without that it would
keep running the version you just replaced.

`ctxlake update --check` reports whether a newer release exists and changes nothing.

## 2. Point it at a store

::: code-group

```sh [S3]
ctxlake init --store s3://my-bucket/ctxlake --fleet myteam --agent-id cc-01
```

```sh [GCS]
ctxlake init --store gs://my-bucket/ctxlake --fleet myteam --agent-id cc-01
```

```sh [MinIO / R2]
export AWS_ENDPOINT=https://minio.internal:9000
ctxlake init --store s3://my-bucket/ctxlake --fleet myteam --agent-id cc-01
```

```sh [Local (try it)]
ctxlake init --store file://~/ctxlake-demo --fleet local --agent-id cc-01
```

:::

| Flag | What it decides |
|---|---|
| `--fleet` | **The boundary of who sees whom.** Everyone sharing it sees each other's roster and briefings. Map it to a team genuinely collaborating, not to a company |
| `--agent-id` | **This machine's identity in that fleet.** Defaults to the hostname |

> **Give every machine a distinct `--agent-id`.** Two hosts sharing one merge into a
> single roster entry, and their claims are attributed to the same agent — which also
> makes the independence gate treat two separate observations as one. The default is
> derived from the hostname, so collisions are unlikely — but a hostname of `Mac` or
> `localhost` is worth replacing with something you would recognise in
> `ctxlake status`, like `cc-01` or `build-runner-2`. Change it later with
> `--agent-id <name> --force`.

The `file://` form runs roster and briefings fine; what you lose is a second machine
joining, so use it to evaluate, not to run a fleet.

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
run it first, not after. See [Storage](storage.md).

## 4. Backfill what you already have

```sh
ctxlake import --all --since 90d
```

Your runtimes have been recording all along, so the first briefing already knows your
repos. Fidelity differs per runtime and redaction runs here too — see
[Adding it](adding-it.md).

## 5. Wire up your agents

```sh
ctxlake install --all          # every runtime doctor found
ctxlake install claude-code    # or one at a time
```

Installs **merge**: existing hooks are preserved, a `.bak` is written first, and
re-running changes nothing. Preview with `--dry-run`; reverse with `ctxlake uninstall`.

## 6. Configure a model — or decide not to

Everything so far runs with no model and no API key: capture, compaction, session
history, friction digests, briefings. **A model buys exactly one thing** — durable
*claims* extracted across sessions. Skip this step and the rest still works.

`init` sets it up and **verifies it with a real call before writing anything**:

::: code-group

```sh [Claude subscription]
# no API key at all — uses the `claude` binary you already signed in to
ctxlake init --store s3://my-bucket/ctxlake --fleet myteam --llm claude-cli --force
```

```sh [OpenRouter]
export OPENROUTER_API_KEY=...
ctxlake init --store s3://my-bucket/ctxlake --fleet myteam --llm openrouter --force
```

```sh [Anthropic]
export ANTHROPIC_API_KEY=...
ctxlake init --store s3://my-bucket/ctxlake --fleet myteam --llm anthropic --force
```

```sh [Local (Ollama)]
# nothing leaves the machine
ctxlake init --store s3://my-bucket/ctxlake --fleet myteam --llm ollama --force
```

:::

`--force` because you already ran `init` in step 2 and this rewrites that config.

| `--llm` | Key needed | Default model |
|---|---|---|
| `claude-cli` | **none** — uses the `claude` binary's own subscription | `haiku` |
| `anthropic` | `ANTHROPIC_API_KEY` | `claude-haiku-4-5` |
| `openrouter` | `OPENROUTER_API_KEY` | `anthropic/claude-haiku-4.5` |
| `gemini` | `GEMINI_API_KEY` | `gemini-2.0-flash` |
| `openai-compatible` | `OPENAI_API_KEY` + `--llm-base-url` | `gpt-4o-mini` |
| `ollama` | none — local, nothing leaves the machine | `llama3.1` |

Override any of it with `--llm-model`, `--llm-key-env`, `--llm-base-url`. The config
stores the **name** of the environment variable, never a key.

The default is `--summarize-mode shadow`: claims accumulate and the gates report, but
nothing reaches a context window until you have read a couple of weeks of them and
decided. See [Memory](memory.md).

> **`claude-cli` and `ANTHROPIC_API_KEY` don't mix.** The CLI prefers the key over the
> subscription, so a stale one makes every call fail with a 401 that says nothing about
> ctxlake. `init` refuses rather than writing a config that will fail later.

## 7. Make the key reachable by the daemon

Skip this if you chose `claude-cli` or `ollama` in step 6 — neither needs a key.

An `export` in your terminal is not visible to a background service. launchd and systemd
start their jobs with a near-empty environment: no exports, no `.zshrc`, no `.bashrc`. So
a key that works perfectly when you run `ctxlake doctor` by hand is simply absent when
the daemon runs, and extraction fails on every cycle for a reason no check in your
terminal can see.

Put it in `~/.config/ctxlake/env`, which both can read:

```sh
mkdir -p ~/.config/ctxlake
touch ~/.config/ctxlake/env && chmod 600 ~/.config/ctxlake/env
echo "OPENROUTER_API_KEY=$(printenv OPENROUTER_API_KEY)" >> ~/.config/ctxlake/env
```

`chmod 600` is required, not advice — ctxlake refuses to read this file if other users
on the machine can, and says so. The key never goes in `ctxlake.toml`, which only ever
holds the *name* of a variable.

`ctxlake doctor` checks for this specifically and prints the two commands above if your
key is only in your shell.

## 8. Check the host before installing anything

```sh
ctxlake doctor
```

```text
store   s3://my-bucket/ctxlake   (region us-west-2)
  put-if-absent / compare-and-swap / conflict detection / conditional GET / list   ok

llm     OPENROUTER_API_KEY — provider answered
```

**`doctor` calls the provider for real**, with a one-word request. That matters because a
revoked key, a typo'd model name, a base URL pointing at nothing and an account over its
quota all resolve an environment variable perfectly well — and then fail hours later,
inside a maintenance log, looking exactly like "extraction found nothing worth claiming."

## 9. Install the daemon

::: code-group

```sh [macOS]
ctxlake sync install
ctxlake sync status
```

```sh [Linux]
ctxlake sync install
sudo loginctl enable-linger $USER   # or the daemon will not come back after a reboot
ctxlake sync status
```

:::

That is the whole setup. One background process handles everything: it ships what your
hooks captured up to the store, pulls the fleet's state back down into the local cache
your hooks read, keeps this agent visible to the others, and runs the maintenance chain.
**Nothing else to schedule** — no cron entry, no systemd timer.

On macOS it installs a LaunchAgent; on Linux, a systemd user unit. Both start at login
and restart on crash.

> **Linux only: `loginctl enable-linger`.** Without it systemd shuts your user session
> down at logout and the daemon does not come back after a reboot. `ctxlake sync install`
> and `ctxlake sync status` both tell you whether it is set.

`install` re-runs the step 8 checks first and **refuses if the store is unreachable**,
because a daemon that cannot reach the store comes up looking healthy and does nothing.
Use `--skip-checks` on an air-gapped host. A model that cannot be reached is a warning
rather than a refusal: capture, the cache and presence do not depend on one, and the
daemon retries the model on every cycle.

Shortcut: **`ctxlake init --daemon`** does steps 2 through 9 in one command.

## 10. See the fleet

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

Start a session in that repo and it opens with the same information in context. How a
prompt becomes that context is one diagram in
[How it works](how-it-works.md#one-prompt-end-to-end).

> **Tier 1 is not wired yet.** No hook fires the turn-end nudge today, so
> `ctxlake doctor` reporting `tier 1 nudges fired: 0` is expected, not a fault in your
> install. Tier 0 digests and Tier 2 extraction both work.

## If the daemon is not running

```sh
ctxlake sync status
```

When a service is installed but no daemon is running, `status` prints the last lines of
the daemon's own output — that is almost always the answer:

```text
service:  installed — ~/Library/LaunchAgents/com.oxidantdata.ctxlake-sync.plist
state:    spawn scheduled
ctxlake sync is not running

why:      the service is installed but no daemon is running.
          last output — ~/.ctxlake/run/sync.err.log:
            Error: resolving the Tier 2 provider: `claude` is not on PATH
          `ctxlake doctor` checks the things that usually cause this.
```

Two causes account for nearly all of these, and both are the same mistake — the daemon
does not inherit your shell:

| What you see | Cause | Fix |
|---|---|---|
| `` `claude` is not on PATH `` | The service was installed before v0.1.6, which pins the PATH into the unit | `ctxlake sync install` again |
| Tier 2 failing every cycle, key looks fine | The key is exported in your shell only | Step 7 — put it in `~/.config/ctxlake/env` |

After a `brew upgrade` from a version before v0.1.6, run `ctxlake sync install` once
more: units written by older versions point at a versioned Homebrew path that the
upgrade deleted. `ctxlake update` handles this for you going forward.

## Next steps

- [How it works](how-it-works.md) — the three planes, in one picture
- [Reference](reference.md) — every command, flag and config key
- [Adding it](adding-it.md) — how this fits alongside what you already run
