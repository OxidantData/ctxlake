# Storage — backends and the CAS capability matrix

ctxlake targets five backends through the [`object_store`](https://docs.rs/object_store)
crate: AWS S3, Google Cloud Storage, MinIO, Cloudflare R2, and the local filesystem.
They are not interchangeable at the primitive level, and pretending otherwise is how a
coordination layer built on conditional writes silently breaks on exactly one of them.

## The two primitives that matter

- **Put-if-absent** — write only if the key does not exist yet
  (`If-None-Match: *`-shaped).
- **Compare-and-swap (CAS)** — write only if the key is still at the version/ETag you
  last read. The primitive `live/` (roster, intents, leases) and the `snapshot/`
  pointer both depend on.

## The matrix

| Backend | Put-if-absent | CAS |
|---|---|---|
| AWS S3 | `If-None-Match: *` | `If-Match: <etag>` |
| GCS | `ifGenerationMatch=0` | `ifGenerationMatch=<generation>` |
| MinIO | **not supported** | `If-Match: <etag>` |
| Cloudflare R2 | `If-None-Match: *` | `If-Match: <etag>` (bucket must be in **ETagMatch** conditional-write mode) |
| Local filesystem | `O_EXCL` | rename + `flock` |

**MinIO has no put-if-absent.** It rejects `If-None-Match: *` outright
([minio/minio#20346](https://github.com/minio/minio/issues/20346), closed by MinIO as
"working as intended") — not only on conflict: `Create` fails outright even on a key
that has never existed. Any design that reaches for put-if-absent as its locking
primitive works on four of these five backends and then fails, confusingly,
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
of possible resources is unbounded user input, not a fixed set (`ctxlake init` *does*
provision the two well-known, fixed-name lease keys up front — `_maintenance` and
`_claim_provision` — precisely because those two are not arbitrary; see
[cli.md](cli.md)). The answer for every other lease is that bootstrap uses neither
primitive in the matrix above: the first time a `GET` on the key comes back `404`, the
client writes `{status: free}` with a plain, unconditional `PUT` — no condition
attached — and re-reads it to get a version before the real, CAS-guarded acquire. Two
agents can race that first `PUT` and both "win," harmlessly, because either one writes
identical content; see [coordination.md](coordination.md) for why that race costs
nothing while the CAS-guarded acquire that follows still admits exactly one winner.

`live/agents/*.json` (roster heartbeats) bootstraps the same unconditional-`PUT` way,
for a different reason: an agent's own roster key has exactly one writer, ever
(invariant 3), so its first heartbeat has no peer to race against. The
`snapshot/*/current.json` pointer swap is the one object in this section that really
is seeded once, at publish time by `ctxlake maint` — its content-addressed blob is
written once and never touched again, so it needs no conditional write at all.

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
verdict: leases and live/ heartbeats will work; anything relying on put-if-absent will
  not.
```

This matters most for R2, where CAS support depends on which conditional-write mode
the bucket was created in — a bucket in the wrong mode returns success codes for
writes that didn't actually apply the condition, which is worse than an honest error
because it looks like it worked. `doctor` also prints which backend it thinks it's
talking to (`ctxlake_store::backend::describe`, sniffed from the URL scheme and
endpoint), so the caveats don't require a human to already know which vendor they're
pointed at.

```text
$ ctxlake doctor
store   s3://my-bucket/ctxlake
backend: s3-compatible (MinIO)
  caveat: no put-if-absent (minio/minio#20346) — ctxlake never relies on it for
          leases (AGENTS.md invariant 4); extraction's idempotency marker does rely
          on it and degrades to "re-attempt every run" here (see coordination.md)
  put-if-absent (If-None-Match: *)  ... UNSUPPORTED (412 on retry, not 200 as expected)
  compare-and-swap                  ... ok
  ...
```

## GCS and Cloudflare R2: what's verified here, and what isn't

**Cloudflare R2** speaks the S3 API, so it takes the exact same `s3://`/`s3a://` code
path as MinIO — there is no `r2://` scheme and no R2-specific branch. `build()` sets
`S3ConditionalPut::ETagMatch` unconditionally for that scheme, verified by tests that
introspect the builder directly rather than assume `build()` merely not erroring means
the mode took effect. What is **not** verified: an actual write against a real R2
bucket in this environment. Run `doctor`'s CAS probes against your own bucket before
trusting it.

**GCS** does not use ETags at all — `PutMode::Create` sends
`x-goog-if-generation-match: 0` and `PutMode::Update(v)` sends
`x-goog-if-generation-match: <generation>`. Unlike S3, GCS's `PutMode::Create` is a
**real** put-if-absent (`generation=0` means "does not exist yet"), so the "not
supported" row in the matrix above is an S3/MinIO-family gap, not a universal one.
This is verified by reading the vendored dependency's source, not by a live bucket —
no GCS bucket was reachable from this environment. Treat GCS support as "implemented
and read carefully," not "field-proven," until `ctxlake doctor` has run against a real
bucket.

## The local filesystem backend

Local FS has no ETags or generation numbers, so CAS is implemented directly: write the
new content to a temp file in the same directory, take an `flock` on a sibling lock
file, verify the target's current content/mtime still matches what you read, then
`rename()` the temp file over the target — atomic with respect to concurrent readers
on POSIX. This path is for local dev, single-host setups, and CI, not for multi-host
fleets, since it gives up the durability a real object store provides.

## What `doctor` cannot fix for you

If your bucket genuinely cannot do CAS at all (an S3-compatible store with no
conditional-write support whatsoever — this does exist among smaller vendors), ctxlake
has no fallback: the entire lease and roster design assumes CAS exists somewhere,
because there is no way to build "at most one writer wins" without it. `doctor` telling
you this before you deploy is the whole point of running it.

## Next steps

- [coordination.md](coordination.md) — how leases use CAS end to end, and what they
  don't guarantee even when CAS itself works perfectly
- [layout.md](layout.md) — the bucket keys these primitives are applied to
- [scaling.md](scaling.md) — what CAS contention on a hot key costs and looks like
