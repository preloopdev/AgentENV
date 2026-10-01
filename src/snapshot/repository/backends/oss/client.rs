use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bytes::Bytes;
use futures::{stream, TryStreamExt};
use object_store_operator::{
    build_object_store_operator, run_with_refresh, AddressingStyle, CachedCredentialSource,
    CredentialSource, ObjectStoreOperatorConfig, ObjectStoreOperatorError, OperatorWithCredential,
};
use opendal::{Error as OpenDalError, ErrorKind as OpenDalErrorKind, Operator};
use overlaybd::backend::oss::upload_file_streaming;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tracing::{info, warn};
use url::Url;

use super::config::NormalizedOssFallback;
use crate::observability::prometheus::MetricGuard;

/// Multipart part size for streaming file uploads. S3/OSS caps a multipart
/// upload at 10,000 parts, so this bounds the largest uploadable object
/// (~625 GiB at 64 MiB). Must be passed explicitly to opendal via
/// `writer_with().chunk()`: without it opendal falls back to the service's
/// minimum multipart part size (5 MiB), capping uploads at ~50 GiB.
///
/// Note the memory cost, which is `(2 * UPLOAD_CONCURRENCY + 2) * CHUNK_SIZE`
/// rather than the product of the two — **measured at 1040 MiB for these
/// figures**. See `overlaybd::backend::oss::upload_file_streaming` for why.
/// Deliberately left as it was when the upload loop moved there: shrinking it
/// would also shrink the largest uploadable object.
const CHUNK_SIZE: usize = 64 * 1024 * 1024;
/// Number of multipart parts uploaded concurrently per file. A single
/// sequential stream tops out at roughly 100 MB/s to the OSS internal
/// endpoint; concurrent parts multiply effective throughput.
const UPLOAD_CONCURRENCY: usize = 8;
const OSS_OPERATION_DURATION: &str = "agentenv_snapshot_oss_operation_duration_seconds";

/// Snapshot artifacts uploaded to OSS. Used as the `artifact` label on upload
/// metrics and in upload completion logs so memory layers can be told apart
/// from rootfs/attached-drive layers.
#[derive(Clone, Copy, Debug)]
pub(crate) enum OssUploadArtifact {
    RootfsLayer,
    AttachedDriveLayer,
    MemoryLayer,
    VmState,
    FirecrackerManifest,
    CatalogRecord,
    Alias,
}

impl OssUploadArtifact {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::RootfsLayer => "rootfs_layer",
            Self::AttachedDriveLayer => "attached_drive_layer",
            Self::MemoryLayer => "memory_layer",
            Self::VmState => "vm_state",
            Self::FirecrackerManifest => "manifest",
            Self::CatalogRecord => "record",
            Self::Alias => "alias",
        }
    }
}

/// Read routing for a repository-relative object key.
///
/// Immutable content-addressed objects (managed layer blobs) may fall back on
/// `NotFound`; mutable catalog/volume records must not, because a stale mirror
/// could resurrect deleted state or roll back a head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyClass {
    Immutable,
    Mutable,
}

/// Repository-relative prefix of content-addressed managed layer blobs.
const MANAGED_LAYERS_PREFIX: &str = "managed-layers/";

pub(crate) fn classify_key(key: &str) -> KeyClass {
    if key
        .trim_start_matches('/')
        .starts_with(MANAGED_LAYERS_PREFIX)
    {
        KeyClass::Immutable
    } else {
        KeyClass::Mutable
    }
}

/// Cached operator plus the primary-read timeout it was built with.
///
/// Reads bound the primary attempt with a shorter timeout when a fallback is
/// configured, so the cache must be invalidated when that timeout changes;
/// otherwise the first-cached operator would silently keep the old timeout.
type CachedOperator = Arc<RwLock<Option<(Option<Duration>, OperatorWithCredential)>>>;

/// Read-only mirror endpoint plus its circuit-breaker state.
///
/// Shared across clones of [`OssClient`] so a single failover decision covers
/// every request handled by the process.
#[derive(Clone, Debug)]
struct OssReadFallback {
    operator_config: ObjectStoreOperatorConfig,
    prefix: String,
    credentials: Arc<CachedCredentialSource>,
    cached_operator: CachedOperator,
    cooldown: Duration,
    primary_timeout: Duration,
    /// Unix-millis until which reads bypass the primary. 0 = primary eligible.
    unavailable_until: Arc<AtomicU64>,
}

impl OssReadFallback {
    fn is_tripped(&self) -> bool {
        self.unavailable_until.load(Ordering::Relaxed) > now_unix_millis()
    }

    fn has_tripped(&self) -> bool {
        self.unavailable_until.load(Ordering::Relaxed) != 0
    }

    fn trip(&self) {
        let until = now_unix_millis() + self.cooldown.as_millis() as u64;
        self.unavailable_until.store(until, Ordering::Relaxed);
    }

    fn clear(&self) {
        self.unavailable_until.store(0, Ordering::Relaxed);
    }
}

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Whether an error means the primary endpoint could not be reached at all, as
/// opposed to a definitive response (auth failure, precondition, not found).
pub(crate) fn is_unavailable_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        if let Some(opendal_error) = cause.downcast_ref::<OpenDalError>() {
            return is_unavailable_kind(opendal_error.kind());
        }
        if let Some(ObjectStoreOperatorError::OpenDal(opendal_error)) =
            cause.downcast_ref::<ObjectStoreOperatorError>()
        {
            return is_unavailable_kind(opendal_error.kind());
        }
        if cause
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
        {
            return true;
        }
        false
    })
}

