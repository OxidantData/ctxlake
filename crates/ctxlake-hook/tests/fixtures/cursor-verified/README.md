# Verified Cursor hook payloads

Captured from a real `cursor-agent 2026.08.25-3e8eec8` run, not written by hand. A
project-level `.cursor/hooks.json` in a scratch directory dumped each event's stdin; the
user's global config was never touched.

Sanitized: the operator email became `operator@example.com` and home paths were
genericized. Nothing else was altered — field names, nesting, and types are verbatim.

## What this corrected

The adapter was first written from inference, and four of those inferences were wrong.
Each one fails silently rather than loudly, which is why fixtures matter here.

| Field | Inferred | Actual |
|---|---|---|
| working directory | `cwd` | `cwd` exists but is often **empty**; the real path is `workspace_roots[0]` |
| tool result | `tool_output` is a string | an **object**: `{"output": "...", "exitCode": 0}` |
| duration | `duration_ms` | `duration`, a float in milliseconds |
| exit code | absent | nested at `tool_output.exitCode` |

The tool-result shape is the one that mattered most. Reading `tool_output` as a string
JSON-encodes the whole object into the result field and never finds `exitCode`, so every
Cursor tool call records `exit_code: None`. Friction detection — "abandoned after 4
failed `cargo test` runs" — is built entirely on exit codes, so it would have gone
quietly blind on Cursor while working correctly on Claude Code.

## Privacy: `user_email`

**Every Cursor hook payload carries `user_email`.** Captured verbatim, that address would
be written into a shared fleet lake on every event, where every other member of the fleet
can read it.

The envelope has no field for it and must not gain one. The adapter drops it; `host_id`
already carries a hashed identifier when a stable one is needed. See `security.md`.

## Events captured

`sessionStart`, `preToolUse`, `beforeShellExecution`, `afterShellExecution`,
`postToolUse`, `sessionEnd`.

Not yet captured: `afterFileEdit`, `beforeReadFile`, `beforeSubmitPrompt`, `stop`,
`afterAgentResponse`, `beforeMCPExecution`, `preCompact`, `subagentStart`,
`subagentStop`. The probe prompt did not trigger them. Fixtures for those remain
inferred, and anything built on them should be treated as unverified until captured the
same way.

## Sanitizing these correctly

Paths appear in more than one encoding. Claude Code's `transcript_path` embeds the
working directory as a *slug* (`-Users-alice-projects-thing`), so a scrub that only
replaces `/Users/alice` leaves the username sitting in the slug. That happened here: a
fixture was pushed to a public repo with the operator's username still in it, caught by
a grep afterwards rather than before.

Scrub both forms, then grep before committing.

## Also worth knowing

- `hook_event_name` uses the same key as Claude Code, so event dispatch is shared.
- `cursor_version` gives `runtime_version` for free.
- `session_id`, `conversation_id`, and `generation_id` were all present and equal here.
  Do not assume that holds — `generation_id` plausibly varies per turn.
- `transcript_path` is `null` at `sessionStart` and populated later, which means Cursor
  does expose a transcript for reconciliation after all, despite its chat store being an
  opaque blob database.
