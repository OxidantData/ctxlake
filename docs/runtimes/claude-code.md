# Claude Code — hook mapping, injection, blocking, coexistence

Claude Code is the reference runtime: every other runtime's doc measures itself against
this one. **Nothing below is implemented yet** — `ctxlake-hook` is still the Wave 1
scaffold (`crates/ctxlake-hook/src/main.rs` prints "not yet implemented" and exits 0) —
so this is a specification written from Claude Code's published hook documentation,
not a report of a working install. Re-verify every row here against a real install
once the adapter lands; until then, "reference" says which runtime the other two are
compared to, not that this page has been tested.

## Config file and mechanism

`ctxlake install` merges hook entries into Claude Code's settings — project-scoped
`.claude/settings.json` or user-scoped `~/.claude/settings.json` — under the `hooks`
key. Claude Code's `hooks` structure maps each event name to an array of matcher
entries, and each matcher's `hooks` array can hold multiple command entries side by
side. The installer appends one entry to the relevant arrays; it never replaces the
`hooks` object wholesale, and it writes a `.bak` first (AGENTS.md invariant 8). MCP
registration is separate: `.mcp.json` (project scope) or `claude mcp add` (user scope),
also merged rather than overwritten.

Each hook invocation runs `ctxlake-hook` as a short-lived child process. Claude Code
writes the event payload as JSON to the process's stdin and reads the process's stdout
and exit code back — this is the entire transport. No socket, no shared memory, no
long-lived connection: exactly the shape invariant 1 requires.

## Event mapping

| Claude Code hook | ctxlake `EventType` | Notes |
|---|---|---|
| `SessionStart` | `SessionStart` | Fires on a new session and on `--resume`/`--continue`. This is the injection point for the briefing. |
| `UserPromptSubmit` | `Prompt` | The human's turn. Can also inject additional context ahead of the prompt, and can block a submission outright. |
| `PreToolUse` | *(not emitted as its own envelope)* | Used to capture `tool.input_hash` before execution, and as the block point for denylisted paths — see below. Merged into the `ToolCall` envelope once `PostToolUse` reports the outcome, so a call whose process dies mid-execution still isn't silently lost from the input side. |
| `PostToolUse` | `ToolCall` | The primary capture point: `tool.name`, `tool.result`, `tool.exit_code`, `tool.duration_ms`, and `tool.paths` when derivable. |
| `PreCompact` | `Compact` | Fires immediately before context is discarded — this is ctxlake's only chance to persist what's about to disappear, and the reason `EventType::Compact` exists in the schema at all. |
| `Stop` / `SubagentStop` | `Assistant` | The assistant's final message for the turn is captured as one `Assistant`-role envelope. |
| `SessionEnd` | `SessionEnd` | Seals the session: the spool's segment for this `session_id` is flushed and, on the daemon's next poll, written to `sessions/` as one Parquet file. |
| `Notification` | *(not mapped)* | Permission prompts and similar UI-level events carry no information the envelope schema is designed to hold; dropped rather than force-fit. |

## Capability matrix

| Capability | Support | Mechanism |
|---|---|---|
| Capture | Yes | `PostToolUse` + `Stop` + the lifecycle hooks above |
| Briefing injection | Yes | `SessionStart` and `UserPromptSubmit` can return additional context that Claude Code folds into the model's input |
| Block | Yes | `PreToolUse` can deny a call before it executes |
| Fail-closed | Supported by the runtime; **used deliberately, not by default** | See below |
| Compaction marker | Yes | `PreCompact` |
| MCP | Yes | `ctxlake mcp` runs as a stdio child registered via `.mcp.json` / `claude mcp add` |

**On fail-closed specifically:** Claude Code's `PreToolUse` hook mechanism *supports*
denying a tool call outright — a hook can return a decision that blocks execution. But
ctxlake's own default posture is **fail-open** for capture: if `ctxlake-hook` errors or
panics while building an envelope, the tool call proceeds anyway, because coordination
metadata is advisory and must never be able to block real work (this is the same
philosophy as invariant 5's leases, applied to the hook itself). The one deliberate
exception is the path denylist in
[`Redactor::is_denied_path`](../../crates/ctxlake-core/src/redact.rs): when a read
targets a known-sensitive path, `ctxlake-hook` can use the very same `PreToolUse`
mechanism to deny the read *before* it executes, rather than only scrubbing the result
after the fact at `PostToolUse`. That's fail-closed, used narrowly, on purpose — not
the hook's general failure policy.

## Coexistence with other hook consumers

Claude Code's settings schema was built for multiple tools to share one event's array
of matchers, and the installer relies on exactly that: it appends its own matcher entry
to `PreToolUse`, `PostToolUse`, and the rest, leaving whatever another tool already
registered untouched. Uninstalling removes only the entries ctxlake added (matched by a
stable marker in the command string), which is what makes `ctxlake install --all`
followed by `ctxlake uninstall` exact rather than "close enough" (invariant 8).

## Known gaps

None known from reading Claude Code's hook documentation — but see the status note at
the top: "none known" means "none found on paper," not "none found by running it,"
since nothing above has run yet. Two narrower things worth knowing regardless:

- Hooks see only the *final* result of a tool call, not intermediate streaming output —
  a long-running `Bash` command's captured `tool.result` is what Claude Code itself
  captured, not a live tail.
- A backgrounded tool call (`run_in_background`) reports its outcome asynchronously;
  ctxlake correlates that outcome with its originating call via the same `tool_id`
  Claude Code uses internally, on whichever later hook invocation reports it.

## Next steps

- [../architecture.md](../architecture.md) — the worked trace this mapping feeds into
- [cursor.md](cursor.md) — the near-parity comparison
- [hermes.md](hermes.md) — where parity is not yet confirmed, said plainly
