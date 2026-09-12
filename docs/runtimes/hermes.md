# Hermes — the Python plugin, event mapping, known gaps

Hermes is **capture-complete**: every event ctxlake needs to build a full envelope is
observable through Hermes' plugin API, and that side is implemented and verified.
Injection and blocking are a different story, and this page says exactly where that
line falls rather than implying the same parity Claude Code and Cursor have.

## Config file and mechanism

Hermes loads ctxlake as a Python plugin, registered under `~/.hermes/`. The plugin
itself calls out to `ctxlake-hook` per event — same transport as the other two
runtimes: the event data is handed to the compiled binary, its output (if any) is read
back, no persistent connection is held open. Hermes' own config lives under the same
directory the redactor's path denylist already knows about —
[`redact.rs`](../../crates/ctxlake-core/src/redact.rs) lists `/.hermes/.env`
specifically because that's where a Hermes install's own secrets live, and a read of it
gets its result withheld like any other denylisted path.

## Event mapping

| Hermes plugin callback | ctxlake `EventType` | Notes |
|---|---|---|
| session start callback | `SessionStart` | Capture verified. |
| prompt-submitted callback | `Prompt` | Capture verified. |
| tool-call callback | `ToolCall` | Capture verified: name, input, result, exit code, and duration are all available through the plugin API. |
| response/turn-complete callback | `Assistant` | Capture verified. |
| session end callback | `SessionEnd` | Capture verified. |
| *(none)* | `Compact` | **No equivalent exists.** This is the gap [`envelope.rs`](../../crates/ctxlake-core/src/envelope.rs) documents directly on `EventType::Compact`: "Hermes has no equivalent event, so Hermes sessions have a gap here." Whatever context Hermes discards on compaction is lost to ctxlake the same way it's lost to the user — there is no hook to catch it before it goes. |

## Capability matrix

| Capability | Support | Status |
|---|---|---|
| Capture | Yes | Verified against a live install |
| Briefing injection | **Unconfirmed** | The plugin API appears to support returning content that Hermes folds into context, but this has not been verified end to end against a real install — do not assume parity with Claude Code/Cursor here until it's confirmed |
| Block | **Unconfirmed** | Same caveat: whether a plugin callback can actually deny a tool call before execution, rather than only observe it after the fact, needs confirming against a live install before relying on it for anything (including the path-denylist pre-block the other two runtimes support) |
| Fail-closed | Unconfirmed | Depends on the block mechanism above |
| Compaction marker | **No** | No equivalent event exists in the Hermes plugin API |
| MCP | Yes | `ctxlake mcp` runs as a stdio child, registered the same way as the other runtimes |

**Read that table as it's written, not optimistically.** "Capture-complete" is a real,
verified claim about one half of this runtime's support. "Inject and block need
confirming" is not hedging for its own sake — it's the honest position until someone
has run both paths against an actual Hermes install and watched them work. Treating an
unconfirmed capability as if it were confirmed is exactly the kind of overclaim
AGENTS.md's house rules single out as a bug, not a style issue.

## Coexistence with other hook consumers

Hermes' plugin system loads multiple plugins side by side by design — `ctxlake`'s
plugin registration is additive, the same posture as the other two runtimes'
array-based hook files, and `ctxlake uninstall` removes only what it added.

## Known gaps, stated plainly

- **`Compact` is never emitted for a Hermes session** — a structural gap, not a bug to
  fix, since there is nothing in the plugin API to hook.
- **Injection and blocking are unverified.** Until confirmed against a live install,
  plan Hermes deployments around capture working reliably and treat briefing injection
  and tool-call blocking as "should work, not yet proven" rather than relying on them
  for anything safety-critical.
- Because block is unconfirmed, the path-denylist *pre-emptive* deny that Claude Code
  and Cursor perform before a sensitive read executes cannot be assumed on Hermes today.
  The redactor's *after-the-fact* quarantine (dropping the result of a denylisted read)
  still applies regardless, since that happens inside `ctxlake-hook` itself and doesn't
  depend on the runtime's blocking mechanism at all.

## Next steps

- [claude-code.md](claude-code.md) — the reference implementation this is measured against
- [cursor.md](cursor.md) — the near-parity runtime, for comparison
- [../security.md](../security.md) — what the redactor still guarantees even where
  blocking is unconfirmed
