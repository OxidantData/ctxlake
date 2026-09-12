# config — `ctxlake.toml` reference

`ctxlake init` writes this file; `ctxlake config` prints the resolved result; every
other subcommand reads it. Default location:
`$XDG_CONFIG_HOME/ctxlake/ctxlake.toml`, or `~/.config/ctxlake/ctxlake.toml` if
`XDG_CONFIG_HOME` is unset. Every `ctxlake` subcommand accepts `--config <path>` to
point elsewhere — useful for running more than one agent identity from one host.

**The one rule that governs every field in this file:** nothing here can ever be a
secret *value* — only the *name* of an environment variable that resolves to one
(AGENTS.md invariant 10). There is no field anywhere in this schema shaped like a place
to type a key, and `ctxlake doctor` reports whether a named variable resolves, never
what it resolves to. If you find yourself wanting to put a real key in `ctxlake.toml`,
that is the signal something about the design changed underneath you — stop and
re-read this page.

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

Every field below `agent_id` is optional and defaults exactly as shown — a minimal file
with just `store`, `fleet_id`, and `agent_id` is valid, and is what `ctxlake init`
writes when you don't touch `[summarize]` at all.

## Top-level fields

### `store` — required

A URL: `s3://bucket/prefix`, `gs://bucket/prefix`, `az://container/prefix` (also
`abfs://`/`abfss://`), or `file:///absolute/path` for local dev and single-host use.
Parsed by [`ctxlake_store::backend::build`](../crates/ctxlake-store/src/backend.rs) —
see [storage.md](storage.md) for the full backend matrix and
[coordination.md](coordination.md) for what each write pattern needs from it.

Credentials are never part of this line or this file. Every backend but `file://` reads
them the way its own SDK normally does — environment variables, a profile, an instance
role. `ctxlake doctor` connects using exactly that resolution chain, so if `doctor`
can reach your bucket, so can everything else in this crate.

### `fleet_id` — required

The boundary of who sees whom. Every agent sharing a `fleet_id` sees each other's
roster entries, leases, and briefings; nothing crosses a fleet boundary. This should
map to a team that is genuinely collaborating on the same repos, not to an entire
company — see [getting-started.md](getting-started.md) for the reasoning.

### `agent_id` — required

A stable, logical identity for this agent — not a hostname, and not something that
should change across restarts (AGENTS.md's knob table: reusing one `agent_id` from two
different physical hosts merges their roster identity, which is a human-facing
confusion, not a correctness bug — CAS still prevents lost writes underneath it).

`ctxlake init --agent-id <id>` sets it explicitly. Omitted, `init` derives a default
from the hostname (`Alices-MacBook-Pro.local` becomes `alices-macbook-pro-local`) —
stable across re-running `init` on the same host, though nothing stops two different
hosts from sharing a hostname and thus this default; name it explicitly in that case.

### `collision_policy` — default `"warn"`

What happens when two agents' declared work overlaps. Leases are advisory either way
(AGENTS.md invariant 5) — this only controls how loudly ctxlake says so:

- **`warn`** — the default. A collision is noted in the briefing and in `ctxlake
  status`; nobody is stopped.
- **`block`** — the hook's pre-tool-use check can additionally decline to let its own
  agent proceed. Still advisory in the sense AGENTS.md means it: no backend here can
  reject a write from a process whose lease already expired, because there is no
  fencing primitive on the other end. `block` changes what *this agent's own hook*
  chooses to do with a collision it observes locally; it cannot make a stale holder's
  write actually fail. See [coordination.md](coordination.md)'s "what advisory leases
  do not promise" section before relying on this for anything irreversible.

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
history, briefings, and Tier 0 digests all work identically regardless of this setting
— summarization.md is explicit that turning it to `none` loses only the belief layer,
nothing else.

### `[summarize.batch]` — only read when `mode` needs it

Ignored entirely under `none` or `agent`. Required once `mode` is `batch`, `both`, or
`shadow` — `ctxlake doctor` will report the configured `api_key_env`'s resolution
status only when one of these three modes is active, since that's the only time a key
is actually needed.

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
# var (see api_key_env above, when [summarize.batch] is set). AGENTS.md
# invariant 10; `ctxlake doctor` reports whether that name resolves, never
# what it resolves to.
```

This is the resolved config, printed in full — there is no `***`-masking pass, because
there is nothing in this schema a mask would need to hide. If a future field ever looks
like it might carry a secret, that is a design bug to fix by removing the field, not by
teaching `ctxlake config` to redact it.

## Next steps

- [cli.md](cli.md) — every subcommand that reads or writes this file
- [summarization.md](summarization.md) — the three tiers `[summarize]` selects between
- [storage.md](storage.md) — what `store`'s URL schemes mean per backend
- [coordination.md](coordination.md) — what `collision_policy` actually changes
