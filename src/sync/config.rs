//! Store selection + assembly (DEV-188, §2.3).
//!
//! `build_store` is the single point that turns configuration into the
//! `SyncStore` the push/pull flows talk to — wrapping the chosen backend in
//! client-side encryption. `StoreConfig` is the extension point: adding a
//! NAS/filesystem/WebDAV backend is a new variant + adapter, nothing above the
//! trait changes.

use super::crypto::DataKey;
use super::encrypting_store::EncryptingStore;
use super::s3_store::{S3Config, S3Store};
use super::store::SyncStore;
use crate::settings::SyncSettings;

/// Which backend to use. Only S3 is built now.
pub enum StoreConfig {
    S3(S3Config),
}

/// Map the user's [`SyncSettings`] onto an [`S3Config`], or an error naming the
/// first missing field.
pub fn s3_config_from(settings: &SyncSettings) -> anyhow::Result<S3Config> {
    Ok(S3Config {
        bucket_name: settings
            .bucket
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no bucket configured"))?,
        region: settings
            .region
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no region configured"))?,
        profile: settings
            .profile
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no AWS profile configured"))?,
        endpoint: settings.endpoint.clone(),
    })
}

/// Build the active store wrapped in client-side encryption.
pub fn build_store(config: StoreConfig, key: DataKey) -> anyhow::Result<Box<dyn SyncStore>> {
    match config {
        StoreConfig::S3(cfg) => Ok(Box::new(EncryptingStore::new(S3Store::new(&cfg)?, key))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_config_from_reports_missing_fields() {
        let mut settings = SyncSettings::default();
        assert!(s3_config_from(&settings).is_err());

        settings.bucket = Some("my-bucket".into());
        settings.region = Some("ap-southeast-2".into());
        settings.profile = Some("deverge-sandbox".into());

        let cfg = s3_config_from(&settings).unwrap();
        assert_eq!(cfg.bucket_name, "my-bucket");
        assert_eq!(cfg.region, "ap-southeast-2");
        assert_eq!(cfg.profile, "deverge-sandbox");
        assert_eq!(cfg.endpoint, None);
    }

    #[test]
    fn device_id_is_generated_once_and_stable() {
        let mut settings = SyncSettings::default();
        assert!(settings.device_id.is_none());
        let first = settings.ensure_device_id();
        let second = settings.ensure_device_id();
        assert_eq!(first, second, "device id must be stable");
        assert_eq!(settings.device_id.as_deref(), Some(first.as_str()));
    }

    #[test]
    fn is_configured_requires_bucket_region_profile() {
        let mut settings = SyncSettings::default();
        assert!(!settings.is_configured());
        settings.bucket = Some("b".into());
        settings.region = Some("r".into());
        assert!(!settings.is_configured());
        settings.profile = Some("p".into());
        assert!(settings.is_configured());
    }
}
