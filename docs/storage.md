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
[coordination.md](coordination.md)) is built entirely on CAS: every lease object is
seeded to `free` at init time and acquired by *updating* it, never by *creating* it.
"A lease object always exists; its contents say free or held" is not a stylistic
choice — it's the direct consequence of this table having no row where put-if-absent
is universal.

The same reasoning governs `live/agents/*.json` (roster heartbeats, also CAS-updated
after first creation by `ctxlake init`) and the `snapshot/*/current.json` pointer swap
(also CAS — the content-addressed blob underneath it is written once and never
touched again, so it needs no conditional write at all).

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
