# Security — redaction, quarantine, injection, IAM

## Redaction: three layers, cheapest first

Redaction runs inside `ctxlake-hook`, before the spool, on every path including
`ctxlake import` — never as a later pass. Bronze is immutable, so the only place
scrubbing can happen is before the first write, once.

| Layer | What it catches | Scope |
|---|---|---|
| **1. Literal prefixes** | A fixed table of markers — `sk-`, `AKIA`, `ghp_`, PEM headers, `Authorization:` — in one Aho-Corasick pass | Every field, whole length |
| **2. Entropy** | Runs of ≥32 base64/hex-charset characters scoring ≥4.5 bits/char | **Tool output only** |
| **3. Path denylist** | `~/.aws/credentials`, `~/.ssh/id_*`, `~/.netrc`, `~/.hermes/.env`, `~/.kube/config` and similar | The *result* is dropped entirely, regardless of content |

Layer 1 catches most accidental leaks: pasted keys, printed env vars, curl commands that
echo their own headers. Layer 2 is restricted to tool output because prose
false-positives gut the content that makes a briefing useful; for calibration, base64
random data sits near 6.0 bits/char while English prose and hex digests both sit near 4.0,
so a digest survives entropy alone. Layer 3 still records the call — "an agent read
`~/.aws/credentials`" stays visible for audit — and withholds only the returned bytes,
because some credential files have no marker and no high entropy.

### Quarantine, not silent drop

Anything that trips a rule is **quarantined**: the value is replaced with a placeholder
naming its byte length and a hash of the original, which never leaves the hook process.
The hash keeps dedup and frequency counting working without ever holding the value.
Entries route to `quarantine/<agent_id>/<date>/<ulid>.json` so a human can review what was
caught without the review itself re-exposing the secret.

### The honest edges of this net

- **`MAX_SCAN_BYTES` is 256 KiB.** Entropy scanning covers only the first 256 KiB of a
  field, because the hook budget is 5ms p99 and scanning a 10MB tool result would blow
  through it. Literal matching still covers the whole field at any size, so the gap is
  specifically "unprefixed, high-entropy, and far into an oversized output" — real but
  narrow.
- **Redaction only protects data that flows through `ctxlake-hook`.** A host with valid
  store credentials can write directly to the bucket, bypassing the scrubber. No
  client-side scrubber can protect against a client that chooses not to run it — which is
  why the IAM section below scopes credentials as tightly as it does.
- **This is a scrubber, not a DLP system.** It is tuned for what leaks through a coding
  agent's tool calls — pasted keys, `.env` reads, curl headers — not general-purpose PII
  detection. Do not route regulated personal data through ctxlake on the assumption that
  redaction makes it safe.

## Prompt injection: everything read from the lake is untrusted

Claims, handoff notes and declared intents are written by *other agents* and rendered
straight into a context window. The rule is **sanitize at render time, not only at
ingest** — the redactor above aims at secrets, and a well-formed sentence with no entropy
and no marker can still be a successful injection. The rendering step that turns a claim
into text an agent will see is the last and most important place to treat it as data,
never as instruction.

`InjectedContext` on every envelope records which claims were injected into which session,
by whom. If a briefed claim turns out to have been an injection attempt, that lineage is
what lets you trace every session exposed to it — and it cannot be reconstructed from
transcripts after the fact.

## IAM: what each process needs

The hook and the MCP server need **no store credentials at all**. Only `ctxlake sync` and
`ctxlake maint` need one, and not the same one.

| Process | Needs |
|---|---|
| `ctxlake sync` | `GetObject`, `PutObject`, `ListBucket`, scoped to the fleet prefix. **Never `DeleteObject`** — `sessions/` is append-only and snapshot blobs are content-addressed, so nothing in the write path deletes |
| `ctxlake maint` | The same, plus `DeleteObject` for compaction, and only on objects it has just superseded. Scope this separately if your IAM setup allows, so a compromised `sync` credential cannot rewrite history |
| `ctxlake doctor` | The union of both, plus write and delete on a scratch probe prefix — it exists to execute conditional-write primitives against your real bucket |

A bucket policy denying `DeleteObject` on `sessions/*` and `claims/fleet/*`, carving out
only the compaction credential, turns "bronze is immutable" from a convention this
codebase honors into a guarantee your cloud provider enforces even against a fully
compromised `ctxlake sync`.

**`ctxlake.toml` never holds a credential value, only the *name* of an environment
variable.** `ctxlake doctor` reports whether that name resolves to something, never what
it resolves to, so a screen-shared terminal or a pasted config file is never how a
credential leaks.

## Threat model

**In scope:** accidental secret capture from normal tool use, via the redactor; a claim
asserting something false or manipulative, via the promotion gate's independence and
contradiction checks ([memory.md](memory.md)); unauthorized writes to `claims/fleet/` or
compacted `sessions/`, via IAM scoping rather than application logic.

**Out of scope, said plainly:** a host with valid store credentials that writes directly
to the bucket, bypassing the hook, is not something a client-side design can prevent — the
store enforces bytes, not meaning. And an agent that never calls `ctxlake-hook` at all — a
misconfigured install, a runtime with no hook support — is captured *not at all*, not
partially. There is no fallback capture path.

## Next steps

- [architecture.md](architecture.md) — "what ctxlake does not guarantee", the fuller list
- [storage.md](storage.md) — where quarantined content and promoted claims live
- [memory.md](memory.md) — attribution on read, and the quarantine kill switch
