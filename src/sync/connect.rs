//! Connection test + bucket discovery for config-time validation (DEV-188, §2.3).
//!
//! Front-loads the calls sync would make anyway so a misconfiguration is caught
//! when the user sets it up, not on first sync — and pinpoints *what* is wrong
//! (profile / account / region / bucket / permission).
//!
//! Bucket discovery degrades gracefully: `s3:ListAllMyBuckets` is account-level
//! and a least-privilege policy omits it, so an AccessDenied on `list_buckets`
//! becomes "type the bucket name" rather than a dead end. The authoritative
//! check is [`validate_bucket`], which exercises `s3:ListBucket` on the chosen
//! bucket (the permission sync actually uses) and resolves the true region.

use s3::creds::Credentials;
use s3::error::S3Error;
use s3::{Bucket, Region};

use super::s3_store::S3Config;

/// Outcome of a bucket-discovery attempt.
#[derive(Debug, Clone)]
pub struct Discovery {
    /// Buckets visible to the profile. Empty when `list_denied` is true.
    pub buckets: Vec<String>,
    /// True when listing all buckets was denied (a least-privilege key without
    /// `s3:ListAllMyBuckets`) — the UI should fall back to a typed bucket name.
    pub list_denied: bool,
}

/// List the buckets visible to `profile`. An AccessDenied is reported as
/// `list_denied` (not an error) so the caller can offer a typed-name path.
pub async fn discover_buckets(profile: &str, region: &str) -> anyhow::Result<Discovery> {
    let creds = credentials(profile)?;
    let region = parse_region(region, None)?;
    match Bucket::list_buckets(region, creds).await {
        Ok(resp) => Ok(Discovery {
            buckets: resp.bucket_names().collect(),
            list_denied: false,
        }),
        Err(e) if is_access_denied(&e) => Ok(Discovery {
            buckets: Vec::new(),
            list_denied: true,
        }),
        Err(e) => Err(anyhow::anyhow!("could not list buckets: {e}")),
    }
}

/// Validate access to a specific bucket and resolve its true region — the
/// authoritative connection check. Returns the resolved region string on
/// success, or an error describing exactly what failed.
///
/// For AWS the region is resolved via `GetBucketLocation` (which works from the
/// `us-east-1` endpoint regardless of the bucket's real region), then access is
/// exercised with a scoped `ListObjects`. For a custom endpoint (R2/MinIO/NAS)
/// the configured region is trusted and only access is checked.
pub async fn validate_bucket(config: &S3Config) -> anyhow::Result<String> {
    let region = if config.endpoint.is_some() {
        config.region.clone()
    } else {
        let (region, _) = bucket_in_region(config, "us-east-1")?
            .location()
            .await
            .map_err(|e| {
                anyhow::anyhow!("cannot resolve region for '{}': {e}", config.bucket_name)
            })?;
        region.to_string()
    };

    // ListObjects on the bucket exercises `s3:ListBucket` — the permission sync
    // itself needs (unlike `ListBuckets`, which tests a broader one).
    bucket_in_region(config, &region)?
        .list("allele/".to_string(), Some("/".to_string()))
        .await
        .map_err(|e| anyhow::anyhow!("cannot access bucket '{}': {e}", config.bucket_name))?;

    Ok(region)
}

fn credentials(profile: &str) -> anyhow::Result<Credentials> {
    Credentials::from_profile(Some(profile)).map_err(|e| {
        anyhow::anyhow!(
            "could not load credentials for profile '{profile}': {e} — \
             refresh the profile (e.g. `aws sso login --profile {profile}`)"
        )
    })
}

fn parse_region(region: &str, endpoint: Option<&str>) -> anyhow::Result<Region> {
    match endpoint {
        Some(endpoint) => Ok(Region::Custom {
            region: region.to_string(),
            endpoint: endpoint.to_string(),
        }),
        None => region
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid region '{region}': {e}")),
    }
}

/// Build a bucket handle for `config`, overriding the region (used for the
/// `us-east-1` GetBucketLocation probe and then the resolved region).
fn bucket_in_region(config: &S3Config, region: &str) -> anyhow::Result<Box<Bucket>> {
    let creds = credentials(&config.profile)?;
    let region = parse_region(region, config.endpoint.as_deref())?;
    let bucket = Bucket::new(&config.bucket_name, region, creds)?;
    Ok(if config.endpoint.is_some() {
        bucket.with_path_style()
    } else {
        bucket
    })
}

fn is_access_denied(error: &S3Error) -> bool {
    matches!(error, S3Error::HttpFailWithBody(403, _))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_config() -> S3Config {
        S3Config {
            bucket_name: std::env::var("ALLELE_S3_TEST_BUCKET").expect("ALLELE_S3_TEST_BUCKET"),
            region: std::env::var("ALLELE_S3_TEST_REGION").unwrap_or_else(|_| "us-east-1".into()),
            profile: std::env::var("ALLELE_S3_TEST_PROFILE").expect("ALLELE_S3_TEST_PROFILE"),
            endpoint: std::env::var("ALLELE_S3_TEST_ENDPOINT").ok(),
        }
    }

    #[tokio::test]
    #[ignore = "requires a live bucket + ALLELE_S3_TEST_{BUCKET,PROFILE[,REGION,ENDPOINT]}"]
    async fn live_validate_bucket_resolves_region() {
        let region = validate_bucket(&live_config()).await.unwrap();
        assert!(!region.is_empty(), "should resolve a region");
        println!("resolved region: {region}");
    }

    #[tokio::test]
    #[ignore = "requires a live profile + ALLELE_S3_TEST_{PROFILE[,REGION]}"]
    async fn live_discover_buckets() {
        let profile = std::env::var("ALLELE_S3_TEST_PROFILE").expect("ALLELE_S3_TEST_PROFILE");
        let region = std::env::var("ALLELE_S3_TEST_REGION").unwrap_or_else(|_| "us-east-1".into());
        let discovery = discover_buckets(&profile, &region).await.unwrap();
        println!(
            "buckets={:?} list_denied={}",
            discovery.buckets, discovery.list_denied
        );
    }
}
