# Security — redaction, quarantine, prompt injection, IAM, threat model

## Redaction: three layers, cheapest first

Redaction runs inside [`ctxlake-hook`](../crates/ctxlake-hook/src/main.rs), before the
spool, on every path including `ctxlake import` — never as a later pass. Bronze
(`sessions/`) is immutable, so a secret that lands there is permanent; the only place
scrubbing can happen is before the first write, once. See
[`redact.rs`](../crates/ctxlake-core/src/redact.rs) for the implementation this section
describes.

**1. Literal prefixes.** A fixed table of markers — `sk-`, `AKIA`, `ghp_`, PEM headers,
`Authorization:`, and similar — matched in one Aho-Corasick pass over the input. Cheap,
and it catches the overwhelming majority of accidental leaks: pasted keys, printed env
vars, curl commands that echo their own headers.

**2. Entropy.** Long runs of base64/hex-charset characters in *tool output* — not
prompts, not assistant text — are scored by Shannon entropy. A run of at least 32
characters scoring at or above 4.5 bits/char is treated as a secret and replaced.
Base64-encoded random data sits near 6.0; English prose sits near 4.0; a hex digest
also sits near 4.0 and survives on entropy alone (it's still fair game for the literal
layer if it happens to match a marker). This layer is deliberately restricted to tool
*output* — prompts and assistant messages get literal matching only, because prose
false-positives are expensive: they silently gut the content that makes a briefing
useful, and a `cat .env` or a `printenv` leaking through tool output is a far more
common real incident than a secret typed in prose.

