# Troubleshooting — how to find out what is actually happening

[architecture.md](architecture.md) has the symptom-to-cause table: *"the briefing is
empty, here is what to check and why."* Start there when you have a symptom.

This page is the other half — how to gather evidence when you do not yet have one, or when
the table's first check was inconclusive. It deliberately does not repeat that table; two
copies of the same diagnostic advice drift apart, and then one of them is wrong.

## The first thing, always

```sh
ctxlake doctor
```

It executes each conditional-write primitive against your actual bucket rather than
assuming support, and reports which runtimes are wired. Most problems are visible here,
and the ones that are not are narrowed considerably.

> **`doctor` reporting hooks belonging to another tool is not a finding.** Coexistence is
> supported. ctxlake appends to your hook configuration and other consumers keep firing
> independently.

## The four places state actually lives

Almost every question reduces to looking at one of these. Nothing else is authoritative.

| What | Where | Read it when |
|---|---|---|
| Config | `~/.config/ctxlake/ctxlake.toml` | attribution is wrong, or the wrong store is in use |
| Spool | `~/.ctxlake/spool/<runtime>/<session_id>.ndjson` | asking "was this event captured at all" |
| Cache | `~/.ctxlake/cache/{briefing,roster}.json` | the briefing is empty, stale, or missing peers |
| Hook errors | `~/.ctxlake/hook-errors.log` | a hook is misbehaving but the session looks fine |

That last one matters more than it looks. **The hook never fails a turn** — on any
internal error it still emits a valid response and exits zero, because a capture tool that
can break your agent is worse than one that occasionally misses an event. So hook failures
are *silent by design*, and this log is where they go.

## Reading the spool

The spool is newline-delimited JSON, one envelope per line, written post-redaction. It is
the ground truth for "did capture happen":

```sh
# Is anything being captured at all?
wc -l ~/.ctxlake/spool/*/*.ndjson

# What kinds of events, for one session?
jq -r .event_type ~/.ctxlake/spool/claude_code/<session_id>.ndjson | sort | uniq -c

# Did redaction fire, and on what rules?
jq -r 'select(.redaction.status != "clean") | "\(.redaction.status)\t\(.redaction.rules_fired | join(","))"' \
  ~/.ctxlake/spool/*/*.ndjson | sort | uniq -c

# Are exit codes present? (their absence is how friction detection goes blind)
jq -r 'select(.tool) | "\(.runtime)\t\(.tool.name)\t\(.tool.exit_code)"' \
  ~/.ctxlake/spool/*/*.ndjson | head
```

Two things the spool tells you that nothing else does:

- **A growing spool with a reachable store means the daemon is not draining it.** A spool
  line is deleted only after its store write is confirmed, so unbounded growth is the
  *correct* symptom of a store outage — capture is still safe, nothing is lost.
- **`fleet_id` or `agent_id` reading `unconfigured-*`** means the hook ran without its
  environment set. Capture works, attribution does not. Re-run the installer; it is
  responsible for putting those into the hook's command environment.

## Reading the cache

The cache is what hooks actually read — they never touch the network (see
[concepts.md](concepts.md)). So a briefing problem is nearly always a cache problem, not a
store problem:

```sh
ls -la ~/.ctxlake/cache/                 # when was it last refreshed?
jq . ~/.ctxlake/cache/roster.json        # who does this host think is active?
```

If the cache is old, the question is about the daemon, not the lake. If the cache is
current but a peer is missing, that peer's heartbeat has genuinely lapsed — which is
indistinguishable, from here, from that peer having stopped. Both are correct answers to
"is it working"; only the peer's own host can tell you which.

## When you suspect the store rather than the client

The lake is plain partitioned Parquet, so any engine can read it without ctxlake:

```sh
duckdb -c "SELECT runtime, event_type, count(*) FROM 's3://bucket/ctxlake/sessions/**/*.parquet' GROUP BY 1,2"
```

This is a genuinely useful escape hatch: it answers "is my data there" without trusting
any of ctxlake's own code. If DuckDB sees your events, capture and upload worked and the
problem is downstream.

## Resetting safely

Ordered from least to most destructive. Stop at the first that helps.

1. **Let the cache rebuild.** Delete `~/.ctxlake/cache/` — it is derived, and the daemon
   refetches. This fixes most briefing weirdness and cannot lose anything.
2. **Reinstall the hooks.** The installer is idempotent and writes a `.bak` first;
   uninstall removes exactly what it added.
3. **Do not delete the spool** unless you accept losing those sessions. It is the only
   copy of anything not yet uploaded.

Nothing here touches the lake. Bronze is append-only, and no troubleshooting step should
ever be a reason to write to it.

## When it is not a ctxlake problem

**`412 Precondition Failed` in the daemon log** looks like a bug and is not — that is
compare-and-swap working. Someone else won a race and the daemon retries. Concentrated
on one key it indicates a genuinely hot object ([scaling.md](scaling.md)); spread
across many keys it suggests clock skew.

## Next steps

- [architecture.md](architecture.md) — the symptom-to-cause table, and every knob
- [coordination.md](coordination.md) — what the roster and intents do and do not promise
- [security.md](security.md) — redaction rules and what lands in quarantine
