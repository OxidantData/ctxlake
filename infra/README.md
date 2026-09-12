# Docs site infrastructure

A reviewable record of what exists in AWS and why. Created by hand against account
`320107919290` — the same account that serves `oxidantdata.com`. **Do not re-run
anything here blindly**; it is a description, not a provisioning script.

## What exists

| Resource | Identifier |
|---|---|
| S3 bucket | `oxidantdata-docs-320107919290` (us-east-1, all public access blocked) |
| CloudFront distribution | `E5S9QVG3JL86S` → `ctxlake.oxidantdata.com` |
| Origin Access Control | `ctxlake-docs-oac` (`E114JFCFIMWEIG`) |
| Deploy role | `arn:aws:iam::320107919290:role/ctxlake-docs-deploy` |

Repo variables wiring `.github/workflows/docs.yml` to the above:
`AWS_DOCS_DEPLOY_ROLE_ARN`, `DOCS_S3_BUCKET`, `DOCS_CLOUDFRONT_ID`.

## Decisions worth keeping

**A separate bucket, not a `docs/` prefix in the site bucket.** The website deploy runs
`aws s3 sync dist s3://… --delete` across its whole bucket, which would erase a shared
prefix on its next run. Separation costs nothing and removes the failure mode.

**A separate distribution, not a `/docs/*` behavior on the existing one.** Adding a
behavior means editing the live distribution serving `oxidantdata.com`. A second
distribution has zero blast radius and needs no new certificate or DNS record.

**Bucket stays private; CloudFront reads it through OAC.** The bucket policy grants
`s3:GetObject` to the CloudFront service principal, conditioned on the source ARN of that
one distribution. Direct S3 URLs return 403, which is verified after each change.

**403 *and* 404 both map to `/404.html`.** With OAC and no `s3:ListBucket`, a missing key
returns `AccessDenied` rather than `NoSuchKey`, so a 404-only mapping never fires. This
was observed in testing, not predicted — a missing page served a raw 403 until both were
mapped.

**The deploy role's invalidation permission names only the docs distribution.** It
initially pointed at the website's and was retargeted; a docs deploy has no business
invalidating the marketing site.

**OIDC trust is `refs/heads/main` of this repo only**, in both the plain and numeric-id
`sub` forms, matching the convention the existing `oxidant-site-deploy` role uses.

## Custom domain

`https://ctxlake.oxidantdata.com` — ACM certificate
`ce0ab97f-1f24-49bd-9362-2430f8c18d71` (us-east-1, DNS-validated), attached to the
distribution as its sole alias, with A and AAAA alias records in hosted zone
`Z0794502B7ZXSE8UV137` pointing at the distribution.

Two details worth keeping:

**The certificate must be in us-east-1** regardless of where anything else lives.
CloudFront reads certificates from that region only.

**`Z2FDTNDATAQYW2` is not this zone's id.** It is CloudFront's fixed, global hosted-zone
id, used as the `AliasTarget.HostedZoneId` for every distribution alias record. Putting
the real zone id there is a common and confusing failure.

Setting an alias without also setting `ViewerCertificate` leaves CloudFront serving its
default `*.cloudfront.net` certificate, which does not cover the alias — every request to
the custom domain then fails TLS while the CloudFront domain keeps working. Both were set
in the same update.

The marketing site's distribution was not touched; this is a separate one, and
`https://oxidantdata.com` was verified still serving afterwards.
