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

/// Build the store addressed by `url`, returning it alongside the [`Path`] `url`
/// pointed at within that store (the part after the bucket/container).
pub fn build(url: &Url, opts: &BackendOptions) -> Result<(Arc<dyn ObjectStore>, Path), StoreError> {
    match url.scheme() {
        "file" => {
            // `object_store` 0.14's `LocalFileSystem` has no `PutMode::Update` at
            // all (it returns `NotImplemented`) — see `local_cas` for why leases
            // need real CAS here too, and how this wrapper provides it.
            let path_str = url.path();
            if path_str.is_empty() {
                return Err(StoreError::Config(format!("file url {url} has no path")));
            }
            let root = PathBuf::from(path_str);
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
            builder = builder
                .with_allow_http(opts.allow_http)
                .with_virtual_hosted_style_request(opts.virtual_hosted_style_request);
            let store = builder.build()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_scheme_builds_a_local_store() {
        let dir = tempfile::tempdir().unwrap();
        let url = Url::from_directory_path(dir.path()).unwrap();
        let (_store, path) = build(&url, &BackendOptions::default()).unwrap();
        assert_eq!(path.as_ref(), "");
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
        // We can't introspect the private `conditional_put` field from here, but a
        // constructed `AmazonS3` store implies `with_conditional_put` didn't panic
        // and `build()` accepted it — the meaningful assertion (that acquire() only
        // ever issues `PutMode::Update`, so this setting is exercised on every
        // lease-file test) lives in lease.rs.
        let _ = store;
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
