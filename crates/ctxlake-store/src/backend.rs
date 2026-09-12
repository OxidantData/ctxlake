//! Constructs an [`ObjectStore`] from a URL plus a small set of connection options.
//!
//! One function, four schemes (`file://`, `s3://`/`s3a://`, `gs://`,
//! `az://`/`abfs://`/`abfss://`), so `ctxlake.toml`'s `store` line is the only place
//! a backend gets chosen — nothing downstream needs to know which one it got.
//!
//! S3-compatible backends are where the option normalization actually matters:
//! MinIO and Cloudflare R2 both speak the S3 API but need `endpoint`, `allow_http`
//! and `virtual_hosted_style_request` set explicitly, and both need
//! [`S3ConditionalPut::ETagMatch`] rather than the (non-existent, per
//! minio/minio#20346) `If-None-Match: *` semantics — see AGENTS.md invariant 4. Real
//! AWS S3 also accepts `ETagMatch`, so there is no "which S3 is this" branch: one
//! setting, every S3-shaped backend in scope.

use std::path::PathBuf;
use std::sync::Arc;

use object_store::aws::{AmazonS3Builder, S3ConditionalPut};
use object_store::azure::MicrosoftAzureBuilder;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::local::LocalFileSystem;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::ObjectStore;
use url::Url;

use crate::error::StoreError;
use crate::local_cas::CasLocalFileSystem;

/// Connection options a URL alone can't carry (credentials, an endpoint override
/// for a self-hosted backend, ...).
///
/// Every field is optional and, left unset, falls back to that backend's normal
/// environment-variable convention (`AmazonS3Builder::from_env` and friends) —
/// `ctxlake.toml` should hold *names* of env vars, never secret values themselves
/// (AGENTS.md invariant 10), so the common case is "don't set anything here, let
/// the environment supply it."
#[derive(Debug, Clone, Default)]
pub struct BackendOptions {
    /// S3-compatible custom endpoint — required for MinIO and R2, unused by real
    /// AWS S3, ignored by GCS and Azure.
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    /// A local dev MinIO is commonly plain HTTP; a misconfigured `true` against a
    /// real cloud endpoint would silently send credentials unencrypted, so this
    /// defaults to `false`.
    pub allow_http: bool,
    /// S3 default is path-style (`https://endpoint/bucket/key`), which is what
    /// MinIO expects out of the box. R2 and some MinIO deployments behind a proxy
    /// need virtual-hosted-style (`https://bucket.endpoint/key`) instead.
    pub virtual_hosted_style_request: bool,
}

/// Assemble the `AmazonS3Builder` for an `s3`/`s3a` URL — factored out of [`build`]
/// so a test can inspect the builder's resolved config (`get_config_value`, which
/// `object_store` exposes on the *builder*, not on the `AmazonS3` client `build()`
/// returns) before `.build()` throws that state away. Without this split, the
/// claim that MinIO and R2 both get `S3ConditionalPut::ETagMatch` was asserted only
/// by "`build()` didn't error" — true even with the `.with_conditional_put(..)`
/// call deleted outright, since `ETagMatch` is `object_store`'s own `#[default]`.
///
/// `.with_conditional_put(S3ConditionalPut::ETagMatch)` is placed *after*
/// `from_env()` deliberately: `from_env()` reads `AWS_CONDITIONAL_PUT` into this
/// same field, so without the explicit call here, an operator's own environment
/// (not `ctxlake.toml` — nothing here sets this var, but nothing stops one being
/// set for an unrelated reason) could carry `AWS_CONDITIONAL_PUT=disabled` and
/// silently reintroduce the wildcard `If-None-Match: *` semantics AGENTS.md
/// invariant 4 rules out. The explicit call wins that fight unconditionally.
fn s3_builder(bucket: &str, opts: &BackendOptions) -> AmazonS3Builder {
    let mut builder = AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_conditional_put(S3ConditionalPut::ETagMatch);
    if let Some(v) = &opts.endpoint {
        builder = builder.with_endpoint(v);
    }
    if let Some(v) = &opts.region {
        builder = builder.with_region(v);
    }
    if let Some(v) = &opts.access_key_id {
        builder = builder.with_access_key_id(v);
    }
    if let Some(v) = &opts.secret_access_key {
        builder = builder.with_secret_access_key(v);
    }
    if let Some(v) = &opts.session_token {
        builder = builder.with_token(v);
    }
    builder
        .with_allow_http(opts.allow_http)
        .with_virtual_hosted_style_request(opts.virtual_hosted_style_request)
}

