# Docs site infrastructure

A reviewable record of what exists in AWS and why. Created by hand against account
`320107919290` — the same account that serves `oxidantdata.com`. **Do not re-run
anything here blindly**; it is a description, not a provisioning script.

## What exists

| Resource | Identifier |
|---|---|
| S3 bucket | `oxidantdata-docs-320107919290` (us-east-1, all public access blocked) |
| CloudFront distribution | `E5S9QVG3JL86S` → `drf9zt30vjszj.cloudfront.net` |
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

## Not done

**No custom domain.** The site answers on its CloudFront domain. `docs.oxidantdata.com`
needs an ACM certificate in us-east-1 and a DNS record, which is a decision about the
domain rather than about this repo.

To add it later: request the cert, validate it, add the alias plus
`ViewerCertificate` to `E5S9QVG3JL86S`, and point a CNAME at
`drf9zt30vjszj.cloudfront.net`.
