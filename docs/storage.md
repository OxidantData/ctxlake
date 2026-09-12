# Storage — backends and the CAS capability matrix

ctxlake targets five backends through the [`object_store`](https://docs.rs/object_store)
crate: AWS S3, Google Cloud Storage, MinIO, Cloudflare R2, and the local filesystem.
They are not interchangeable at the primitive level, and pretending otherwise is how a
coordination layer built on conditional writes silently breaks on exactly one of them.

## The two primitives that matter

Everything in `live/` and `snapshot/` reduces to two operations:

- **Put-if-absent** — write only if the key does not exist yet. The obvious primitive
  for "create this the first time," conventionally expressed as an `If-None-Match: *`
  header.
- **Compare-and-swap (CAS)** — write only if the key is still at the version/ETag you
  last read. The primitive for "update this without racing anyone."

## The matrix

| Backend | Put-if-absent | CAS |
|---|---|---|
| AWS S3 | `If-None-Match: *` | `If-Match: <etag>` |
| GCS | `ifGenerationMatch=0` | `ifGenerationMatch=<generation>` |
| MinIO | **not supported** | `If-Match: <etag>` |
| Cloudflare R2 | `If-None-Match: *` | `If-Match: <etag>` (bucket must be in **ETagMatch** conditional-write mode) |
| Local filesystem | `O_EXCL` | rename + `flock` |

Read that middle column again: **MinIO has no put-if-absent.** It rejects
`If-None-Match: *` outright
([minio/minio#20346](https://github.com/minio/minio/issues/20346), closed by MinIO as
"working as intended" — it is a deliberate scope decision on their end, not a bug
waiting to be fixed upstream). Any design that reaches for put-if-absent as its
locking primitive works on four of these five backends and then fails, confusingly,
specifically on the one that ships as everyone's local dev and CI environment.

## This is why leases are CAS-only, never put-if-absent

`If-Match`/`ifGenerationMatch`/rename+flock is the row every backend has in common. So
ctxlake's lease protocol (AGENTS.md invariant 4, detailed in
[coordination.md](coordination.md)) is built entirely on CAS for acquisition: a lease
is always *updated*, never *created*, once it exists. "A lease object always exists;
its contents say free or held" is not a stylistic choice — it's the direct consequence
of this table having no row where put-if-absent is universal.

That still leaves a question this table doesn't answer by itself: how does a lease
object's *first* version come to exist, given that `resource_key(repo, resource)`
(`crates/ctxlake-core/src/hash.rs`) hashes an arbitrary string a human supplies at
claim time? `ctxlake init` cannot pre-seed a resource nobody has named yet — the space
of possible resources is unbounded user input, not a fixed set. The answer is that
bootstrap uses neither primitive in the matrix above: the first time a `GET` on the key
comes back `404`, the client writes `{status: free}` with a plain, unconditional `PUT`
— no condition attached — and re-reads it to get a version before the real, CAS-guarded
acquire. Two agents can race that first `PUT` and both "win," harmlessly, because
either one writes identical content; see [coordination.md](coordination.md) for why
that race costs nothing while the CAS-guarded acquire that follows still admits exactly
one winner.

`live/agents/*.json` (roster heartbeats) bootstraps the same unconditional-`PUT` way,
for a different reason: an agent's own roster key has exactly one writer, ever
(invariant 3), so its first heartbeat has no peer to race against, and every later one
CASes off the version last read as a self-consistency check rather than as protection
from a writer that was never going to show up. The `snapshot/*/current.json` pointer
swap is the one object in this section that really is seeded once, at publish time by
`ctxlake maint` — its content-addressed blob is written once and never touched again,
so it needs no conditional write at all.

## `doctor` executes the primitives, it does not assume them

Because the matrix above has a real gap (put-if-absent) and a real footgun (R2's
default conditional-write mode), `ctxlake doctor` never infers what a bucket supports
from its hostname or vendor. It runs each primitive against a scratch object in your
actual bucket and reports what happened:

```text
$ ctxlake doctor
backend: s3-compatible (MinIO)
  put-if-absent (If-None-Match: *)  ... UNSUPPORTED (412 on retry, not 200 as expected)
  CAS (If-Match: <etag>)            ... ok
  Date header present               ... ok
  list                              ... ok
verdict: leases and live/ heartbeats will work; anything relying on put-if-absent will not.
```

This matters most for R2, where CAS support depends on which conditional-write mode
the bucket was created in — a bucket in the wrong mode returns success codes for writes
that didn't actually apply the condition, which is a far worse failure than an honest
error, because it looks like it worked. Executing the primitive and checking the
*result*, not just the status code's happy path, is the only way `doctor` catches that
before it becomes a lost-update bug in production instead of a line in a preflight
report.

`doctor` also prints which backend it thinks it's talking to, before any of the probe
lines — a best-effort label from `ctxlake_store::backend::describe`, sniffed from the
scheme plus (for `s3://`/`s3a://` URLs) the endpoint override, never a hard-coded branch
in the code path that actually builds the store. It exists so the caveats below don't
require a human to already know which vendor they're pointed at:

```text
$ ctxlake doctor
store   s3://my-bucket/ctxlake
backend: s3-compatible (MinIO)
  caveat: no put-if-absent (minio/minio#20346) — ctxlake never relies on it; leases are
          CAS-only (AGENTS.md invariant 4)
  put-if-absent (If-None-Match: *)  ... UNSUPPORTED (412 on retry, not 200 as expected)
  compare-and-swap                  ... ok
  ...
```

A hostname that doesn't obviously say "minio" or "r2.cloudflarestorage.com" (a MinIO
behind a corporate proxy on its own domain, say) falls back to the honest
`s3-compatible (unrecognized vendor)` rather than guessing wrong — this is a display
label for a human, and it is *never* consulted by [`backend::build`], which sets
`S3ConditionalPut::ETagMatch` unconditionally for every `s3`/`s3a` URL regardless of
what `describe` calls it (see that module's own doc: "one setting, every S3-shaped
backend in scope"). Getting the label wrong costs a slightly less specific line in a
report; nothing downstream branches on it.

## GCS and Cloudflare R2: what's verified here, and what isn't

Both of these get their own paragraph because "the matrix table says it works" and
"this was checked against something real" are different claims, and this codebase has
a house rule against blurring them.

**Cloudflare R2** speaks the S3 API, so it takes the *exact same* `s3://`/`s3a://` code
path as MinIO in `crates/ctxlake-store/src/backend.rs` — there is no `r2://` scheme and
no R2-specific branch, by design. `build()` sets `S3ConditionalPut::ETagMatch`
unconditionally for that scheme regardless of which S3-compatible endpoint you point it
at, so R2 gets exactly the treatment MinIO does with no extra code to keep in sync. That
much — construction succeeding, and the same conditional-put mode being requested for an
R2-shaped endpoint as for a MinIO-shaped one — is asserted by
`r2_endpoint_builds_with_the_same_etag_conditional_put_as_minio` and
`describe_recognizes_r2_from_the_endpoint_hostname` in `backend.rs`'s test module,
**verified by running those tests**, not by reasoning about the code. What is **not**
verified: an actual write against a real R2 bucket in this environment. R2's own
documented footgun stands as written above — a bucket created in the wrong
conditional-write mode returns success codes for a write whose condition silently did
not apply — and nothing in this repo has exercised that failure mode against a live R2
account. `doctor`'s cas-update/cas-conflict-detection probes are what would actually
catch it; run them against your own bucket before trusting it.

**GCS** does not use ETags at all. Reading `object_store` 0.14.1's own GCS client source
(`src/gcp/client.rs`) shows `PutMode::Create` sends the header
`x-goog-if-generation-match: 0` and `PutMode::Update(v)` sends
`x-goog-if-generation-match: <generation>` — GCS's `generation` number fills the same
role an ETag does for S3, and `object_store` maps it onto `PutMode` without ctxlake's
`backend::build` needing to configure anything extra (contrast the S3 branch, which must
opt into `ETagMatch` explicitly — see AGENTS.md invariant 4). One consequence worth
noting: unlike S3, GCS's `PutMode::Create` is a **real put-if-absent** (`generation=0`
means "does not exist yet"), so the "put-if-absent: not supported" row in the matrix
above is an S3/MinIO-family gap, not a universal one — ctxlake still never relies on it
anywhere, since the lease design has to work on the backend that lacks it regardless.
This is **verified by reading the vendored dependency's source**, which is what
`describe_recognizes_gcs_and_flags_generation_preconditions_as_unverified_live` in
`backend.rs` pins down (the test name says exactly that: the claim is a source-read, not
a live check). What is **not** verified: no GCS bucket was reachable from this
environment, so nothing here has issued a real `x-goog-if-generation-match` request
against Google's servers and watched it succeed or fail. Treat GCS support as
"implemented and read carefully," not "field-proven," until someone runs `ctxlake
doctor` against a real bucket and this paragraph gets to cite that instead.