/// Build the store addressed by `url`, returning it alongside the [`Path`] `url`
/// pointed at within that store (the part after the bucket/container).
pub fn build(url: &Url, opts: &BackendOptions) -> Result<(Arc<dyn ObjectStore>, Path), StoreError> {
    match url.scheme() {
        "file" => {
            // `object_store` 0.14's `LocalFileSystem` has no `PutMode::Update` at
            // all (it returns `NotImplemented`) — see `local_cas` for why every
            // CAS-dependent caller (the roster fan-in, the snapshot pointer)
            // needs real CAS here too, and how this wrapper provides it.
            //
            // `url.path()` is the raw, percent-*encoded* path component ("my
            // lake" comes back as "my%20lake") — handing that to `PathBuf`
            // produces a path that doesn't exist on disk for anything but plain
            // ASCII. `to_file_path()` decodes it back into the real OS path.
            let root: PathBuf = url.to_file_path().map_err(|_| {
                StoreError::Config(format!("file url {url} is not a valid local path"))
            })?;
            if root.as_os_str().is_empty() {
                return Err(StoreError::Config(format!("file url {url} has no path")));
            }
            let inner = LocalFileSystem::new_with_prefix(&root)?;
            Ok((
                Arc::new(CasLocalFileSystem::new(inner, root)),
                Path::from(""),
            ))
        }
        "memory" => Ok((Arc::new(InMemory::new()), Path::from(url.path()))),
        "s3" | "s3a" => {
            let bucket = url.host_str().ok_or_else(|| {
                StoreError::Config(format!("s3 url {url} is missing a bucket (host)"))
            })?;
            let store = s3_builder(bucket, opts).build()?;
            Ok((Arc::new(store), Path::from(url.path())))
        }
        "gs" => {
            // GCS has no MinIO-style self-hosted variant in scope, so there is no
            // endpoint override to normalize here — bucket-from-host plus whatever
            // `from_env` picks up (`GOOGLE_SERVICE_ACCOUNT`, ADC, ...) is enough.
            let bucket = url.host_str().ok_or_else(|| {
                StoreError::Config(format!("gs url {url} is missing a bucket (host)"))
            })?;
            let store = GoogleCloudStorageBuilder::from_env()
                .with_bucket_name(bucket)
                .build()?;
            Ok((Arc::new(store), Path::from(url.path())))
        }
        "az" | "abfs" | "abfss" => {
            let container = url.host_str().ok_or_else(|| {
                StoreError::Config(format!("azure url {url} is missing a container (host)"))
            })?;
            let mut builder = MicrosoftAzureBuilder::from_env().with_container_name(container);
            if let Some(v) = &opts.access_key_id {
                builder = builder.with_account(v);
            }
            if let Some(v) = &opts.secret_access_key {
                builder = builder.with_access_key(v);
            }
            if let Some(v) = &opts.endpoint {
                builder = builder.with_endpoint(v.clone());
            }
            let store = builder.build()?;
            Ok((Arc::new(store), Path::from(url.path())))
        }
        other => Err(StoreError::UnsupportedScheme(other.to_string())),
    }
}

/// A friendly identity for the backend a store URL addresses, plus any
/// operational caveats specific to *that* backend (not the generic matrix in
/// docs/storage.md, but the one-liners worth a human's attention right now).
///
/// This exists because `url.scheme()` alone collapses AWS S3, MinIO and
/// Cloudflare R2 into the same `s3` string — they all take the identical code
/// path in [`build`] (that's the point: "one setting, every S3-shaped backend
/// in scope," per this module's doc), but an operator staring at `ctxlake
/// doctor` output still wants to know *which one* they're pointed at and what
/// is worth double-checking about it. Detection here is a best-effort label
/// for display, never a branch [`build`] takes — [`build`] must keep working
/// identically regardless of what this guesses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendInfo {
    /// Human-readable backend name, e.g. `"s3-compatible (Cloudflare R2)"`.
    pub kind: &'static str,
    /// Zero or more short, backend-specific caveats. Empty when there is
    /// nothing beyond the general matrix in docs/storage.md worth surfacing.
    pub caveats: Vec<&'static str>,
}

