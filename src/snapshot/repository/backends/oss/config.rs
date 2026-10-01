use std::time::Duration;

use anyhow::{Context, Result};
use object_store_operator::{
    credential_source_from_fields, normalized_credential, AddressingStyle, CredentialFields,
    CredentialSource, CredentialSourceOptions,
};

use crate::cfg::{
    OssAddressingStyle, OssBackendConfig, OssFallbackConfig, SnapshotImageStoragePolicy,
};

/// Read routing for a primary unavailability before probing the primary again.
pub(crate) const DEFAULT_FALLBACK_COOLDOWN: Duration = Duration::from_secs(30);
/// Per-request timeout applied to primary reads when a fallback is configured.
pub(crate) const DEFAULT_FALLBACK_PRIMARY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub(crate) struct NormalizedOssConfig {
    bucket: String,
    endpoint: String,
    region: String,
    prefix: String,
    credential_source: CredentialSource,
    snapshot_image_storage: SnapshotImageStoragePolicy,
    addressing_style: Option<AddressingStyle>,
    fallback: Option<NormalizedOssFallback>,
}

/// Normalized read-only mirror endpoint.
#[derive(Debug, Clone)]
pub(crate) struct NormalizedOssFallback {
    bucket: String,
    endpoint: String,
    region: String,
    prefix: String,
    credential_source: CredentialSource,
    addressing_style: Option<AddressingStyle>,
    cooldown: Duration,
    primary_timeout: Duration,
}

impl NormalizedOssFallback {
    pub(crate) fn bucket(&self) -> &str {
        &self.bucket
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) fn region(&self) -> &str {
        &self.region
    }

    pub(crate) fn prefix(&self) -> &str {
        &self.prefix
    }

    pub(crate) fn credential_source(&self) -> CredentialSource {
        self.credential_source.clone()
    }

    pub(crate) fn addressing_style(&self) -> Option<AddressingStyle> {
        self.addressing_style
    }

    pub(crate) fn cooldown(&self) -> Duration {
        self.cooldown
    }

    pub(crate) fn primary_timeout(&self) -> Duration {
        self.primary_timeout
    }
}

impl NormalizedOssConfig {
    pub(crate) fn new(
        config: &OssBackendConfig,
        snapshot_image_storage: SnapshotImageStoragePolicy,
    ) -> Result<Self> {
        let bucket = normalized_credential(Some(config.bucket.as_str()))
            .context("backend.oss.bucket cannot be empty")?
            .to_string();
        let endpoint = normalized_credential(Some(config.endpoint.as_str()))
            .context("backend.oss.endpoint cannot be empty")?
            .to_string();
        // Region is only consumed later by overlaybd global-config generation.
        // The runtime backend keeps the validation here so incomplete OSS config
        // still fails fast during backend construction.
        let region = normalized_credential(config.region.as_deref())
            .context("backend.oss.region is required")?
            .to_string();
        let prefix = config
            .prefix
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .trim_matches('/')
            .to_string();
        let credential_source = credential_source_from_fields(
            CredentialFields {
                access_key_id: config.access_key_id.as_deref(),
                secret_access_key: config.access_key_secret.as_deref(),
                security_token: config.security_token.as_deref(),
                credential_process: config.credential_process.as_deref(),
            },
            CredentialSourceOptions {
                scope: "backend.oss",
                allow_anonymous: false,
                required_access_key_id_label: "backend.oss.access_key_id",
                required_secret_access_key_label: "backend.oss.access_key_secret",
            },
        )?;
        let addressing_style = config.addressing_style.map(|style| match style {
            OssAddressingStyle::Path => AddressingStyle::Path,
            OssAddressingStyle::Virtual => AddressingStyle::Virtual,
        });
        let fallback = config
            .fallback
            .as_ref()
            .map(|fallback| normalize_fallback(fallback, &credential_source))
            .transpose()?;
        Ok(Self {
            bucket,
            endpoint,
            region,
            prefix,
            credential_source,
            snapshot_image_storage,
            addressing_style,
            fallback,
        })
    }

    pub(crate) fn fallback(&self) -> Option<&NormalizedOssFallback> {
        self.fallback.as_ref()
    }

    pub(crate) fn bucket(&self) -> &str {
        &self.bucket
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) fn prefix(&self) -> &str {
        &self.prefix
    }

    pub(crate) fn region(&self) -> &str {
        &self.region
    }

    /// Explicit bucket addressing style override, if configured. `None` means
    /// the client should fall back to endpoint-based auto-detection.
    pub(crate) fn addressing_style(&self) -> Option<AddressingStyle> {
        self.addressing_style
    }

    pub(crate) fn credential_source(&self) -> CredentialSource {
        self.credential_source.clone()
    }

    pub(crate) fn snapshot_image_storage(&self) -> SnapshotImageStoragePolicy {
        self.snapshot_image_storage
    }

