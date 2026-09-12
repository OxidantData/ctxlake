//! Real CAS on top of [`LocalFileSystem`] — which, in `object_store` 0.14, does not
//! implement `PutMode::Update` at all (it returns `Error::NotImplemented`; only
//! `Create`, via an atomic hard link, and `Overwrite`, via an atomic rename, are
//! supported). The local filesystem is a documented backend (`docs/storage.md`) and
//! the CAS-torture suite's default target, so leases need to actually work there.
//!
//! POSIX gives no primitive for "replace this file's contents only if they still
//! match X" — `rename(2)` is atomic but unconditional, and `link(2)` is atomic but
//! only for *creating*, not for updating. The standard way to build a conditional
//! update out of unconditional primitives on a single machine is a real OS lock:
//! [`std::fs::File::lock`] (an `flock`-family advisory lock, stable since Rust
//! 1.89) held around a check-then-write sequence. This is legitimate specifically
//! *because* it is single-machine: unlike the CAS primitives this crate uses
//! against S3/GCS/Azure, an advisory lock does not survive a shared network
//! filesystem or a second machine touching the same path, so this wrapper is scoped
//! to genuinely-local use (dev, tests, a single-host deployment) — the same scope
//! `file://` has everywhere else in this project.
//!
//! Every other operation (`get`, `list`, `delete`, `Create`, `Overwrite`, ...)
//! passes straight through to the inner [`LocalFileSystem`] unchanged.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::path::{Path as StdPath, PathBuf};

use async_trait::async_trait;
use futures::stream::BoxStream;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{
    CopyOptions, Error as OsError, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, ObjectStoreExt, PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult,
    Result as OsResult,
};

#[derive(Debug)]
pub(crate) struct CasLocalFileSystem {
    inner: LocalFileSystem,
    /// The same directory `inner` was rooted at (`LocalFileSystem::new_with_prefix`
    /// keeps no public accessor for this), needed to place a lock file next to each
    /// object without duplicating `inner`'s own path-escaping logic — the lock
    /// file's name is derived from the already-escaped [`Path`], not from raw
    /// input, so it inherits the same traversal safety as every other key here.
    root: PathBuf,
}

impl fmt::Display for CasLocalFileSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CasLocalFileSystem({})", self.inner)
    }
}

impl CasLocalFileSystem {
    pub(crate) fn new(inner: LocalFileSystem, root: PathBuf) -> Self {
        Self { inner, root }
    }

    fn lock_file_path(&self, location: &Path) -> PathBuf {
        let mut os_string = self.root.join(location.as_ref()).into_os_string();
        os_string.push(".ctxlake-lock");
        PathBuf::from(os_string)
    }
}

fn io_err(context: &'static str, path: &Path, source: std::io::Error) -> OsError {
    OsError::Generic {
        store: "CasLocalFileSystem",
        source: format!("{context} for {path}: {source}").into(),
    }
}

/// Acquire an exclusive lock on `lock_path`, creating it (and its parent
/// directory) if needed. Blocking, so callers run it via `spawn_blocking`.
fn acquire_lock(lock_path: &StdPath) -> std::io::Result<File> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    file.lock()?;
    Ok(file)
}

#[async_trait]
impl ObjectStore for CasLocalFileSystem {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> OsResult<PutResult> {
        let PutMode::Update(expected) = &opts.mode else {
            // Create (hard link) and Overwrite (rename) are both already atomic on
            // LocalFileSystem — nothing to add.
            return self.inner.put_opts(location, payload, opts).await;
        };
        let expected_etag = expected.e_tag.clone();

        let lock_path = self.lock_file_path(location);
        // The lock must be held across the whole read-check-write sequence, or two
        // Updates could interleave between the check and the write — exactly the
        // race this wrapper exists to close. `spawn_blocking` only wraps the
        // acquisition; the `File` (and thus the OS lock) is carried forward and
        // dropped — releasing it — after the write below completes.
        let location_owned = location.clone();
        let lock_file = tokio::task::spawn_blocking(move || acquire_lock(&lock_path))
            .await
            .map_err(|e| OsError::Generic {
                store: "CasLocalFileSystem",
                source: format!("lock task panicked: {e}").into(),
            })?
            .map_err(|e| io_err("acquiring lease lock", &location_owned, e))?;