const MINIO_NO_PUT_IF_ABSENT: &str = "no put-if-absent (minio/minio#20346) — ctxlake's own \
     coordination (the roster fan-in, the snapshot pointer) never relies on it, only on real \
     CAS (AGENTS.md invariant 4)";

const R2_CONDITIONAL_WRITE_MODE: &str = "CAS depends on the bucket's conditional-write mode \
     being ETagMatch-compatible — a bucket created in the wrong mode returns success codes for \
     writes whose condition silently did not apply (see docs/storage.md); `ctxlake doctor`'s \
     cas-update/cas-conflict-detection probes are what actually catch this, this label is only \
     a pointer to run them";

const GCS_GENERATION_PRECONDITIONS: &str = "CAS uses generation preconditions \
     (x-goog-if-generation-match), not ETags — verified by reading object_store's GCS client \
     source, not against a live bucket in this environment (see docs/storage.md)";

/// `opts.endpoint`, falling back to the same environment variables
/// [`AmazonS3Builder::from_env`] itself recognizes for an S3 endpoint override —
/// mirroring both which keys it reads and the precedence [`AmazonS3Builder::build`]
/// applies between them, not just the key names.
///
/// Two things about `from_env` are easy to get wrong here, and this function
/// exists to get them right:
///
/// 1. `from_env` filters to vars whose name starts with the literal `AWS_`
///    *before* parsing the rest as a config key (`object_store`'s
///    `aws/builder.rs::from_env`) — so bare `ENDPOINT_URL`/`ENDPOINT` (no
///    `AWS_` prefix) are names `from_env` never even looks at, even though
///    they parse to a valid `ConfigKey` on their own. A bystander env var
///    with either of those bare names (common — lots of unrelated tools set
///    `ENDPOINT`) must NOT be mistaken for an S3 endpoint override here.
/// 2. `AWS_ENDPOINT_URL_S3` maps to a *separate* field (`s3_endpoint`) from
///    `AWS_ENDPOINT_URL`/`AWS_ENDPOINT` (`endpoint`), and `build()` resolves
///    them as `s3_endpoint.or(endpoint)` — so `AWS_ENDPOINT_URL_S3` wins
///    whenever it's set, even over an `opts.endpoint` passed to
///    `with_endpoint()`, because `with_endpoint()` only ever touches the
///    `endpoint` field. That precedence has to be mirrored here in the same
///    order, not folded into the same `or_else` chain as `opts.endpoint`.
///
/// [`describe`] needs this because `ctxlake-cli`'s `store_ctx::connect` calls
/// [`build`] with a bare `BackendOptions::default()` today — nothing in
/// `ctxlake.toml` carries an endpoint field yet, so a MinIO or R2 deployment is
/// configured purely through these env vars. Without checking them too,
/// `describe` would call every S3-shaped bucket "AWS S3" the moment it's asked
/// with the same options `connect` actually uses, which defeats the point of
/// telling MinIO and R2 apart from real S3 at all.
fn resolve_s3_endpoint(opts: &BackendOptions) -> Option<String> {
    // `AWS_ENDPOINT_URL_S3` lands in `s3_endpoint`, which `build()` prefers
    // unconditionally (`s3_endpoint.or(endpoint)`) — it beats even an
    // explicit `opts.endpoint`, so it has to be checked first and alone here.
    if let Ok(v) = std::env::var("AWS_ENDPOINT_URL_S3") {
        return Some(v);
    }
    opts.endpoint.clone().or_else(|| {
        ["AWS_ENDPOINT_URL", "AWS_ENDPOINT"]
            .into_iter()
            .find_map(|k| std::env::var(k).ok())
    })
}