    pub(crate) fn managed_layers_repo_blob_url(&self) -> String {
        // overlaybd expects an S3-compatible repo blob URL here, including for
        // Alibaba OSS, so the scheme remains `s3://` rather than `oss://`.
        if self.prefix.is_empty() {
            format!("s3://{}/managed-layers", self.bucket)
        } else {
            format!("s3://{}/{}/managed-layers", self.bucket, self.prefix)
        }
    }
}

/// Normalize the fallback section, inheriting the primary credentials when the
/// fallback does not configure its own.
fn normalize_fallback(
    config: &OssFallbackConfig,
    primary_credential_source: &CredentialSource,
) -> Result<NormalizedOssFallback> {
    let has_own_credentials = config
        .access_key_id
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
        || config
            .access_key_secret
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
        || config
            .security_token
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
        || config
            .credential_process
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty());

    let bucket = normalized_credential(Some(config.bucket.as_str()))
        .context("backend.oss.fallback.bucket cannot be empty")?
        .to_string();
    let endpoint = normalized_credential(Some(config.endpoint.as_str()))
        .context("backend.oss.fallback.endpoint cannot be empty")?
        .to_string();
    let region = normalized_credential(config.region.as_deref())
        .context("backend.oss.fallback.region is required")?
        .to_string();
    let prefix = config
        .prefix
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .trim_matches('/')
        .to_string();
    let credential_source = if has_own_credentials {
        credential_source_from_fields(
            CredentialFields {
                access_key_id: config.access_key_id.as_deref(),
                secret_access_key: config.access_key_secret.as_deref(),
                security_token: config.security_token.as_deref(),
                credential_process: config.credential_process.as_deref(),
            },
            CredentialSourceOptions {
                scope: "backend.oss.fallback",
                allow_anonymous: false,
                required_access_key_id_label: "backend.oss.fallback.access_key_id",
                required_secret_access_key_label: "backend.oss.fallback.access_key_secret",
            },
        )?
    } else {
        primary_credential_source.clone()
    };
    let addressing_style = config.addressing_style.map(|style| match style {
        OssAddressingStyle::Path => AddressingStyle::Path,
        OssAddressingStyle::Virtual => AddressingStyle::Virtual,
    });

    Ok(NormalizedOssFallback {
        bucket,
        endpoint,
        region,
        prefix,
        credential_source,
        addressing_style,
        cooldown: config
            .cooldown_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_FALLBACK_COOLDOWN),
        primary_timeout: config
            .primary_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_FALLBACK_PRIMARY_TIMEOUT),
    })
}

#[cfg(test)]
mod tests {
    use super::NormalizedOssConfig;
    use crate::cfg::{OssBackendConfig, SnapshotImageStoragePolicy};

    fn sample_config() -> OssBackendConfig {
        OssBackendConfig {
            endpoint: " https://oss-cn-hangzhou.aliyuncs.com ".to_string(),
            bucket: " demo-bucket ".to_string(),
            prefix: Some(" /snapshots/managed/ ".to_string()),
            credential_process: None,
            access_key_id: Some(" ak ".to_string()),
            access_key_secret: Some(" sk ".to_string()),
            security_token: Some(" token ".to_string()),
            region: Some(" cn-hangzhou ".to_string()),
            addressing_style: None,
            cache_max_size_gb: Some(8),
            fallback: None,
        }
    }

    #[test]
    fn normalized_config_trims_and_derives_shared_values() {
        let normalized =
            NormalizedOssConfig::new(&sample_config(), SnapshotImageStoragePolicy::ObjectStorage)
                .expect("normalize config");

        assert_eq!(normalized.bucket(), "demo-bucket");
        assert_eq!(
            normalized.endpoint(),
            "https://oss-cn-hangzhou.aliyuncs.com"
        );
        assert_eq!(normalized.prefix(), "snapshots/managed");
        assert_eq!(
            normalized.managed_layers_repo_blob_url(),
            "s3://demo-bucket/snapshots/managed/managed-layers"
        );
        assert_eq!(
            normalized.snapshot_image_storage(),
            crate::cfg::SnapshotImageStoragePolicy::ObjectStorage
        );
    }

    #[test]
    fn normalized_config_accepts_snapshot_image_publish_policy() {
        let normalized =
            NormalizedOssConfig::new(&sample_config(), SnapshotImageStoragePolicy::SourceRegistry)
                .expect("normalize config");

        assert_eq!(
            normalized.snapshot_image_storage(),
            crate::cfg::SnapshotImageStoragePolicy::SourceRegistry
        );
    }

    #[test]
    fn normalized_config_rejects_mixed_credential_sources() {
        let mut config = sample_config();
        config.credential_process = Some("echo creds".to_string());

        let err = NormalizedOssConfig::new(&config, SnapshotImageStoragePolicy::ObjectStorage)
            .expect_err("mixed credentials must fail");
        assert!(err
            .to_string()
            .contains("credential_process cannot be combined"));
    }