fn is_unavailable_kind(kind: OpenDalErrorKind) -> bool {
    matches!(
        kind,
        OpenDalErrorKind::Unexpected | OpenDalErrorKind::RateLimited
    )
}

/// What to do when a primary read fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadFailure {
    /// Primary is unavailable: trip the breaker and read from the mirror.
    TripFallback,
    /// Immutable object missing on the primary: try the mirror once.
    NotFoundFallback,
    /// Definitive failure (auth, precondition, mutable NotFound): surface it.
    Propagate,
}

fn classify_read_failure(class: KeyClass, error: &anyhow::Error) -> ReadFailure {
    if is_unavailable_error(error) {
        ReadFailure::TripFallback
    } else if class == KeyClass::Immutable && OssClient::is_not_found_error(error) {
        ReadFailure::NotFoundFallback
    } else {
        ReadFailure::Propagate
    }
}

/// Thin wrapper around the OSS client used by the repository and resolver.
#[derive(Clone, Debug)]
pub(crate) struct OssClient {
    operator_config: ObjectStoreOperatorConfig,
    prefix: String,
    credentials: Arc<CachedCredentialSource>,
    cached_operator: CachedOperator,
    fallback: Option<OssReadFallback>,
}

impl OssClient {
    /// Convenience constructor without a fallback, used by tests.
    #[cfg(test)]
    pub(crate) fn new(
        bucket: String,
        endpoint: String,
        region: String,
        prefix: String,
        credential_source: CredentialSource,
        addressing_override: Option<AddressingStyle>,
    ) -> Result<Self> {
        Self::new_with_fallback(
            bucket,
            endpoint,
            region,
            prefix,
            credential_source,
            addressing_override,
            None,
        )
    }

    pub(crate) fn new_with_fallback(
        bucket: String,
        endpoint: String,
        region: String,
        prefix: String,
        credential_source: CredentialSource,
        addressing_override: Option<AddressingStyle>,
        fallback: Option<&NormalizedOssFallback>,
    ) -> Result<Self> {
        let operator_config =
            build_operator_config(&bucket, &endpoint, &region, addressing_override)?;
        let fallback = match fallback {
            Some(fallback) => {
                let fallback_config = build_operator_config(
                    fallback.bucket(),
                    fallback.endpoint(),
                    fallback.region(),
                    fallback.addressing_style(),
                )?;
                Some(OssReadFallback {
                    operator_config: fallback_config,
                    prefix: fallback.prefix().to_string(),
                    credentials: Arc::new(CachedCredentialSource::new(
                        fallback.credential_source(),
                    )),
                    cached_operator: Arc::new(RwLock::new(None)),
                    cooldown: fallback.cooldown(),
                    primary_timeout: fallback.primary_timeout(),
                    unavailable_until: Arc::new(AtomicU64::new(0)),
                })
            }
            None => None,
        };
        Ok(Self {
            operator_config,
            prefix,
            credentials: Arc::new(CachedCredentialSource::new(credential_source)),
            cached_operator: Arc::new(RwLock::new(None)),
            fallback,
        })
    }