/// Best-effort identification of the backend `url` (plus `opts`, since an `s3://`
/// URL's real identity — AWS vs. MinIO vs. R2 — lives in the endpoint override,
/// not the scheme) addresses, for display in `ctxlake doctor` output.
///
/// Endpoint sniffing is a heuristic over a hostname substring, not a protocol
/// negotiation — a self-hosted MinIO behind a proxy with a custom domain won't
/// match `"minio"`, and that's fine: worst case this falls back to the generic
/// "S3-compatible" label, which is still accurate, just less specific.
pub fn describe(url: &Url, opts: &BackendOptions) -> BackendInfo {
    match url.scheme() {
        "s3" | "s3a" => {
            let endpoint = resolve_s3_endpoint(opts)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if endpoint.is_empty() {
                BackendInfo {
                    kind: "AWS S3",
                    caveats: vec![],
                }
            } else if endpoint.contains("r2.cloudflarestorage.com") {
                BackendInfo {
                    kind: "s3-compatible (Cloudflare R2)",
                    caveats: vec![R2_CONDITIONAL_WRITE_MODE],
                }
            } else if endpoint.contains("minio") {
                BackendInfo {
                    kind: "s3-compatible (MinIO)",
                    caveats: vec![MINIO_NO_PUT_IF_ABSENT],
                }
            } else {
                BackendInfo {
                    kind: "s3-compatible (unrecognized vendor)",
                    caveats: vec![
                        "vendor not recognized from the endpoint hostname — run `ctxlake \
                         doctor` and don't assume put-if-absent works (MinIO doesn't)",
                    ],
                }
            }
        }
        "gs" => BackendInfo {
            kind: "Google Cloud Storage",
            caveats: vec![GCS_GENERATION_PRECONDITIONS],
        },
        "az" | "abfs" | "abfss" => BackendInfo {
            kind: "Azure Blob Storage",
            caveats: vec![],
        },
        "file" => BackendInfo {
            kind: "local filesystem",
            caveats: vec![
                "CAS is emulated with flock + rename, single-host only — not a substitute \
                 for a real object store in a multi-host fleet (see docs/storage.md)",
            ],
        },
        "memory" => BackendInfo {
            kind: "in-memory (test only)",
            caveats: vec!["not durable across process restarts — never use outside tests"],
        },
        _ => BackendInfo {
            kind: "unknown",
            caveats: vec!["unrecognized store URL scheme — see docs/storage.md"],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::ObjectStoreExt;

    // `describe`'s env-var fallback reads process-wide state (`std::env::var`),
    // and `cargo test` runs a crate's tests on multiple threads of the same
    // process by default — two tests setting/clearing the same env var
    // concurrently would flake each other. This serializes just the tests that
    // touch it; every other test in this file is unaffected.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard: always restores (or removes) the env var on drop, including
    /// on an assertion panic mid-test, so one failing test can't poison every
    /// test that runs after it in the same process.
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            // SAFETY: serialized by `ENV_LOCK`, held by every caller of this guard.
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn clear(key: &'static str) -> Self {
            let previous = std::env::var(key).ok();
            // SAFETY: serialized by `ENV_LOCK`, held by every caller of this guard.
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: serialized by `ENV_LOCK`, held by every caller of this guard.
            unsafe {
                match &self.previous {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    // Bare `ENDPOINT`/`ENDPOINT_URL` are deliberately NOT in this list: they are
    // not `AWS_`-prefixed, so `AmazonS3Builder::from_env()` never reads them
    // (see `resolve_s3_endpoint`'s doc comment) and `describe` must not either.
    // They stay here only as `NON_AWS_ENDPOINT_ENV_KEYS` so tests can still
    // clear a developer's ambient `ENDPOINT` before asserting the negative.
    const S3_ENDPOINT_ENV_KEYS: [&str; 3] =
        ["AWS_ENDPOINT_URL", "AWS_ENDPOINT", "AWS_ENDPOINT_URL_S3"];
    const NON_AWS_ENDPOINT_ENV_KEYS: [&str; 2] = ["ENDPOINT_URL", "ENDPOINT"];

    /// Every `describe()` test below that doesn't itself mean to test the env
    /// fallback still runs under this: a developer's own shell (`ENDPOINT` is
    /// not an uncommon name to have set for something unrelated) must not leak
    /// into `resolve_s3_endpoint`'s ambient reads and flake an assertion about
    /// `opts.endpoint` alone.
    fn clear_s3_endpoint_env() -> Vec<EnvVarGuard> {
        S3_ENDPOINT_ENV_KEYS
            .iter()
            .chain(NON_AWS_ENDPOINT_ENV_KEYS.iter())
            .map(|k| EnvVarGuard::clear(k))
            .collect()
    }

    #[test]
    fn file_scheme_builds_a_local_store() {
        let dir = tempfile::tempdir().unwrap();
        let url = Url::from_directory_path(dir.path()).unwrap();
        let (_store, path) = build(&url, &BackendOptions::default()).unwrap();
        assert_eq!(path.as_ref(), "");
    }

    #[tokio::test]
    async fn file_scheme_handles_a_path_containing_a_space() {
        // Regression test: `url.path()` is percent-encoded ("my lake" becomes
        // "my%20lake"), so building the root from it instead of
        // `url.to_file_path()` handed `LocalFileSystem` a path that doesn't exist
        // on disk. Routine on macOS/Windows (anything under an iCloud or OneDrive
        // folder), and the local filesystem is the default backend.
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("my lake");
        std::fs::create_dir(&dir).unwrap();
        let url = Url::from_directory_path(&dir).unwrap();
        assert!(
            url.path().contains("%20"),
            "test assumption: the URL's path component is percent-encoded: {url}"
        );

        let (store, _path) = build(&url, &BackendOptions::default())
            .unwrap_or_else(|e| panic!("expected a space in the path to work fine: {e}"));

        // Prove it's actually rooted at the real directory, not just that
        // construction didn't panic: round-trip a write through it.
        store
            .put(
                &Path::from("probe.json"),
                object_store::PutPayload::from_static(b"{}"),
            )
            .await
            .unwrap();
        assert!(
            dir.join("probe.json").exists(),
            "write should have landed in the real 'my lake' directory"
        );
    }

    #[test]
    fn s3_scheme_sets_etag_conditional_put_even_when_unset_by_caller() {
        // The whole point of invariant 4 is that this is not an opt-in the caller
        // can forget: MinIO breaks with the default `Disabled`-adjacent behavior of
        // trusting `If-None-Match: *`, so ETagMatch must be the one we always ask
        // for, not a flag someone has to remember to pass.
        let url = Url::parse("s3://my-bucket/ctxlake").unwrap();
        let opts = BackendOptions {
            endpoint: Some("http://localhost:9000".into()),
            allow_http: true,
            ..Default::default()
        };
        let (store, path) = build(&url, &opts).unwrap();
        assert_eq!(path.as_ref(), "ctxlake");
        // `build()`'s success alone doesn't distinguish ETagMatch from any other
        // mode — object_store's `AmazonS3` client has no public accessor for it
        // either, so this can only ever show "construction didn't error". The
        // actual conditional-put mode is asserted below, on the builder, in
        // `s3_builder_forces_etag_conditional_put_for_minio_and_r2_alike` and
        // `s3_builder_overrides_a_hostile_aws_conditional_put_env_var`.
        let _ = store;
    }

    #[test]
    fn r2_endpoint_builds_with_the_same_etag_conditional_put_as_minio() {
        // R2 speaks the S3 API through the exact same `s3`/`s3a` scheme as MinIO —
        // there is no `r2://` branch in `build()`, by design (this module's doc:
        // "one setting, every S3-shaped backend in scope"). This test exists so
        // that claim stays checked: an R2-shaped endpoint (the real
        // `<account_id>.r2.cloudflarestorage.com` hostname pattern) must build
        // exactly as readily as a MinIO endpoint does, with no separate code path
        // to fall out of sync. This only asserts construction succeeds — see
        // `s3_builder_forces_etag_conditional_put_for_minio_and_r2_alike` below for
        // the actual conditional-put-mode assertion.
        let url = Url::parse("s3://my-bucket/ctxlake").unwrap();
        let opts = BackendOptions {
            endpoint: Some("https://abc123.r2.cloudflarestorage.com".into()),
            virtual_hosted_style_request: true,
            ..Default::default()
        };
        let (store, path) = build(&url, &opts).unwrap();
        assert_eq!(path.as_ref(), "ctxlake");
        let _ = store;
    }

    #[test]
    fn s3_builder_forces_etag_conditional_put_for_minio_and_r2_alike() {
        // Regression: docs/storage.md used to claim the conditional-put mode
        // itself was "verified by running" the two tests above — it wasn't; both
        // only ever checked that `build()` returned `Ok`, which stays true even
        // with `.with_conditional_put(ETagMatch)` deleted outright (ETagMatch is
        // `object_store`'s own `#[default]`). `get_config_value` is the one place
        // `object_store` actually exposes this — on the *builder*, not on the
        // built client — which is why `build` was split out into `s3_builder`.
        use object_store::aws::AmazonS3ConfigKey;
        for endpoint in [
            "http://localhost:9000",
            "https://abc123.r2.cloudflarestorage.com",
        ] {
            let opts = BackendOptions {
                endpoint: Some(endpoint.into()),
                ..Default::default()
            };
            let value =
                s3_builder("my-bucket", &opts).get_config_value(&AmazonS3ConfigKey::ConditionalPut);
            assert_eq!(
                value.as_deref(),
                Some("etag"),
                "endpoint {endpoint} must build with ETagMatch conditional-put, got {value:?}"
            );
        }
    }

    #[test]
    fn s3_builder_overrides_a_hostile_aws_conditional_put_env_var() {
        // Regression: `AmazonS3Builder::from_env()` reads `AWS_CONDITIONAL_PUT`
        // into the exact same field `.with_conditional_put()` sets. Nothing in
        // ctxlake sets that var, but nothing stops an operator's shell from
        // carrying it for an unrelated reason — this is the case the explicit
        // `.with_conditional_put(ETagMatch)` call in `s3_builder` (placed *after*
        // `from_env()`) exists to defend against: without it, `AWS_CONDITIONAL_
        // PUT=disabled` in the environment would silently reintroduce the
        // wildcard `If-None-Match: *` semantics AGENTS.md invariant 4 forbids.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _clean = clear_s3_endpoint_env();
        let _env = EnvVarGuard::set("AWS_CONDITIONAL_PUT", "disabled");

        use object_store::aws::AmazonS3ConfigKey;
        let opts = BackendOptions {
            endpoint: Some("http://localhost:9000".into()),
            ..Default::default()
        };
        let value =
            s3_builder("my-bucket", &opts).get_config_value(&AmazonS3ConfigKey::ConditionalPut);
        assert_eq!(
            value.as_deref(),
            Some("etag"),
            "an env-supplied AWS_CONDITIONAL_PUT=disabled must not survive s3_builder, got {value:?}"
        );
    }

    #[test]
    fn gs_scheme_builds_a_gcs_store() {
        let url = Url::parse("gs://my-bucket/ctxlake").unwrap();
        let (store, path) = build(&url, &BackendOptions::default()).unwrap();
        assert_eq!(path.as_ref(), "ctxlake");
        let _ = store;
    }

    #[test]
    fn gs_url_missing_a_bucket_is_reported_not_panicked() {
        let url = Url::parse("gs:/no-host").unwrap();
        let err = build(&url, &BackendOptions::default());
        assert!(err.is_err());
    }

    #[test]
    fn describe_recognizes_aws_s3_by_scheme_with_no_endpoint_override() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _clean = clear_s3_endpoint_env();

        let url = Url::parse("s3://my-bucket/ctxlake").unwrap();
        let info = describe(&url, &BackendOptions::default());
        assert_eq!(info.kind, "AWS S3");
        assert!(info.caveats.is_empty());
    }

    #[test]
    fn describe_recognizes_minio_from_the_endpoint_hostname() {
        // A bare "http://localhost:9000" (this crate's own CI/dev default,
        // AGENTS.md's "hard-won facts") has nothing in it that says "MinIO" —
        // the hostname is generic. This test uses the shape a self-hosted
        // deployment's endpoint actually carries (a "minio" hostname segment,
        // e.g. docker-compose's service-name-as-DNS-name convention) so the
        // heuristic has something real to match; the localhost case correctly
        // falls through to the generic "S3-compatible" label instead (covered
        // by `describe_falls_back_to_generic_s3_compatible_for_an_unrecognized_endpoint`).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _clean = clear_s3_endpoint_env();

        let url = Url::parse("s3://my-bucket/ctxlake").unwrap();
        let opts = BackendOptions {
            endpoint: Some("http://minio.internal:9000".into()),
            ..Default::default()
        };
        let info = describe(&url, &opts);
        assert_eq!(info.kind, "s3-compatible (MinIO)");
        assert!(
            info.caveats.iter().any(|c| c.contains("put-if-absent")),
            "MinIO's missing put-if-absent (minio/minio#20346) must be surfaced: {:?}",
            info.caveats
        );
    }

    #[test]
    fn describe_recognizes_r2_from_the_endpoint_hostname() {
        // Regression guard: this is the "R2 needs the same treatment as MinIO"
        // requirement, checked at the level that actually matters for an
        // operator reading `ctxlake doctor` output — the right *label and
        // caveat*, not just that `build()` didn't panic (covered above).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _clean = clear_s3_endpoint_env();

        let url = Url::parse("s3://my-bucket/ctxlake").unwrap();
        let opts = BackendOptions {
            endpoint: Some("https://abc123.r2.cloudflarestorage.com".into()),
            ..Default::default()
        };
        let info = describe(&url, &opts);
        assert_eq!(info.kind, "s3-compatible (Cloudflare R2)");
        assert!(
            info.caveats
                .iter()
                .any(|c| c.contains("conditional-write mode")),
            "R2's conditional-write-mode footgun must be surfaced: {:?}",
            info.caveats
        );
    }

    #[test]
    fn describe_falls_back_to_the_same_env_vars_from_env_reads_for_the_endpoint() {
        // `store_ctx::connect` (ctxlake-cli) calls `build` with a bare
        // `BackendOptions::default()` — nothing in `ctxlake.toml` carries an
        // `endpoint` field today, so a real MinIO/R2 deployment's endpoint comes
        // in purely through `AmazonS3Builder::from_env()`'s env vars. If
        // `describe` only looked at `opts.endpoint`, it would call every such
        // bucket "AWS S3" under the exact options `connect` actually uses —
        // this guards against that regression by exercising `describe` the same
        // way: default `BackendOptions`, endpoint supplied via env var alone.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _clean = clear_s3_endpoint_env();
        let _env = EnvVarGuard::set("AWS_ENDPOINT_URL", "http://minio.internal:9000");

        let url = Url::parse("s3://my-bucket/ctxlake").unwrap();
        let info = describe(&url, &BackendOptions::default());
        assert_eq!(
            info.kind, "s3-compatible (MinIO)",
            "an endpoint set only via AWS_ENDPOINT_URL must still be detected"
        );
    }

    #[test]
    fn describe_prefers_an_explicit_opts_endpoint_over_the_environment() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _clean = clear_s3_endpoint_env();
        let _env = EnvVarGuard::set("AWS_ENDPOINT_URL", "http://minio.internal:9000");

        let url = Url::parse("s3://my-bucket/ctxlake").unwrap();
        let opts = BackendOptions {
            endpoint: Some("https://abc123.r2.cloudflarestorage.com".into()),
            ..Default::default()
        };
        let info = describe(&url, &opts);
        assert_eq!(
            info.kind, "s3-compatible (Cloudflare R2)",
            "an explicit endpoint in BackendOptions must win over the ambient environment"
        );
    }

    #[test]
    fn describe_ignores_bare_endpoint_env_vars_that_from_env_never_reads() {
        // Regression: `AmazonS3Builder::from_env()` filters to `AWS_`-prefixed
        // names *before* parsing the rest as a config key, so bare `ENDPOINT`/
        // `ENDPOINT_URL` (no prefix) are never read by the builder `build()`
        // actually uses, even though the same strings parse to a valid
        // `ConfigKey` on their own. If `describe` read them anyway, an operator
        // pointed at real AWS S3 whose shell happens to export an unrelated
        // `ENDPOINT` (common — e.g. many dev tools set this) would be told
        // they're on MinIO and shown a caveat for a backend they aren't using.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _clean = clear_s3_endpoint_env();
        let _env1 = EnvVarGuard::set("ENDPOINT", "http://minio.internal:9000");
        let _env2 = EnvVarGuard::set("ENDPOINT_URL", "http://minio.internal:9000");

        let url = Url::parse("s3://my-bucket/ctxlake").unwrap();
        let info = describe(&url, &BackendOptions::default());
        assert_eq!(
            info.kind, "AWS S3",
            "bare ENDPOINT/ENDPOINT_URL are not read by AmazonS3Builder::from_env() \
             and must not be read by describe() either — got {info:?}"
        );
    }

    #[test]
    fn describe_prefers_aws_endpoint_url_s3_even_over_an_explicit_opts_endpoint() {
        // Regression: object_store's `AmazonS3Builder::build()` resolves the
        // endpoint as `self.s3_endpoint.or(self.endpoint)` — `AWS_ENDPOINT_URL_S3`
        // (which lands in `s3_endpoint`) wins unconditionally, even over an
        // explicit `opts.endpoint` passed through `with_endpoint()` (which only
        // ever sets `self.endpoint`, never `self.s3_endpoint`). `describe` has
        // to mirror that precedence or it labels a bucket by an endpoint that
        // `build()` will not actually connect to.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _clean = clear_s3_endpoint_env();
        let _env = EnvVarGuard::set("AWS_ENDPOINT_URL_S3", "http://minio.internal:9000");

        let url = Url::parse("s3://my-bucket/ctxlake").unwrap();
        let opts = BackendOptions {
            endpoint: Some("https://abc123.r2.cloudflarestorage.com".into()),
            ..Default::default()
        };
        let info = describe(&url, &opts);
        assert_eq!(
            info.kind, "s3-compatible (MinIO)",
            "AWS_ENDPOINT_URL_S3 must win over opts.endpoint, matching \
             AmazonS3Builder::build()'s s3_endpoint.or(endpoint) precedence — got {info:?}"
        );
    }

    #[test]
    fn describe_falls_back_to_generic_s3_compatible_for_an_unrecognized_endpoint() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _clean = clear_s3_endpoint_env();

        let url = Url::parse("s3://my-bucket/ctxlake").unwrap();
        let opts = BackendOptions {
            endpoint: Some("https://storage.example-vendor.net".into()),
            ..Default::default()
        };
        let info = describe(&url, &opts);
        assert_eq!(info.kind, "s3-compatible (unrecognized vendor)");
    }

    #[test]
    fn describe_recognizes_gcs_and_flags_generation_preconditions_as_unverified_live() {
        let url = Url::parse("gs://my-bucket/ctxlake").unwrap();
        let info = describe(&url, &BackendOptions::default());
        assert_eq!(info.kind, "Google Cloud Storage");
        assert!(
            info.caveats
                .iter()
                .any(|c| c.contains("generation") && c.contains("not against a live bucket")),
            "GCS's verification basis (source-read, not live) must be honest: {:?}",
            info.caveats
        );
    }

    #[test]
    fn describe_recognizes_the_local_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let url = Url::from_directory_path(dir.path()).unwrap();
        let info = describe(&url, &BackendOptions::default());
        assert_eq!(info.kind, "local filesystem");
    }

    #[test]
    fn unsupported_scheme_is_reported_not_panicked() {
        let url = Url::parse("ftp://example.com/x").unwrap();
        let err = build(&url, &BackendOptions::default()).unwrap_err();
        assert!(matches!(err, StoreError::UnsupportedScheme(_)));
    }

    #[test]
    fn missing_bucket_is_reported_not_panicked() {
        // `s3:///no-host` — no authority segment at all.
        let url = Url::parse("s3:/no-host").unwrap();
        let err = build(&url, &BackendOptions::default());
        assert!(err.is_err());
    }
}
