//! S3-compatible [`SyncStore`] implementation (DEV-187).
//!
//! Backed by `durch/rust-s3`. Talks to AWS S3 and any S3-compatible endpoint
//! (Cloudflare R2, MinIO, NAS gateways). Credentials are **not stored by
//! Allele** — they are resolved fresh from `~/.aws/credentials` via a named
//! profile (see `Plans/SESSION-SYNC-PROPOSAL.md` §2.3), which works uniformly
//! for static IAM keys and session-token-bearing SSO/STS temp creds.
//!
//! Payloads handed to this layer are already client-side encrypted (DEV-189);
//! it only moves opaque bytes.

use async_trait::async_trait;
use s3::creds::Credentials;
use s3::error::S3Error;
use s3::{Bucket, Region};

use super::store::SyncStore;

/// Config for an S3-compatible backend. All plain strings — no secrets. Auth is
/// resolved from `~/.aws/credentials` using `profile`.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// Bucket name (not ARN — rust-s3 addresses by name + region).
    pub bucket_name: String,
    /// AWS region (e.g. `ap-southeast-2`), or the region label for a custom
    /// endpoint (Cloudflare R2 uses `auto`). Auto-resolved at config time from
    /// the bucket's `x-amz-bucket-region` for AWS (DEV-188).
    pub region: String,
    /// Named profile in `~/.aws/credentials` to resolve credentials from.
    pub profile: String,
    /// Set only for non-AWS S3-compatible targets (R2 / MinIO / NAS). Presence
    /// switches the client to path-style addressing.
    pub endpoint: Option<String>,
}

/// An S3-compatible [`SyncStore`].
pub struct S3Store {
    bucket: Box<Bucket>,
}

impl S3Store {
    /// Build a store from config. Credentials are resolved here from the named
    /// profile; a missing/expired profile fails fast with an actionable message
    /// rather than surfacing at first sync.
    pub fn new(config: &S3Config) -> anyhow::Result<Self> {
        let credentials = Credentials::from_profile(Some(&config.profile)).map_err(|e| {
            anyhow::anyhow!(
                "could not load AWS credentials for profile '{}': {e} — \
                 refresh the profile (e.g. `aws sso login --profile {}`)",
                config.profile,
                config.profile
            )
        })?;

        let region = resolve_region(&config.region, config.endpoint.as_deref())?;

        let bucket = Bucket::new(&config.bucket_name, region, credentials)?;
        // Non-AWS endpoints (MinIO, some NAS gateways) require path-style
        // addressing; virtual-hosted style is AWS's default.
        let bucket = if config.endpoint.is_some() {
            bucket.with_path_style()
        } else {
            bucket
        };

        Ok(Self { bucket })
    }
}

/// Map a region string (+ optional custom endpoint) onto a rust-s3 [`Region`].
/// A custom endpoint yields `Region::Custom`; otherwise the string is parsed as
/// an AWS region.
fn resolve_region(region: &str, endpoint: Option<&str>) -> anyhow::Result<Region> {
    match endpoint {
        Some(endpoint) => Ok(Region::Custom {
            region: region.to_string(),
            endpoint: endpoint.to_string(),
        }),
        None => region
            .parse::<Region>()
            .map_err(|e| anyhow::anyhow!("invalid AWS region '{region}': {e}")),
    }
}

#[async_trait]
impl SyncStore for S3Store {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        // rust-s3's `fail-on-err` feature (enabled by default) already turns any
        // non-2xx response into an `Err`, so `?` is sufficient.
        self.bucket.put_object(key, &bytes).await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        // A missing object is `Ok(None)`, not an error. With `fail-on-err` a 404
        // arrives as `Err(HttpFailWithBody(404, _))`; without it, as `Ok` with a
        // 404 status. Handle both so correctness doesn't hinge on the feature.
        match self.bucket.get_object(key).await {
            Ok(resp) => {
                let code = resp.status_code();
                if code == 404 {
                    Ok(None)
                } else if (200..300).contains(&code) {
                    Ok(Some(resp.to_vec()))
                } else {
                    anyhow::bail!("S3 get {key} failed: HTTP {code}")
                }
            }
            Err(S3Error::HttpFailWithBody(404, _)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let pages = self.bucket.list(prefix.to_string(), None).await?;
        let mut keys: Vec<String> = pages
            .into_iter()
            .flat_map(|page| page.contents.into_iter().map(|obj| obj.key))
            .collect();
        keys.sort();
        Ok(keys)
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        // S3 returns 204 for a delete; deleting an absent key also succeeds, so
        // the operation is idempotent (fail-on-err covers real failures).
        self.bucket.delete_object(key).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_region_uses_custom_for_endpoint() {
        let region = resolve_region("auto", Some("https://acct.r2.cloudflarestorage.com")).unwrap();
        match region {
            Region::Custom { region, endpoint } => {
                assert_eq!(region, "auto");
                assert_eq!(endpoint, "https://acct.r2.cloudflarestorage.com");
            }
            other => panic!("expected Region::Custom, got {other:?}"),
        }
    }

    #[test]
    fn resolve_region_parses_aws_region() {
        let region = resolve_region("ap-southeast-2", None).unwrap();
        assert_eq!(region.to_string(), "ap-southeast-2");
    }

    /// Full put/get/list/delete round-trip against a real bucket — the DEV-187
    /// acceptance criterion. Ignored by default (needs live credentials); run
    /// with a materialized profile:
    ///
    /// ```sh
    /// ALLELE_S3_TEST_BUCKET=my-bucket \
    /// ALLELE_S3_TEST_PROFILE=deverge-sandbox \
    /// ALLELE_S3_TEST_REGION=ap-southeast-2 \
    ///   cargo test sync::s3_store::tests::live_roundtrip -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "requires a live S3 bucket + ALLELE_S3_TEST_{BUCKET,PROFILE[,REGION,ENDPOINT]}"]
    async fn live_roundtrip() {
        let config = S3Config {
            bucket_name: std::env::var("ALLELE_S3_TEST_BUCKET").expect("ALLELE_S3_TEST_BUCKET"),
            region: std::env::var("ALLELE_S3_TEST_REGION").unwrap_or_else(|_| "us-east-1".into()),
            profile: std::env::var("ALLELE_S3_TEST_PROFILE").expect("ALLELE_S3_TEST_PROFILE"),
            endpoint: std::env::var("ALLELE_S3_TEST_ENDPOINT").ok(),
        };
        let store = S3Store::new(&config).expect("build store");
        let key = "allele/__selftest__/probe.bin";

        assert_eq!(
            store.get(key).await.unwrap(),
            None,
            "probe should start absent"
        );
        store.put(key, b"hello-allele".to_vec()).await.unwrap();
        assert_eq!(
            store.get(key).await.unwrap(),
            Some(b"hello-allele".to_vec())
        );
        let listed = store.list("allele/__selftest__/").await.unwrap();
        assert!(listed.iter().any(|k| k == key), "listed keys: {listed:?}");
        store.delete(key).await.unwrap();
        assert_eq!(store.get(key).await.unwrap(), None, "probe should be gone");
    }
}