        let current_etag = match self.inner.head(location).await {
            Ok(meta) => meta.e_tag,
            Err(OsError::NotFound { .. }) => None,
            Err(e) => {
                drop(lock_file);
                return Err(e);
            }
        };

        if current_etag.is_none() || current_etag != expected_etag {
            drop(lock_file);
            return Err(OsError::Precondition {
                path: location.to_string(),
                source: format!("expected e_tag {expected_etag:?}, found {current_etag:?}").into(),
            });
        }

        let result = self
            .inner
            .put_opts(location, payload, PutMode::Overwrite.into())
            .await;
        drop(lock_file); // explicit: makes the lock's scope match the CAS window, not fn exit.
        result
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> OsResult<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> OsResult<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, OsResult<Path>>,
    ) -> BoxStream<'static, OsResult<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, OsResult<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> OsResult<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> OsResult<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::UpdateVersion;

    fn store(dir: &std::path::Path) -> CasLocalFileSystem {
        let inner = LocalFileSystem::new_with_prefix(dir).unwrap();
        CasLocalFileSystem::new(inner, dir.to_path_buf())
    }

    #[tokio::test]
    async fn update_succeeds_against_the_version_just_written() {
        let dir = tempfile::tempdir().unwrap();
        let fs = store(dir.path());
        let key = Path::from("lease.json");

        let created = fs.put(&key, PutPayload::from_static(b"v1")).await.unwrap();
        let version = UpdateVersion::from(created);
        fs.put_opts(
            &key,
            PutPayload::from_static(b"v2"),
            PutMode::Update(version).into(),
        )
        .await
        .unwrap();
        assert_eq!(
            fs.get(&key).await.unwrap().bytes().await.unwrap().as_ref(),
            b"v2"
        );
    }

    #[tokio::test]
    async fn update_against_a_stale_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let fs = store(dir.path());
        let key = Path::from("lease.json");

        let created = fs.put(&key, PutPayload::from_static(b"v1")).await.unwrap();
        let stale_version = UpdateVersion::from(created);
        fs.put(&key, PutPayload::from_static(b"v2")).await.unwrap(); // advances the real e_tag

        let result = fs
            .put_opts(
                &key,
                PutPayload::from_static(b"v3"),
                PutMode::Update(stale_version).into(),
            )
            .await;
        assert!(matches!(result, Err(OsError::Precondition { .. })));
        assert_eq!(
            fs.get(&key).await.unwrap().bytes().await.unwrap().as_ref(),
            b"v2"
        );
    }

    #[tokio::test]
    async fn concurrent_updates_against_the_same_key_never_both_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let fs = std::sync::Arc::new(store(dir.path()));
        let key = Path::from("lease.json");
        let created = fs.put(&key, PutPayload::from_static(b"v0")).await.unwrap();
        let version = UpdateVersion::from(created);

        let a = {
            let fs = fs.clone();
            let key = key.clone();
            let version = version.clone();
            tokio::spawn(async move {
                fs.put_opts(
                    &key,
                    PutPayload::from_static(b"from-a"),
                    PutMode::Update(version).into(),
                )
                .await
            })
        };
        let b = {
            let fs = fs.clone();
            let key = key.clone();
            tokio::spawn(async move {
                fs.put_opts(
                    &key,
                    PutPayload::from_static(b"from-b"),
                    PutMode::Update(version).into(),
                )
                .await
            })
        };
        let (ra, rb) = tokio::join!(a, b);
        let successes = [ra.unwrap(), rb.unwrap()]
            .into_iter()
            .filter(Result::is_ok)
            .count();
        assert_eq!(
            successes, 1,
            "exactly one racing Update against the same stale version may win"
        );
    }
}
