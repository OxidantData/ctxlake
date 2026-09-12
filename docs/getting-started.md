# Getting started

From nothing to an agent session that opens knowing what the rest of the fleet is doing.

> **Status: pre-alpha.** Commands and flags will change. Nothing here changes your
> agents' behaviour without you asking, and `ctxlake uninstall` is exact.

You need a bucket (S3, GCS, MinIO, R2 — or a local directory to try it), at least one of
Claude Code, Cursor Agent CLI or Hermes, and credentials your machine already resolves via
the standard chain. ctxlake manages no cloud credentials of its own.

## 1. Install

```sh
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/OxidantData/ctxlake/main/packaging/install.sh | sh

brew install oxidantdata/tap/ctxlake        # macOS

cargo install --git https://github.com/OxidantData/ctxlake ctxlake-cli   # from source
cargo install --git https://github.com/OxidantData/ctxlake ctxlake-hook
```

Two binaries land: `ctxlake` and `ctxlake-hook`. They are separate because the hook fires
on every tool call under a 5ms budget and links no network stack at all — see
[architecture.md](architecture.md). Set `CTXLAKE_INSTALL_DIR` to choose where the script
puts them; it defaults to `~/.local/bin` and tells you if that is not on your `PATH`.

Prebuilt `.tar.xz` archives for `{aarch64,x86_64}-apple-darwin` and
`{x86_64,aarch64}-unknown-linux-gnu` are on the
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

## 6. Configure a model — or decide not to

Everything so far runs with no model and no API key: capture, compaction, session
history, friction digests, briefings. **A model buys exactly one thing** — durable
*claims* extracted across sessions. Skip this step and the rest still works.

`init` sets it up and **verifies it with a real call before writing anything**:

```sh
# already have Claude Code? use its subscription — no API key at all
ctxlake init --store s3://my-bucket/ctxlake --fleet myteam --llm claude-cli

# or a provider key
export OPENROUTER_API_KEY=...
ctxlake init --store s3://my-bucket/ctxlake --fleet myteam --llm openrouter
```

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
decided. See [memory.md](memory.md).

> **`claude-cli` and `ANTHROPIC_API_KEY` don't mix.** The CLI prefers the key over the
> subscription, so a stale one makes every call fail with a 401 that says nothing about
> ctxlake. `init` refuses rather than writing a config that will fail later.

## 7. Check the host before you install anything

```sh
export OPENROUTER_API_KEY=...
ctxlake doctor
```

```text
store   s3://my-bucket/ctxlake   (region us-west-2)
  put-if-absent / compare-and-swap / conflict detection / conditional GET / list   ok

llm     OPENROUTER_API_KEY resolves, provider answered
```

**`doctor` calls the provider for real**, with a one-word request. That matters because a
revoked key, a typo'd model name, a base URL pointing at nothing and an account over its
quota all resolve an environment variable perfectly well — and then fail hours later,
inside a maintenance log, looking exactly like "extraction found nothing worth claiming."

## 8. Install the daemon

```sh
ctxlake sync install       # systemd user unit (Linux) or LaunchAgent (macOS)
ctxlake sync status
```

One process does all of it: spool → store, store → the local cache your hooks read, this
agent's heartbeat, **and the maintenance chain** — compaction, digests, extraction, the
gates, the snapshot. There is no cron entry to add and no timer to schedule.

`install` re-runs the step 7 checks and **refuses** if the store is unreachable or a
configured model cannot be called, because a supervised daemon that cannot do its job
comes up `active` and achieves nothing — and capture keeps working regardless, so the
first symptom is a briefing going stale days later with nothing in `systemctl status` to
explain it. `--skip-checks` overrides it for an air-gapped host.

> **Linux: run `sudo loginctl enable-linger $USER` too**, or systemd stops your user
> manager at logout and the daemon does not return after a reboot. `install` and `status`
> both tell you whether it is set.

`ctxlake init --daemon` does steps 2 through 8 in one command, with the same checks.

## 9. See the fleet

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
[how-it-works.md](how-it-works.md#one-prompt-end-to-end).

> **Tier 1 is not wired yet.** No hook fires the turn-end nudge today, so
> `ctxlake doctor` reporting `tier 1 nudges fired: 0` is expected, not a fault in your
> install. Tier 0 digests and Tier 2 extraction both work.

## Next steps

- [how-it-works.md](how-it-works.md) — the three planes, in one picture
- [reference.md](reference.md) — every command, flag and config key
- [adding-it.md](adding-it.md) — how this fits alongside what you already run