    #[test]
    fn normalized_config_requires_region() {
        let mut config = sample_config();
        config.region = Some(" ".to_string());

        let err = NormalizedOssConfig::new(&config, SnapshotImageStoragePolicy::ObjectStorage)
            .expect_err("missing region must fail");
        assert!(err.to_string().contains("backend.oss.region is required"));
    }

    fn fallback_config() -> crate::cfg::OssFallbackConfig {
        crate::cfg::OssFallbackConfig {
            endpoint: " http://mirror:9000 ".to_string(),
            bucket: " mirror-bucket ".to_string(),
            prefix: Some(" /replica/ ".to_string()),
            credential_process: None,
            access_key_id: None,
            access_key_secret: None,
            security_token: None,
            region: Some(" us-east-1 ".to_string()),
            addressing_style: None,
            cooldown_secs: Some(5),
            primary_timeout_secs: Some(2),
        }
    }

    #[test]
    fn normalized_config_without_fallback_has_none() {
        let normalized =
            NormalizedOssConfig::new(&sample_config(), SnapshotImageStoragePolicy::ObjectStorage)
                .expect("normalize config");
        assert!(normalized.fallback().is_none());
    }

    #[test]
    fn normalized_config_normalizes_fallback_and_inherits_credentials() {
        let mut config = sample_config();
        config.fallback = Some(fallback_config());
        let normalized =
            NormalizedOssConfig::new(&config, SnapshotImageStoragePolicy::ObjectStorage)
                .expect("normalize config");

        let fallback = normalized.fallback().expect("fallback present");
        assert_eq!(fallback.endpoint(), "http://mirror:9000");
        assert_eq!(fallback.bucket(), "mirror-bucket");
        assert_eq!(fallback.prefix(), "replica");
        assert_eq!(fallback.region(), "us-east-1");
        assert_eq!(fallback.cooldown(), std::time::Duration::from_secs(5));
        assert_eq!(
            fallback.primary_timeout(),
            std::time::Duration::from_secs(2)
        );
        // No fallback credentials configured, so the primary's are inherited.
        assert_eq!(fallback.credential_source(), normalized.credential_source());
    }

    #[test]
    fn normalized_config_fallback_uses_own_credentials_and_default_timings() {
        let mut config = sample_config();
        let mut fallback = fallback_config();
        fallback.access_key_id = Some(" mirror-ak ".to_string());
        fallback.access_key_secret = Some(" mirror-sk ".to_string());
        fallback.cooldown_secs = None;
        fallback.primary_timeout_secs = None;
        config.fallback = Some(fallback);

        let normalized =
            NormalizedOssConfig::new(&config, SnapshotImageStoragePolicy::ObjectStorage)
                .expect("normalize config");
        let normalized_fallback = normalized.fallback().expect("fallback present");
        assert_eq!(
            normalized_fallback.cooldown(),
            super::DEFAULT_FALLBACK_COOLDOWN
        );
        assert_eq!(
            normalized_fallback.primary_timeout(),
            super::DEFAULT_FALLBACK_PRIMARY_TIMEOUT
        );
        assert_ne!(
            normalized_fallback.credential_source(),
            normalized.credential_source()
        );
    }

    #[test]
    fn normalized_config_rejects_fallback_without_endpoint() {
        let mut config = sample_config();
        let mut fallback = fallback_config();
        fallback.endpoint = "  ".to_string();
        config.fallback = Some(fallback);

        let err = NormalizedOssConfig::new(&config, SnapshotImageStoragePolicy::ObjectStorage)
            .expect_err("empty fallback endpoint must fail");
        assert!(err
            .to_string()
            .contains("backend.oss.fallback.endpoint cannot be empty"));
    }

    #[test]
    fn normalized_config_rejects_fallback_without_region() {
        let mut config = sample_config();
        let mut fallback = fallback_config();
        fallback.region = None;
        config.fallback = Some(fallback);

        let err = NormalizedOssConfig::new(&config, SnapshotImageStoragePolicy::ObjectStorage)
            .expect_err("missing fallback region must fail");
        assert!(err
            .to_string()
            .contains("backend.oss.fallback.region is required"));
    }

    #[test]
    fn normalized_config_rejects_mixed_fallback_credentials() {
        let mut config = sample_config();
        let mut fallback = fallback_config();
        fallback.access_key_id = Some("ak".to_string());
        fallback.access_key_secret = Some("sk".to_string());
        fallback.credential_process = Some("echo creds".to_string());
        config.fallback = Some(fallback);

        let err = NormalizedOssConfig::new(&config, SnapshotImageStoragePolicy::ObjectStorage)
            .expect_err("mixed fallback credentials must fail");
        assert!(err
            .to_string()
            .contains("credential_process cannot be combined"));
    }
}