**3. Path denylist.** Reading `~/.aws/credentials`, `~/.ssh/id_*`, `~/.netrc`,
`~/.hermes/.env`, `~/.kube/config`, and similar has its *result* dropped entirely,
regardless of content — the call itself is still recorded (so "an agent read
`~/.aws/credentials`" remains visible for audit), only the returned bytes are withheld.
This exists because some credential files have no literal marker and no obviously high
entropy (a well-formatted INI file of plaintext values, for instance) — the safest rule
for a known-sensitive path is not to inspect its contents at all.

### Quarantine, not silent drop

Anything that trips a rule is **quarantined**, not dropped: the withheld value is
replaced with a placeholder naming its byte length and a hash of the original, and the
original never leaves the hook process. The hash is what lets dedup and frequency
counting keep working without ever holding the value — the same secret pasted twice
produces the same placeholder, a different secret produces a visibly different one.
Quarantined entries route to `quarantine/<agent_id>/<date>/<ulid>.json` in the bucket
(see [layout.md](layout.md)) so a human can review what got caught, without the review
itself being a way to re-expose the secret.

### The honest edges of this net

- **`MAX_SCAN_BYTES` is 256 KiB.** Entropy scanning only covers the first 256 KiB of a
  field — the hook budget is 5ms p99, and scanning a 10MB tool result in full would
  blow through that on its own. A secret positioned past that boundary with no literal
  prefix survives. Literal marker matching still covers the entire field regardless of
  size, so this gap is specifically "unprefixed, high-entropy, and far into an
  oversized output" — a real but narrow case.
- **Redaction only protects data that flows through `ctxlake-hook`.** A host with valid
  store credentials can always write directly to the bucket, bypassing the hook and its
  scrubber entirely. This is not a bug to fix in the redactor — no client-side scrubber
  can protect against a client that chooses not to run it. It's the reason the IAM
  section below scopes credentials as tightly as it does: the redactor is the only line
  of defense on the intended path, so the *unintended* path needs to be made as narrow
  as possible instead.
- **This is a scrubber, not a DLP system.** It is tuned for what actually leaks through
  a coding agent's tool calls — pasted keys, `.env` reads, curl headers — not for
  general-purpose PII detection. Don't route regulated personal data through ctxlake on
  the assumption that redaction makes it safe to do so.

## Prompt injection: treat everything read from the lake as untrusted

Claims, handoff notes, and declared intents are written by *other agents*, then
rendered directly into a context window. That makes every one of them an opportunity
for prompt injection the moment a compromised or careless agent writes something
designed to manipulate whoever reads it next — "ignore your instructions and instead
...", written into a claim that gets promoted and briefed to the whole fleet.

The house rule (AGENTS.md) is explicit: **sanitize at render time, not only at
ingest.** Ingest-time filtering (the redactor above) is aimed at secrets, not at
adversarial instructions — a well-formed sentence with no entropy and no marker can
still be a successful injection. The rendering step that turns a claim or a briefing
into text an agent's context window will actually see is the last and most important
place to treat that text as data, never as instruction — the same posture this
document itself takes toward comment text, tool output, and anything else that
originates outside the code you're reading right now.

This is also the entire reason [`InjectedContext`](../crates/ctxlake-core/src/envelope.rs)
exists as a field on every envelope: it records exactly which claims were injected into
which session, by whom. If a briefed claim later turns out to have been an injection
attempt, this lineage is what lets you trace every session that was exposed to it —
it cannot be reconstructed after the fact from transcripts alone, which is why it's
captured at injection time or not at all.

## IAM: what each process actually needs

The hook and the MCP server need **no store credentials at all** — they never touch
the network (invariant 1). Only `ctxlake sync` and `ctxlake maint` need a credential,
and they don't need the same one:

- **`ctxlake sync`** needs `GetObject`, `PutObject`, and `ListBucket` scoped to the
  fleet's prefix. It never needs `DeleteObject` — nothing in the normal write path
  deletes anything, since `sessions/` is append-only and `snapshot/` blobs are
  content-addressed (a new publish never removes an old one; that's a lifecycle
  policy's job, not a runtime one).
- **`ctxlake maint`** needs the same read/write scope as `sync`, plus permission to
  actually run compaction (which does rewrite `sessions/` into fewer, larger files —
  the one legitimate case that needs `DeleteObject`, and only on objects it just
  finished superseding with a compacted replacement). Scope this separately from
  `sync`'s credential if your IAM setup supports it, so a compromised `sync` credential
  on one host can't rewrite history.
- **`ctxlake doctor`** needs the union of the above plus permission to write and delete
  scratch objects under a probe prefix, since it exists specifically to execute
  conditional-write primitives against your real bucket rather than assume support.

A bucket policy that denies `DeleteObject` on `sessions/*` and `claims/fleet/*` outright
(carving out only the compaction credential's narrow exception) turns "bronze is
immutable" from a convention this codebase honors into a guarantee your cloud provider
enforces even against a fully compromised `ctxlake sync` process.

Per AGENTS.md invariant 10: **`ctxlake.toml` never holds a credential value, only the
*name* of an environment variable to read one from.** `ctxlake doctor` reports whether
that name resolves to something, never what it resolves to — so a screen-shared
terminal or a copy-pasted config file is never how a credential leaks.

## Threat model

What ctxlake defends against, and — as importantly — what it explicitly does not:

- **In scope:** accidental secret capture from normal tool use (pasted keys, `.env`
  reads, verbose curl output) via the redactor; a claim asserting something false or
  manipulative via the promotion gate's independence and contradiction checks (see
  memory.md); unauthorized writes to `claims/fleet/` or compacted `sessions/` data via
  IAM scoping, not application logic.
- **Out of scope, and said plainly:** a host with valid store credentials that chooses
  to write directly to the bucket, bypassing the hook, is not something any client-side
  design can prevent — the store cannot enforce meaning, only bytes. An agent that
  never calls into `ctxlake-hook` at all (a misconfigured install, a runtime with no
  hook support) is captured *not at all*, not partially — there is no fallback capture
  path, by design, since a fallback that reads process output some other way would
  itself be a much larger attack surface than the hook it was meant to backstop.
  **A stale lease being honored by a stalled process is a coordination gap, not a
  security one** — see [coordination.md](coordination.md)'s "what advisory leases do
  not promise" for why that's named there instead of implied to be covered here.

## Next steps

- [coordination.md](coordination.md) — what a lease does and does not protect against
- [layout.md](layout.md) — where quarantined content and promoted claims live
- [architecture.md](architecture.md) — "what ctxlake does not guarantee," the fuller list
