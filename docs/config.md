# config — `ctxlake.toml` reference

`ctxlake init` writes this file; `ctxlake config` prints the resolved result; every
other subcommand reads it. Default location:
`$XDG_CONFIG_HOME/ctxlake/ctxlake.toml`, or `~/.config/ctxlake/ctxlake.toml` if
`XDG_CONFIG_HOME` is unset. Every subcommand accepts `--config <path>` to point
elsewhere.

**Nothing in this file can ever be a secret *value*** — only the *name* of an
environment variable that resolves to one. `ctxlake doctor` reports whether a named
variable resolves, never what it resolves to.

## The full shape

```toml
store             = "s3://my-bucket/ctxlake"
fleet_id          = "myteam"
agent_id          = "cc-01"
collision_policy  = "warn"          # "warn" | "block"   (default: warn)

[summarize]
mode = "agent"                      # "none" | "agent" | "batch" | "both" | "shadow"
                                     # (default: agent)

[summarize.batch]                   # only read when mode is batch, both, or shadow
provider              = "anthropic" # anthropic | openai-compatible | ollama
model                 = "claude-haiku-4-5"
api_key_env           = "ANTHROPIC_API_KEY"  # the NAME of an env var, never the key
base_url              = ""                   # set for ollama or a self-hosted endpoint
use_batch_api         = true
max_sessions_per_run  = 50
max_input_tokens      = 8000
```

Every field below `agent_id` is optional and defaults exactly as shown — a minimal
file with just `store`, `fleet_id`, and `agent_id` is what `ctxlake init` writes when
you don't touch `[summarize]` at all.

## Top-level fields

| Field | Meaning |
|---|---|
| `store` — required | A URL: `s3://bucket/prefix`, `gs://bucket/prefix`, `az://container/prefix` (also `abfs://`/`abfss://`), or `file:///absolute/path` for local dev. Credentials are never part of this line — every backend but `file://` reads them the way its own SDK normally does. See [storage.md](storage.md). |
| `fleet_id` — required | The boundary of who sees whom. Everyone sharing a `fleet_id` sees each other's roster entries and briefings. Map this to a team genuinely collaborating on the same repos, not to an entire company. |
| `agent_id` — required | A stable, logical identity for this agent — not a hostname, and not something that should change across restarts. `ctxlake init --agent-id <id>` sets it explicitly; omitted, `init` derives a default from the hostname. |
| `collision_policy` — default `"warn"` | What happens when two agents' declared intents overlap (see [coordination.md](coordination.md)). `warn`: noted in the briefing and `ctxlake status`, nobody stopped. `block`: the hook's pre-tool-use check can additionally decline to let its own agent proceed. Either way this is advisory — it changes only what this agent's own hook does, it cannot stop another agent from writing. |

## `[summarize]`

The full three-tier model is [summarization.md](summarization.md)'s subject; this is
the config surface for it.

### `mode` — default `"agent"`

| Value | What runs | Needs an LLM key? |
|---|---|---|
| `none` | Tier 0 (structural digests) only | No |
| `agent` | Tiers 0 and 1 — the agent that did the work writes its own handoff | No |
| `batch` | Tier 0 and Tier 2 (batch claim extraction) | Yes |
| `both` | Tiers 0, 1, and 2 | Yes |
| `shadow` | Everything Tier 2 does, but promoted claims never reach a context window | Yes |

**`agent` is the default, and it needs no LLM API key at all.** Coordination, session
history, briefings, and Tier 0 digests all work identically regardless of this
setting.

### `[summarize.batch]` — only read when `mode` needs it

Ignored under `none` or `agent`. Required once `mode` is `batch`, `both`, or `shadow`.

| Field | Meaning | Default |
|---|---|---|
| `provider` | `anthropic`, `openai-compatible`, or `ollama` | — (required) |
| `model` | Model name for the batch provider | — (required) |
| `api_key_env` | **Name** of the environment variable holding the API key | — (required) |
| `base_url` | Override for a self-hosted or `ollama` endpoint | unset (provider default) |
| `use_batch_api` | Use the provider's batch API (half the price, results out of order) | `true` |
| `max_sessions_per_run` | Sessions extracted per maintenance run | `50` |
| `max_input_tokens` | Per-session input cap; oldest turns truncated first | `8000` |

Running entirely locally, so no transcript ever leaves the host:

```toml
[summarize.batch]
provider = "ollama"
model    = "qwen2.5:14b"
base_url = "http://localhost:11434"
api_key_env = "OLLAMA_API_KEY"   # unused by a local ollama endpoint, but still a
                                  # name, never a literal value, per the rule above
```

## What `ctxlake config` shows you

```sh
$ ctxlake config
store = "s3://my-bucket/ctxlake"
fleet_id = "myteam"
agent_id = "cc-01"
collision_policy = "warn"

# No field in this file can hold a secret value — only the NAME of an env
# var (see api_key_env above, when [summarize.batch] is set). `ctxlake
# doctor` reports whether that name resolves, never what it resolves to.
```

This is the resolved config, printed in full — there is nothing in this schema a
`***`-masking pass would need to hide.

## Next steps

- [cli.md](cli.md) — every subcommand that reads or writes this file
- [summarization.md](summarization.md) — the three tiers `[summarize]` selects between
- [storage.md](storage.md) — what `store`'s URL schemes mean per backend
- [coordination.md](coordination.md) — what `collision_policy` actually changes
