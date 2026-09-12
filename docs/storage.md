# Storage — backends and the CAS capability matrix

ctxlake targets five backends through the [`object_store`](https://docs.rs/object_store)
crate: AWS S3, Google Cloud Storage, MinIO, Cloudflare R2, and the local filesystem.
They are not interchangeable at the primitive level, and pretending otherwise is how a
coordination layer built on conditional writes silently breaks on exactly one of them.

## The two primitives that matter

- **Put-if-absent** — write only if the key does not exist yet
  (`If-None-Match: *`-shaped).
- **Compare-and-swap (CAS)** — write only if the key is still at the version/ETag you
  last read. The primitive `live/` (roster, intents) and the `snapshot/` pointer both
  depend on.

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
"working as intended"). This is why every CAS-dependent write in ctxlake — roster
heartbeats, the snapshot pointer — is built entirely on CAS for acquisition, never on
put-if-absent: `If-Match`/`ifGenerationMatch`/rename+flock is the row every backend
here has in common.

Bootstrap (an object's *first* version) uses neither primitive: the first `GET` on a
key that comes back `404` is followed by a plain, unconditional `PUT`, then a re-read
to obtain a version before any real CAS-guarded update. An agent's own roster key has
exactly one writer ever, so its first heartbeat has no peer to race against. The
`snapshot/*/current.json` pointer is seeded once, at publish time, by whichever
`ctxlake maint` run gets there first — its content-addressed blob is written once and
never touched again, so it needs no conditional write at all.

## `doctor` executes the primitives, it does not assume them

```text
$ ctxlake doctor
backend: s3-compatible (MinIO)
  put-if-absent (If-None-Match: *)  ... UNSUPPORTED (412 on retry, not 200 as expected)
  CAS (If-Match: <etag>)            ... ok
  Date header present               ... ok
  list                              ... ok
verdict: roster heartbeats and live/ writes will work; anything relying on
  put-if-absent will not.
```

This matters most for R2, where CAS support depends on which conditional-write mode
the bucket was created in — a bucket in the wrong mode returns success codes for
writes that didn't actually apply the condition, which is worse than an honest error
because it looks like it worked. `doctor` also prints which backend it thinks it's
talking to (`ctxlake_store::backend::describe`, sniffed from the URL scheme and
endpoint), so the caveats don't require a human to already know which vendor they're
pointed at.

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
has no fallback: the roster and snapshot design assumes CAS exists somewhere, because
there is no way to build "at most one write wins" without it. `doctor` telling you
this before you deploy is the whole point of running it.

## Next steps

- [coordination.md](coordination.md) — how the roster and snapshot pointer use CAS
  end to end
- [layout.md](layout.md) — the bucket keys these primitives are applied to
- [scaling.md](scaling.md) — what CAS contention on a hot key costs and looks like