## The local filesystem backend

Local FS has no ETags or generation numbers, so its CAS is implemented directly rather
than mapped onto a header: write the new content to a temp file in the same directory,
take an `flock` on a sibling lock file, verify the target's current content/mtime still
matches what you read, then `rename()` the temp file over the target. `rename()` on a
POSIX filesystem is atomic with respect to concurrent readers, which is the property
CAS needs — nobody observes a half-written file. This path exists for local dev,
single-host setups, and CI; it is not the path multi-host fleets should run on, since
it gives up the durability and multi-writer-from-anywhere properties a real object
store provides.

## What `doctor` cannot fix for you

The matrix is the honest state of five vendors' APIs, not a compatibility layer that
hides their differences. If your bucket genuinely cannot do CAS at all (an S3-compatible
store with no conditional-write support whatsoever, which does exist among smaller
vendors), ctxlake has no fallback — the entire lease and roster design assumes CAS
exists somewhere, because there is no way to build "at most one writer wins" without
it. `doctor` telling you this before you deploy is the whole point of running it.

## Next steps

- [coordination.md](coordination.md) — how leases use CAS end to end, and what they
  don't guarantee even when CAS itself works perfectly
- [layout.md](layout.md) — the bucket keys these primitives are applied to
- [scaling.md](scaling.md) — what CAS contention on a hot key costs and looks like
