# Runtimes — Claude Code, Cursor, Hermes

Three runtimes, three hook vocabularies, one envelope. Everything downstream —
compaction, digests, the belief layer — reads only the envelope.

<svg viewBox="0 0 720 240" role="img" aria-label="Claude Code, Cursor and Hermes each fire their own hook events into the single ctxlake-hook binary, whose three adapters normalize them into one Envelope type that everything downstream reads." style="width:100%;height:auto">
  <defs>
    <marker id="mr" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto">
      <path d="M0 0 L8 4 L0 8 z" fill="var(--oxidant-text-muted)"/>
    </marker>
    <style>
      .b { fill: var(--oxidant-surface); stroke: var(--oxidant-border-strong); stroke-width: 1; rx: 6 }
      .t { fill: var(--oxidant-text); font: 500 12px var(--oxidant-font-ui) }
      .s { fill: var(--oxidant-text-muted); font: 400 10px var(--oxidant-font-ui) }
      .l { stroke: var(--oxidant-text-muted); stroke-width: 1.25; fill: none; marker-end: url(#mr) }
    </style>
  </defs>

  <text x="8" y="20" class="s">RUNTIME · ITS OWN EVENT NAMES</text>
  <text x="210" y="20" class="s">ONE BINARY · THREE ADAPTERS</text>
  <text x="420" y="20" class="s">ONE SCHEMA</text>

  <rect class="b" x="8" y="30" width="160" height="48"/>
  <text x="20" y="52" class="t">Claude Code</text>
  <text x="20" y="68" class="s">PostToolUse, PreCompact, Stop…</text>

  <rect class="b" x="8" y="100" width="160" height="48"/>
  <text x="20" y="122" class="t">Cursor</text>
  <text x="20" y="138" class="s">afterFileEdit, stop…</text>

  <rect class="b" x="8" y="170" width="160" height="48"/>
  <text x="20" y="192" class="t">Hermes</text>
  <text x="20" y="208" class="s">post_tool_call, post_llm_call…</text>

  <rect class="b" x="210" y="30" width="130" height="48"/>
  <text x="222" y="58" class="t">claude_code.rs</text>

  <rect class="b" x="210" y="100" width="130" height="48"/>
  <text x="222" y="128" class="t">cursor.rs</text>

  <rect class="b" x="210" y="170" width="130" height="48"/>
  <text x="222" y="198" class="t">hermes.rs</text>

  <rect class="b" x="420" y="100" width="140" height="48"/>
  <text x="432" y="122" class="t">Envelope</text>
  <text x="432" y="138" class="s">SCHEMA_VERSION</text>

  <rect class="b" x="600" y="100" width="112" height="48"/>
  <text x="612" y="122" class="t">everything</text>
  <text x="612" y="138" class="s">reads only this</text>

  <path class="l" d="M168 54 H206"/>
  <path class="l" d="M168 124 H206"/>
  <path class="l" d="M168 194 H206"/>

  <path class="l" d="M340 54 H380 V120 H416"/>
  <path class="l" d="M340 124 H416"/>
  <path class="l" d="M340 194 H380 V128 H416"/>

  <path class="l" d="M560 124 H596"/>

  <text x="420" y="176" class="s">a field is only ever added, never reinterpreted —</text>
  <text x="420" y="190" class="s">bronze is immutable, so a field whose meaning</text>
  <text x="420" y="204" class="s">changed underneath it would make history unreadable</text>
</svg>

## The shared mechanism

Every runtime invokes `ctxlake-hook` as a short-lived child process: the payload arrives
as JSON on stdin, and the runtime reads stdout and the exit code back. No socket, no
shared memory, no long-lived connection.

```sh
env CTXLAKE_FLEET_ID=myteam CTXLAKE_AGENT_ID=cc-01 ctxlake-hook <event> <runtime>
```

Both arguments are **positional**; the binary parses no flags at all, so a
`--runtime hermes` is silently swallowed as a non-existent third argument and the hook
captures nothing. The event is the runtime's own name (`PostToolUse`, `afterFileEdit`,
`post_tool_call`), not the envelope's. The `env` prefix is required because no hook schema
has an `env` key; without it events capture fine but land attributed to
`unconfigured-fleet`/`unconfigured-agent`.

## Event mapping

| Envelope event | Claude Code | Cursor | Hermes |
|---|---|---|---|
| `SessionStart` | `SessionStart` (also on `--resume`) | — inferred from the first `beforeSubmitPrompt` | `on_session_start` |
| `Prompt` | `UserPromptSubmit` | `beforeSubmitPrompt` | `pre_llm_call` |
| *(pre-check, block point)* | `PreToolUse` | `beforeShellExecution`, `beforeReadFile`, `beforeMCPExecution` | `pre_tool_call` |
| `ToolCall` | `PostToolUse` | `afterFileEdit` | `post_tool_call` |
| `Assistant` | `Stop` / `SubagentStop` | `stop` | `post_llm_call` |
| `SessionEnd` | `SessionEnd` | *(process lifecycle)* | `on_session_end`, `on_session_finalize`, `on_session_reset` |
| `Compact` | `PreCompact` | **none** | **none** |

`PreToolUse` emits no envelope of its own; it captures `tool.input_hash` before execution
and merges into the `ToolCall` envelope once the post-hook reports the outcome. Claude
Code's `Notification` is not mapped — permission prompts carry nothing the schema holds.
Hermes fires `pre_tool_call` once per call, so three parallel calls fire it three times.

## Capability matrix

| Capability | Claude Code | Cursor | Hermes |
|---|---|---|---|
| Capture | yes | yes | yes |
| Briefing injection | mechanically yes | mechanically yes | mechanically yes |
| Block a colliding edit | mechanically yes | mechanically yes | mechanically yes |
| Compaction marker | yes | **no** | **no** |
| MCP client | yes | yes | yes |

> **Injection and blocking are not sent yet, on any runtime.** All three runtimes
> support them, but `response_for` returns `{}` for every event: both need the daemon's
> view of the lake — roster state, a synthesized digest — which does not reach the hook
> in this wave. Capture is what works today.

**On fail-closed.** ctxlake's posture is fail-open for capture: if the hook errors while
building an envelope, the tool call proceeds anyway, because coordination metadata is
advisory and must never block real work. The one exception is the path denylist
([security.md](security.md)) — a read of a known-sensitive path is denied at the pre-tool
hook rather than scrubbed afterwards.

All three schemas hold an array per event, so more than one tool can register.
`ctxlake install` appends; `ctxlake uninstall` removes only its own entries, matched by a
stable marker (`ctxlake-hook` as a whitespace-delimited token followed two tokens later by
the runtime id), so a wrapper you wrote around the line survives and a foreign tool that
merely mentions `ctxlake-hook` is left alone.

## Claude Code

Config: `.claude/settings.json` (project) or `~/.claude/settings.json` (user), under
`hooks`. MCP is registered separately via `.mcp.json` or `claude mcp add`.

> **Verification basis:** the adapter is implemented and covered by golden fixtures for
> every event, but those fixtures were written from Claude Code's *documented* hook field
> list, not captured from a live session — the mapping is tested for self-consistency,
> not against observed payloads. Claude Code's prompt text in particular was never
> captured, and the reference names a field the binary does not send.

Hooks see only the *final* result of a tool call, not intermediate streaming output. A
backgrounded call (`run_in_background`) reports asynchronously, correlated to its
originating call by the same `tool_id` Claude Code uses internally.

## Cursor

Config: `.cursor/hooks.json` or `~/.cursor/hooks.json`, schema `"version": 1`. MCP in
`.cursor/mcp.json`.

> **Verification basis:** the adapter is asserted against payloads **captured from a
> real `cursor-agent` run** (`crates/ctxlake-hook/tests/fixtures/cursor-verified/`). Of
> the three runtimes, Cursor's field mapping is the one grounded in observed traffic
> rather than documentation. That capture corrected five fields that had been inferred
> from the docs — every one of them failing silently.

What Cursor actually sends, where its documentation misleads:

| Field | What the docs suggest | What arrives |
|---|---|---|
| working directory | `cwd` | `cwd` is present but **empty**; the path is `workspace_roots[0]` |
| tool result | `tool_output`, a string | a **JSON-encoded string** holding `{"output": ..., "exitCode": N}` |
| exit code | — | nested inside that decoded object |
| duration | `duration_ms`, integer | `duration`, a **float** in milliseconds |
| client version | — | `cursor_version` |

The exit code is the one that matters: friction detection is built entirely on exit codes,
so without it that signal is blind on Cursor with nothing anywhere to say so.

> **`user_email` is dropped.** Every Cursor payload carries the operator's email address;
> stored verbatim it would publish that address to everyone who can read the fleet's lake.
> The envelope has no field for it, and a test asserts it never appears in one.

Two gaps against Claude Code: Cursor has **no `SessionStart` event** (boundaries are
inferred from the `cursor-agent` process lifecycle, so briefing injection happens at the
first `beforeSubmitPrompt`), and **no pre-compaction event**, so `EventType::Compact` is
never emitted for a Cursor session. Import fidelity differs too — see
[adding-it.md](adding-it.md).

## Hermes

Hermes declares shell hooks in `~/.hermes/config.yaml`, and its wire contract is Claude
Code-compatible: `hook_event_name`, `tool_name`, `tool_input`, `session_id` and `cwd` are
top level and named identically, which is why one binary serves both. ctxlake uses this
surface rather than Hermes's Python plugin API so every runtime shares one redactor. The
two coexist — Hermes runs Python plugins first, then shell hooks, first valid directive
wins.

> **Verification basis:** every capability, field name and semantic here was read from
> Hermes's own source — `agent/shell_hooks.py`, `hermes_cli/plugins.py` (`VALID_HOOKS`)
> and its hooks documentation. `adapters::hermes` normalizes exactly this payload shape.
> What is **not** yet true: `ctxlake install hermes` does not exist, so the block below
> is the target invocation you write by hand, not installer output.

```yaml
# ~/.hermes/config.yaml
hooks:
  on_session_start:
    - command: env CTXLAKE_FLEET_ID=myteam CTXLAKE_AGENT_ID=herm-01 ctxlake-hook on_session_start hermes
  pre_tool_call:
    - command: env CTXLAKE_FLEET_ID=myteam CTXLAKE_AGENT_ID=herm-01 ctxlake-hook pre_tool_call hermes
      timeout: 5
      fail_closed: false
  # …and the same line for pre_llm_call, post_tool_call, post_llm_call, on_session_end
```

Each entry accepts `command`, and optionally `matcher` (a regex over the tool name),
`timeout` (seconds) and `fail_closed`. **`fail_closed` only does something on
`pre_tool_call`**, the sole blocking event; Hermes warns if you set it elsewhere. ctxlake
leaves it `false`: a collision check is advisory, and a hook crash should never wedge your
agent. An unknown event name gets a did-you-mean warning and is skipped, so a typo fails
visibly.

> **`extra` is the one place the payloads genuinely differ.** Claude Code puts
> `tool_result` at the top level; Hermes nests `result`, `duration_ms`, `task_id`,
> `model` and `platform` under `extra`. An adapter reading only the shared top-level
> fields records every Hermes tool call with no result and no duration.

Injection lands in the **user message**, never the system prompt, keeping the system
prompt byte-identical so Hermes's prompt cache stays warm — which suits a volatile
per-turn briefing exactly. Several plugins' contributions are joined with blank lines in
alphabetical order of directory name.

Three gaps:

- **No compaction equivalent**, so a Hermes session has no marker for the moment its
  context was discarded — you cannot reconstruct what the agent could still see at a given
  point. Tool calls, outcomes and digests are unaffected. The gateway's `session:compress`
  event covers messaging-platform sessions, not the tool-calling loop, so it is not a
  substitute.
- **`pre_llm_call`/`post_llm_call` envelopes carry `role` but no `content`.** No field name
  for the prompt or assistant text has been verified against a live capture, and this
  adapter leaves a field empty rather than spool a guessed key. A Hermes session's bronze
  record therefore has one `Prompt` and one `Assistant` envelope per turn, correctly
  ordered and timestamped, without the text. Fix it by diffing against a live capture.
- **A forced plugin reload clears the hook registry.** Hermes re-registers config hooks
  afterwards, but if capture stops after you reload an unrelated plugin, that is the
  window — `hermes hooks list` shows whether registration came back.

## Next steps

- [how-it-works.md](how-it-works.md) — where the envelope goes next
- [reference.md](reference.md) — what `ctxlake install` writes, per runtime
- [architecture.md](architecture.md) — the worked trace this mapping feeds into