    fn full_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{}", self.prefix, key)
        }
    }

    pub(crate) fn managed_layers_repo_blob_url(&self) -> String {
        // overlaybd expects an S3-compatible repo blob URL here, including for
        // Alibaba OSS, so the scheme remains `s3://` rather than `oss://`.
        if self.prefix.is_empty() {
            format!("s3://{}/managed-layers", self.operator_config.bucket)
        } else {
            format!(
                "s3://{}/{}/managed-layers",
                self.operator_config.bucket, self.prefix
            )
        }
    }

    /// Read a small object entirely into memory.
    pub(crate) async fn get_bytes(&self, key: &str) -> Result<Bytes> {
        let mut metric = MetricGuard::operation(OSS_OPERATION_DURATION, "get_bytes");
        let result = self
            .run_read(key, classify_key(key), |operator, target| async move {
                operator
                    .read(&target.full_key())
                    .await
                    .map(|b| b.to_bytes())
            })
            .await
            .with_context(|| format!("oss get '{key}'"));
        metric.finish(&result);
        result
    }

    /// Reads a small object together with its backend version token for a
    /// conditional update.
    ///
    /// Deliberately primary-only: the token feeds an `If-Match`/`If-None-Match`
    /// compare-and-swap that writes to the primary, so a mirror version would be
    /// meaningless and could corrupt the swap.
    pub(crate) async fn get_bytes_with_etag(&self, key: &str) -> Result<(Bytes, Option<String>)> {
        let full_key = self.full_key(key);
        self.run_operation(|operator| {
            let full_key = full_key.clone();
            async move {
                for _attempt in 0..5 {
                    let metadata = operator.stat(&full_key).await?;
                    let etag = metadata.etag().map(str::to_owned);
                    let read = match etag.as_deref() {
                        Some(etag) => operator.read_with(&full_key).if_match(etag).await,
                        None => operator.read(&full_key).await,
                    };
                    match read {
                        Ok(bytes) => return Ok((bytes.to_bytes(), etag)),
                        Err(error) if error.kind() == OpenDalErrorKind::ConditionNotMatch => {
                            continue
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(OpenDalError::new(
                    OpenDalErrorKind::Unexpected,
                    "object changed too often while reading its version",
                ))
            }
        })
        .await
        .with_context(|| format!("oss get versioned object '{key}'"))
    }

    /// Download an object directly to a local file (atomic: temp + rename).
    pub(crate) async fn get_to_file(&self, key: &str, dest: &Path) -> Result<u64> {
        let mut metric = MetricGuard::operation(OSS_OPERATION_DURATION, "get_to_file");
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create cache dir '{}'", parent.display()))?;
        }

        let dest = dest.to_path_buf();
        let result = self
            .run_read(key, classify_key(key), |operator, target| {
                let dest = dest.clone();
                async move { download_object_to_file(&operator, &target.full_key(), &dest).await }
            })
            .await
            .with_context(|| format!("oss download '{key}'"));
        metric.finish(&result);
        result
    }

    /// Check whether an object exists.
    ///
    /// `opendal` reports "absent" as `Ok(false)` rather than an error, so the
    /// immutable-content-addressed fallback on absence is handled explicitly
    /// here: a healthy primary that lacks a managed layer still consults the
    /// durable mirror.
    pub(crate) async fn exists(&self, key: &str) -> Result<bool> {
        let mut metric = MetricGuard::operation(OSS_OPERATION_DURATION, "exists");
        let class = classify_key(key);
        let result = async {
            let present = self
                .run_read(key, class, |operator, target| async move {
                    operator.exists(&target.full_key()).await
                })
                .await
                .with_context(|| format!("oss exists '{key}'"))?;
            if present || class != KeyClass::Immutable {
                return Ok(present);
            }
            match self.fallback.as_ref() {
                Some(fallback) if !fallback.is_tripped() => {
                    // Primary answered "absent"; the mirror may still hold it.
                    match self.run_fallback_exists(key).await {
                        Ok(found) => Ok(found),
                        Err(error) => {
                            warn!(
                                key = %key,
                                error = %error,
                                "fallback existence probe failed; treating immutable object as absent"
                            );
                            Ok(false)
                        }
                    }
                }
                _ => Ok(false),
            }
        }
        .await;
        metric.finish(&result);
        result
    }

    async fn run_fallback_exists(&self, key: &str) -> Result<bool> {
        let fallback = self
            .fallback
            .as_ref()
            .expect("fallback existence probe requires a configured fallback");
        let target = ReadTarget::new(key, &fallback.prefix);
        Self::run_operation_on(
            &fallback.operator_config,
            &fallback.credentials,
            &fallback.cached_operator,
            |operator| {
                let full_key = target.full_key();
                async move { operator.exists(&full_key).await }
            },
        )
        .await
    }

    /// List all files recursively under a prefix, returning repository-relative
    /// keys.
    pub(crate) async fn list_keys_recursive(&self, prefix: &str) -> Result<Vec<String>> {
        self.run_read(prefix, KeyClass::Mutable, |operator, target| async move {
            let entries = operator
                .list_with(&target.full_key())
                .recursive(true)
                .await?;
            let strip = target.strip_prefix();
            Ok(entries
                .into_iter()
                .filter(|entry| !entry.metadata().mode().is_dir())
                .map(|entry| entry.path().to_string())
                .map(|path| match strip.as_deref() {
                    Some(strip) => path.strip_prefix(strip).unwrap_or(&path).to_string(),
                    None => path,
                })
                .collect())
        })
        .await
        .with_context(|| format!("oss list '{prefix}'"))
    }

    /// Lists at most `limit` files after a repository-relative key, returning
    /// repository-relative keys.
    pub(crate) async fn list_keys_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>> {
        self.run_read(prefix, KeyClass::Mutable, move |operator, target| {
            let start_after = start_after.map(str::to_string);
            async move {
                let full_prefix = target.full_key();
                let builder = operator
                    .lister_with(&full_prefix)
                    .recursive(true)
                    .limit(limit);
                let mut lister = match start_after.as_deref() {
                    Some(key) => builder.start_after(&target.full_key_of(key)).await?,
                    None => builder.await?,
                };
                let strip = target.strip_prefix();
                let mut keys = Vec::with_capacity(limit);
                while keys.len() < limit {
                    let Some(entry) = lister.try_next().await? else {
                        break;
                    };
                    if entry.metadata().mode().is_dir() {
                        continue;
                    }
                    let path = entry.path().to_string();
                    keys.push(match strip.as_deref() {
                        Some(strip) => path.strip_prefix(strip).unwrap_or(&path).to_string(),
                        None => path,
                    });
                }
                Ok(keys)
            }
        })
        .await
        .with_context(|| format!("oss list page '{prefix}'"))
    }

    /// Write small data (catalog JSON, alias JSON, etc.).
    pub(crate) async fn put_bytes(
        &self,
        key: &str,
        data: impl Into<Bytes>,
        artifact: OssUploadArtifact,
    ) -> Result<()> {
        let data = data.into();
        let size = data.len() as u64;
        let oss_key = self.full_key(key);
        let mut metric =
            MetricGuard::operation_artifact(OSS_OPERATION_DURATION, "put_bytes", artifact.as_str());
        let result = self
            .run_operation(|operator| {
                let data = data.clone();
                let oss_key = oss_key.clone();
                async move { write_bytes_to_operator(&operator, &oss_key, data).await }
            })
            .await
            .with_context(|| format!("oss put '{key}'"));
        metric.finish(&result);
        if result.is_ok() {
            metrics::counter!(
                "agentenv_snapshot_oss_upload_bytes_total",
                "operation" => "put_bytes",
                "artifact" => artifact.as_str(),
            )
            .increment(size);
        }
        result?;
        Ok(())
    }

    /// Conditionally writes a small object. `etag = None` means the object
    /// must not already exist. A failed condition returns `Ok(false)`.
    pub(crate) async fn put_bytes_conditionally(
        &self,
        key: &str,
        data: impl Into<Bytes>,
        etag: Option<&str>,
    ) -> Result<bool> {
        let data = data.into();
        let oss_key = self.full_key(key);
        self.run_operation(|operator| {
            let data = data.clone();
            let oss_key = oss_key.clone();
            let etag = etag.map(str::to_owned);
            async move {
                let write = operator.write_with(&oss_key, data);
                let result = match etag.as_deref() {
                    Some(etag) => write.if_match(etag).await,
                    None => write.if_not_exists(true).await,
                };
                match result {
                    Ok(_) => Ok(true),
                    Err(error) if error.kind() == OpenDalErrorKind::ConditionNotMatch => Ok(false),
                    Err(error) => Err(error),
                }
            }
        })
        .await
        .with_context(|| format!("oss conditional put '{key}'"))
    }

    /// Upload a local file to OSS.
    pub(crate) async fn put_file(
        &self,
        key: &str,
        path: &Path,
        artifact: OssUploadArtifact,
    ) -> Result<()> {
        let oss_key = self.full_key(key);
        let path = path.to_path_buf();
        let mut metric =
            MetricGuard::operation_artifact(OSS_OPERATION_DURATION, "put_file", artifact.as_str());
        let start = Instant::now();
        let result: Result<u64> = async {
            let size = tokio::fs::metadata(&path)
                .await
                .with_context(|| format!("stat oss upload source file '{}'", path.display()))?
                .len();
            self.run_operation(|operator| {
                let oss_key = oss_key.clone();
                let path = path.clone();
                async move {
                    upload_file_streaming(
                        &operator,
                        &oss_key,
                        &path,
                        CHUNK_SIZE,
                        UPLOAD_CONCURRENCY,
                        None,
                    )
                    .await
                }
            })
            .await
            .with_context(|| format!("oss put file '{key}'"))?;
            Ok(size)
        }
        .await;
        metric.finish(&result);
        match result {
            Ok(size) => {
                metrics::counter!(
                    "agentenv_snapshot_oss_upload_bytes_total",
                    "operation" => "put_file",
                    "artifact" => artifact.as_str(),
                )
                .increment(size);
                info!(
                    key = %oss_key,
                    artifact = artifact.as_str(),
                    size_bytes = size,
                    elapsed_ms = start.elapsed().as_millis(),
                    "oss file uploaded"
                );
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    /// Delete a single object. Idempotent – missing objects are not errors.
    pub(crate) async fn delete(&self, key: &str) -> Result<()> {
        self.run_with_key(key, |operator, key| async move {
            match operator.delete(&key).await {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == OpenDalErrorKind::NotFound => Ok(()),
                Err(err) => Err(err),
            }
        })
        .await
        .with_context(|| format!("oss delete '{key}'"))
    }

    /// Delete all objects under a prefix.
    pub(crate) async fn delete_prefix(&self, prefix: &str) -> Result<()> {
        // Deletes only ever target the primary, so the listing that drives them
        // must be primary-only too: reading it from the mirror would name keys
        // that this delete cannot (and should not) touch.
        //
        // `list_keys_recursive_primary()` returns repository-relative keys with
        // the configured backend prefix stripped, while `delete()` expects that
        // same repository-relative form and re-applies the backend prefix.
        let keys = self.list_keys_recursive_primary(prefix).await?;
        stream::iter(keys.into_iter().map(Ok::<_, anyhow::Error>))
            .try_for_each_concurrent(16, |key| async move { self.delete(&key).await })
            .await
    }

    /// List all files recursively under a prefix from the primary only.
    async fn list_keys_recursive_primary(&self, prefix: &str) -> Result<Vec<String>> {
        let target = ReadTarget::new(prefix, &self.prefix);
        self.run_operation(|operator| {
            let target = target.clone();
            async move {
                let entries = operator
                    .list_with(&target.full_key())
                    .recursive(true)
                    .await?;
                let strip = target.strip_prefix();
                Ok(entries
                    .into_iter()
                    .filter(|entry| !entry.metadata().mode().is_dir())
                    .map(|entry| entry.path().to_string())
                    .map(|path| match strip.as_deref() {
                        Some(strip) => path.strip_prefix(strip).unwrap_or(&path).to_string(),
                        None => path,
                    })
                    .collect())
            }
        })
        .await
        .with_context(|| format!("oss list primary '{prefix}'"))
    }

    pub(crate) fn is_not_found_error(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| {
            if let Some(opendal_error) = cause.downcast_ref::<OpenDalError>() {
                return opendal_error.kind() == OpenDalErrorKind::NotFound;
            }
            if let Some(ObjectStoreOperatorError::OpenDal(opendal_error)) =
                cause.downcast_ref::<ObjectStoreOperatorError>()
            {
                return opendal_error.kind() == OpenDalErrorKind::NotFound;
            }
            false
        })
    }

    async fn run_with_key<T, F, Fut>(&self, key: &str, operation: F) -> Result<T>
    where
        F: Fn(Operator, String) -> Fut,
        Fut: Future<Output = opendal::Result<T>>,
    {
        let key = self.full_key(key);
        self.run_operation(|operator| {
            let key = key.clone();
            operation(operator, key)
        })
        .await
    }

    /// Primary-only operation. Writes and conditional reads use this so they can
    /// never touch the fallback.
    async fn run_operation<T, F, Fut>(&self, operation: F) -> Result<T>
    where
        F: Fn(Operator) -> Fut,
        Fut: Future<Output = opendal::Result<T>>,
    {
        Self::run_operation_on(
            &self.operator_config,
            &self.credentials,
            &self.cached_operator,
            operation,
        )
        .await
    }

    async fn run_operation_on<T, F, Fut>(
        operator_config: &ObjectStoreOperatorConfig,
        credentials: &Arc<CachedCredentialSource>,
        cached_operator: &CachedOperator,
        operation: F,
    ) -> Result<T>
    where
        F: Fn(Operator) -> Fut,
        Fut: Future<Output = opendal::Result<T>>,
    {
        // Centralizes one-shot operator construction plus credential-refresh
        // retry semantics so individual OSS operations don't each have to
        // reason about cached credentials and operator replacement.
        let current =
            Self::ensure_fresh_operator(operator_config, credentials, cached_operator).await?;
        let (value, refreshed) = run_with_refresh(
            &current,
            Some(credentials.as_ref()),
            operator_config,
            operation,
        )
        .await
        .map_err(anyhow::Error::from)?;
        if let Some(refreshed) = refreshed {
            *cached_operator.write().await = Some((operator_config.timeout, refreshed));
        }
        Ok(value)
    }

    async fn ensure_fresh_operator(
        operator_config: &ObjectStoreOperatorConfig,
        credentials: &Arc<CachedCredentialSource>,
        cached_operator: &CachedOperator,
    ) -> Result<OperatorWithCredential> {
        let credential = credentials.current().await?.ok_or_else(|| {
            anyhow::anyhow!("snapshot OSS client requires non-anonymous credentials")
        })?;

        {
            let cached = cached_operator.read().await;
            if let Some((timeout, state)) = cached.as_ref() {
                if *timeout == operator_config.timeout && state.credential() == Some(&credential) {
                    return Ok(state.clone());
                }
            }
        }

        let entry = OperatorWithCredential::new(
            build_object_store_operator(operator_config, Some(&credential))?,
            Some(credential),
        );
        *cached_operator.write().await = Some((operator_config.timeout, entry.clone()));
        Ok(entry)
    }

    /// Run a read operation, failing over to the mirror when the primary is
    /// unavailable (or, for immutable content-addressed keys, not found).
    ///
    /// The breaker short-circuits the primary attempt for the cooldown window
    /// after an unavailability error, so a burst of reads does not each pay a
    /// connect timeout. Reads use a shorter primary timeout so failover is fast.
    async fn run_read<T, F, Fut>(&self, key: &str, class: KeyClass, operation: F) -> Result<T>
    where
        F: Fn(Operator, ReadTarget) -> Fut + Clone,
        Fut: Future<Output = opendal::Result<T>>,
    {
        let Some(fallback) = self.fallback.as_ref() else {
            let target = ReadTarget::new(key, &self.prefix);
            return self
                .run_operation(|operator| operation(operator, target.clone()))
                .await;
        };
        let primary_target = ReadTarget::new(key, &self.prefix);
        let fallback_target = ReadTarget::new(key, &fallback.prefix);

        if !fallback.is_tripped() {
            let mut primary_config = self.operator_config.clone();
            primary_config.timeout = Some(fallback.primary_timeout);
            let probe = Self::run_operation_on(
                &primary_config,
                &self.credentials,
                &self.cached_operator,
                |operator| operation(operator, primary_target.clone()),
            )
            .await;
            match probe {
                Ok(value) => {
                    if fallback.has_tripped() {
                        fallback.clear();
                        info!(
                            endpoint = %self.operator_config.endpoint,
                            "oss primary recovered; routing reads back to the primary"
                        );
                    }
                    return Ok(value);
                }
                Err(error) => match classify_read_failure(class, &error) {
                    ReadFailure::TripFallback => {
                        fallback.trip();
                        warn!(
                            endpoint = %self.operator_config.endpoint,
                            error = %error,
                            "oss primary unavailable; routing reads to the fallback mirror"
                        );
                    }
                    ReadFailure::NotFoundFallback => {
                        warn!(
                            key = %key,
                            endpoint = %self.operator_config.endpoint,
                            "oss primary is missing an immutable object; trying the fallback mirror"
                        );
                    }
                    ReadFailure::Propagate => return Err(error),
                },
            }
        }

        let result = Self::run_operation_on(
            &fallback.operator_config,
            &fallback.credentials,
            &fallback.cached_operator,
            |operator| operation(operator, fallback_target.clone()),
        )
        .await;
        if result.is_ok() {
            metrics::counter!(
                "agentenv_snapshot_oss_fallback_reads_total",
                "key_class" => match class {
                    KeyClass::Immutable => "immutable",
                    KeyClass::Mutable => "mutable",
                },
            )
            .increment(1);
        }
        result.with_context(|| {
            format!(
                "read '{key}' from fallback endpoint {} failed",
                fallback.operator_config.endpoint
            )
        })
    }
}

/// Object location for one read attempt against a specific endpoint.
///
/// Carries the serving endpoint's prefix so a listing or download targets the
/// right keys and strips the right prefix when the mirror lays keys out
/// differently from the primary.
#[derive(Clone, Debug)]
struct ReadTarget {
    key: String,
    prefix: String,
}

impl ReadTarget {
    fn new(key: &str, prefix: &str) -> Self {
        Self {
            key: key.to_string(),
            prefix: prefix.to_string(),
        }
    }

    fn full_key(&self) -> String {
        self.full_key_of(&self.key)
    }

    fn full_key_of(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{}", self.prefix, key)
        }
    }

    fn strip_prefix(&self) -> Option<String> {
        (!self.prefix.is_empty()).then(|| format!("{}/", self.prefix))
    }
}

fn build_operator_config(
    bucket: &str,
    endpoint: &str,
    region: &str,
    addressing_override: Option<AddressingStyle>,
) -> Result<ObjectStoreOperatorConfig> {
    // Detection also validates the endpoint URL, so it always runs; an
    // explicit config override then wins over the detected style.
    let detected_style = detect_addressing_style(endpoint, bucket)?;
    Ok(ObjectStoreOperatorConfig {
        addressing_style: addressing_override.unwrap_or(detected_style),
        bucket: bucket.to_string(),
        endpoint: endpoint.to_string(),
        region: region.to_string(),
        // `None` keeps the operator's default read/write timeout; read failover
        // overrides it per request when a fallback is configured.
        timeout: None,
        max_retries: None,
    })
}

fn detect_addressing_style(endpoint: &str, bucket: &str) -> Result<AddressingStyle> {
    let url = Url::parse(endpoint).context("parse snapshot OSS endpoint for addressing style")?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("snapshot OSS endpoint host is missing"))?;
    let bucket_host = format!("{bucket}.");
    let is_bucket_virtual_host = host.starts_with(&bucket_host);
    let is_aliyun_endpoint = host.ends_with(".aliyuncs.com") || host.ends_with(".aliyun-inc.com");

    if is_bucket_virtual_host {
        return Ok(AddressingStyle::Virtual);
    }
    if is_aliyun_endpoint {
        return Ok(AddressingStyle::Virtual);
    }
    Ok(AddressingStyle::Path)
}

async fn download_object_to_file(
    operator: &Operator,
    key: &str,
    dest: &Path,
) -> opendal::Result<u64> {
    // Keep the tempfile handle alive until the final rename so any early
    // return still benefits from `NamedTempFile`'s automatic cleanup.
    let tmp = tempfile::NamedTempFile::new_in(dest.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|err| io_error_to_opendal(err, "create temporary download file"))?;
    let tmp_path = tmp.path().to_path_buf();
    let std_file = tmp
        .reopen()
        .map_err(|err| io_error_to_opendal(err, "reopen temporary download file"))?;
    let mut file = tokio::fs::File::from_std(std_file);
    let mut size = 0_u64;
    let mut stream = operator.reader(key).await?.into_stream(..).await?;
    while let Some(buffer) = stream.try_next().await? {
        for chunk in buffer {
            size += chunk.len() as u64;
            file.write_all(chunk.as_ref())
                .await
                .map_err(|err| io_error_to_opendal(err, "write downloaded object chunk"))?;
        }
    }

    file.flush()
        .await
        .map_err(|err| io_error_to_opendal(err, "flush downloaded object file"))?;
    file.sync_all()
        .await
        .map_err(|err| io_error_to_opendal(err, "sync downloaded object file"))?;
    drop(file);
    tokio::fs::rename(&tmp_path, dest)
        .await
        .map_err(|err| io_error_to_opendal(err, "rename downloaded object into place"))?;

    Ok(size)
}

async fn write_bytes_to_operator(
    operator: &Operator,
    key: &str,
    data: Bytes,
) -> opendal::Result<()> {
    operator.write(key, data).await.map(|_| ())
}

fn io_error_to_opendal(error: std::io::Error, message: &'static str) -> OpenDalError {
    OpenDalError::new(OpenDalErrorKind::Unexpected, message).set_source(error)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::cfg::{OssBackendConfig, OssFallbackConfig, SnapshotImageStoragePolicy};
    use crate::snapshot::repository::backends::oss::config::NormalizedOssConfig;

    fn client_with_fallback(
        primary_endpoint: &str,
        fallback_endpoint: &str,
        cooldown_secs: u64,
    ) -> OssClient {
        let config = OssBackendConfig {
            endpoint: primary_endpoint.to_string(),
            bucket: "primary-bucket".to_string(),
            prefix: Some("snapshots".to_string()),
            credential_process: None,
            access_key_id: Some("ak".to_string()),
            access_key_secret: Some("sk".to_string()),
            security_token: None,
            region: Some("us-east-1".to_string()),
            addressing_style: None,
            cache_max_size_gb: None,
            fallback: Some(OssFallbackConfig {
                endpoint: fallback_endpoint.to_string(),
                bucket: "mirror-bucket".to_string(),
                prefix: Some("replica".to_string()),
                credential_process: None,
                access_key_id: None,
                access_key_secret: None,
                security_token: None,
                region: Some("us-east-1".to_string()),
                addressing_style: None,
                cooldown_secs: Some(cooldown_secs),
                primary_timeout_secs: None,
            }),
        };
        let normalized =
            NormalizedOssConfig::new(&config, SnapshotImageStoragePolicy::ObjectStorage)
                .expect("normalize config");
        OssClient::new_with_fallback(
            normalized.bucket().to_string(),
            normalized.endpoint().to_string(),
            normalized.region().to_string(),
            normalized.prefix().to_string(),
            normalized.credential_source(),
            normalized.addressing_style(),
            normalized.fallback(),
        )
        .expect("build client with fallback")
    }

    fn unavailable() -> anyhow::Error {
        ObjectStoreOperatorError::OpenDal(OpenDalError::new(
            OpenDalErrorKind::Unexpected,
            "connect refused",
        ))
        .into()
    }

    fn not_found() -> anyhow::Error {
        ObjectStoreOperatorError::OpenDal(OpenDalError::new(OpenDalErrorKind::NotFound, "missing"))
            .into()
    }

    fn permission_denied() -> anyhow::Error {
        ObjectStoreOperatorError::OpenDal(OpenDalError::new(
            OpenDalErrorKind::PermissionDenied,
            "403",
        ))
        .into()
    }

    #[test]
    fn explicit_override_takes_precedence_over_detection() {
        let detected = OssClient::new(
            "snapshots".to_string(),
            "https://t3.storage.dev".to_string(),
            "auto".to_string(),
            String::new(),
            CredentialSource::Anonymous,
            None,
        )
        .expect("build client with detected style");
        assert_eq!(
            detected.operator_config.addressing_style,
            AddressingStyle::Path
        );

        let overridden = OssClient::new(
            "snapshots".to_string(),
            "https://t3.storage.dev".to_string(),
            "auto".to_string(),
            String::new(),
            CredentialSource::Anonymous,
            Some(AddressingStyle::Virtual),
        )
        .expect("build client with override");
        assert_eq!(
            overridden.operator_config.addressing_style,
            AddressingStyle::Virtual
        );
    }

    #[test]
    fn explicit_override_still_validates_endpoint() {
        OssClient::new(
            "snapshots".to_string(),
            "not a valid endpoint".to_string(),
            "auto".to_string(),
            String::new(),
            CredentialSource::Anonymous,
            Some(AddressingStyle::Virtual),
        )
        .expect_err("malformed endpoint must fail even with an explicit override");
    }

    #[test]
    fn key_classification_is_immutable_only_for_managed_layers() {
        assert_eq!(
            classify_key("managed-layers/sha256:abc"),
            KeyClass::Immutable
        );
        assert_eq!(
            classify_key("/managed-layers/sha256:abc"),
            KeyClass::Immutable
        );
        // Mutable records and heads must never be treated as content-addressed.
        assert_eq!(classify_key("catalog/aliases/app.json"), KeyClass::Mutable);
        assert_eq!(classify_key("catalog/records/1234.json"), KeyClass::Mutable);
        assert_eq!(
            classify_key("volumes/records/vol-1.json"),
            KeyClass::Mutable
        );
        assert_eq!(
            classify_key("volumes/aliases/cache.json"),
            KeyClass::Mutable
        );
        assert_eq!(
            classify_key("template-build/cache-head.json"),
            KeyClass::Mutable
        );
        assert_eq!(
            classify_key("artifacts/1234/vm_state.bin"),
            KeyClass::Mutable
        );
    }

    #[test]
    fn read_failure_classification_matches_error_kind_and_key_class() {
        // Transport-level failure always fails over.
        assert_eq!(
            classify_read_failure(KeyClass::Mutable, &unavailable()),
            ReadFailure::TripFallback
        );
        assert_eq!(
            classify_read_failure(KeyClass::Immutable, &unavailable()),
            ReadFailure::TripFallback
        );

        // Auth and precondition failures never fail over.
        assert_eq!(
            classify_read_failure(KeyClass::Immutable, &permission_denied()),
            ReadFailure::Propagate
        );

        // NotFound only fails over for immutable content-addressed objects.
        assert_eq!(
            classify_read_failure(KeyClass::Immutable, &not_found()),
            ReadFailure::NotFoundFallback
        );
        assert_eq!(
            classify_read_failure(KeyClass::Mutable, &not_found()),
            ReadFailure::Propagate
        );
    }

    #[test]
    fn fallback_operator_config_uses_mirror_endpoint_bucket_and_prefix() {
        let client = client_with_fallback("http://primary:9000", "http://mirror:9000", 30);
        let fallback = client.fallback.as_ref().expect("fallback configured");
        assert_eq!(fallback.operator_config.endpoint, "http://mirror:9000");
        assert_eq!(fallback.operator_config.bucket, "mirror-bucket");
        assert_eq!(fallback.prefix, "replica");
        assert_eq!(
            ReadTarget::new("managed-layers/x", &fallback.prefix).full_key(),
            "replica/managed-layers/x"
        );
        assert_eq!(fallback.primary_timeout, Duration::from_secs(5));
    }

    #[test]
    fn breaker_trips_and_expires_on_cooldown() {
        let client = client_with_fallback("http://primary:9000", "http://mirror:9000", 60);
        let fallback = client.fallback.clone().expect("fallback configured");

        assert!(!fallback.is_tripped());
        assert!(!fallback.has_tripped());
        fallback.trip();
        assert!(fallback.is_tripped());
        assert!(fallback.has_tripped());

        // Simulate cooldown expiry without sleeping.
        fallback
            .unavailable_until
            .store(now_unix_millis().saturating_sub(1), Ordering::Relaxed);
        assert!(!fallback.is_tripped());
        assert!(fallback.has_tripped());

        fallback.clear();
        assert!(!fallback.has_tripped());
    }

    #[test]
    fn read_target_applies_and_strips_endpoint_prefix() {
        let target = ReadTarget::new("catalog/records/1.json", "snapshots");
        assert_eq!(target.full_key(), "snapshots/catalog/records/1.json");
        assert_eq!(target.strip_prefix().as_deref(), Some("snapshots/"));

        let unprefixed = ReadTarget::new("managed-layers/x", "");
        assert_eq!(unprefixed.full_key(), "managed-layers/x");
        assert!(unprefixed.strip_prefix().is_none());
    }

    /// Two real S3 endpoints (MinIO) exercise the full read/write routing:
    /// primary served, primary stopped, primary empty, and primary restored.
    #[tokio::test]
    #[ignore = "requires docker"]
    async fn dual_endpoint_failover_end_to_end() {
        use agentenv_test_support::minio::{MinioFixture, MINIO_PASS, MINIO_USER};

        let primary_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
            listener.local_addr().expect("probe addr").port()
        };
        let primary = MinioFixture::start_on_port(primary_port)
            .await
            .expect("start primary minio");
        let mirror = MinioFixture::start().await.expect("start mirror minio");

        let build_client = |primary_endpoint: &str, mirror_endpoint: &str| {
            let config = OssBackendConfig {
                endpoint: primary_endpoint.to_string(),
                bucket: primary.bucket.clone(),
                prefix: Some("snapshots".to_string()),
                credential_process: None,
                access_key_id: Some(MINIO_USER.to_string()),
                access_key_secret: Some(MINIO_PASS.to_string()),
                security_token: None,
                region: Some(primary.region.clone()),
                addressing_style: None,
                cache_max_size_gb: None,
                fallback: Some(OssFallbackConfig {
                    endpoint: mirror_endpoint.to_string(),
                    bucket: mirror.bucket.clone(),
                    prefix: Some("snapshots".to_string()),
                    credential_process: None,
                    access_key_id: Some(MINIO_USER.to_string()),
                    access_key_secret: Some(MINIO_PASS.to_string()),
                    security_token: None,
                    region: Some(mirror.region.clone()),
                    addressing_style: None,
                    cooldown_secs: Some(1),
                    primary_timeout_secs: Some(2),
                }),
            };
            let normalized =
                NormalizedOssConfig::new(&config, SnapshotImageStoragePolicy::ObjectStorage)
                    .expect("normalize");
            Arc::new(
                OssClient::new_with_fallback(
                    normalized.bucket().to_string(),
                    normalized.endpoint().to_string(),
                    normalized.region().to_string(),
                    normalized.prefix().to_string(),
                    normalized.credential_source(),
                    normalized.addressing_style(),
                    normalized.fallback(),
                )
                .expect("client"),
            )
        };

        let mutable_key = "catalog/records/1.json";
        let immutable_key = "managed-layers/sha256:aaa";
        let full_mutable = "snapshots/catalog/records/1.json";
        let full_immutable = "snapshots/managed-layers/sha256:aaa";

        primary
            .put_object(full_mutable, b"primary".to_vec())
            .await
            .expect("seed primary record");
        primary
            .put_object(full_immutable, b"primary-layer".to_vec())
            .await
            .expect("seed primary layer");
        mirror
            .put_object(full_mutable, b"mirror".to_vec())
            .await
            .expect("seed mirror record");
        mirror
            .put_object(full_immutable, b"mirror-layer".to_vec())
            .await
            .expect("seed mirror layer");

        let client = build_client(&primary.endpoint, &mirror.endpoint);

        // 1. Primary up: reads are served from the primary.
        let bytes = client.get_bytes(mutable_key).await.expect("primary read");
        assert_eq!(bytes.as_ref(), b"primary");
        assert!(!client.fallback.as_ref().unwrap().has_tripped());

        // 2. Stop the primary: reads fail over within the breaker window.
        primary.stop().await.expect("stop primary");
        tokio::time::sleep(Duration::from_millis(300)).await;

        let started = Instant::now();
        let bytes = client
            .get_bytes(mutable_key)
            .await
            .expect("fallback read for mutable record");
        let first_failover = started.elapsed();
        assert_eq!(bytes.as_ref(), b"mirror");
        assert!(
            client.fallback.as_ref().unwrap().is_tripped(),
            "primary unavailability must trip the breaker"
        );

        // The breaker is tripped, so the next read skips the primary entirely
        // and stays fast.
        let started = Instant::now();
        let bytes = client
            .get_bytes(immutable_key)
            .await
            .expect("fallback read for immutable blob");
        let second_read = started.elapsed();
        assert_eq!(bytes.as_ref(), b"mirror-layer");
        assert!(
            second_read < Duration::from_secs(1),
            "breaker must skip the dead primary (took {second_read:?}, first {first_failover:?})"
        );

        // 3. Writes fail while the primary is down: they never reach the mirror.
        client
            .put_bytes(
                mutable_key,
                Bytes::from_static(b"write"),
                OssUploadArtifact::CatalogRecord,
            )
            .await
            .expect_err("write must fail with the primary down");
        assert_eq!(
            mirror
                .get_object(full_mutable)
                .await
                .expect("mirror record"),
            b"mirror".to_vec(),
            "writes must never be applied to the fallback"
        );

        // 4. Primary restored after the cooldown: reads return to the primary.
        primary.restart().await.expect("restart primary");
        let mut reseeded = false;
        for _ in 0..40 {
            if primary
                .put_object(full_mutable, b"primary-v2".to_vec())
                .await
                .is_ok()
            {
                reseeded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(reseeded, "primary did not become ready after restart");
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let bytes = client
            .get_bytes(mutable_key)
            .await
            .expect("read after primary restore");
        assert_eq!(bytes.as_ref(), b"primary-v2");
        assert!(!client.fallback.as_ref().unwrap().has_tripped());

        // 5. Primary up but empty: mutable record is NotFound (no fallback),
        //    immutable content-addressed blob falls back to the mirror.
        primary
            .delete_object(full_immutable)
            .await
            .expect("clear primary layer");
        let mutable_err = client
            .get_bytes("catalog/records/missing.json")
            .await
            .expect_err("mutable NotFound must not fall back");
        assert!(OssClient::is_not_found_error(&mutable_err));
        let bytes = client
            .get_bytes(immutable_key)
            .await
            .expect("immutable blob falls back on NotFound");
        assert_eq!(bytes.as_ref(), b"mirror-layer");
        // Immutable existence also consults the mirror.
        assert!(client
            .exists(immutable_key)
            .await
            .expect("immutable exists falls back"));
        assert!(!client
            .exists("catalog/records/missing.json")
            .await
            .expect("mutable absent stays absent"));
    }
}
