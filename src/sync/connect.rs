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
//! bucket (the permission sync actually uses) in its configured region.

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

/// Validate access to a specific bucket in its configured region — the
/// authoritative connection check. Returns the region on success, or an error
/// describing exactly what failed.
///
/// Access is exercised with a scoped `ListObjects` (the `s3:ListBucket`
/// permission sync itself needs). The request must be signed *against the
/// bucket's real region*: rust-s3's `GetBucketLocation` always targets the
/// `us-east-1` endpoint and comes back unsigned there, so we can't auto-resolve
/// the region — the user supplies it (it's known, and encoded in most bucket
/// names). A wrong region surfaces as an actionable error below.
pub async fn validate_bucket(config: &S3Config) -> anyhow::Result<String> {
    let region = config.region.trim();
    if region.is_empty() {
        anyhow::bail!("enter the bucket's region (e.g. ap-southeast-2)");
    }

    bucket_in_region(config, region)?
        .list("allele/".to_string(), Some("/".to_string()))
        .await
        .map_err(|e| explain_access_error(&config.bucket_name, region, config.endpoint.is_some(), e))?;

    Ok(region.to_string())
}

/// Translate a raw `ListObjects` failure into an actionable message. The three
/// common real-world causes — an expired SSO session, a wrong region, and a
/// missing `s3:ListBucket` grant — otherwise surface as opaque S3 XML.
fn explain_access_error(
    bucket: &str,
    region: &str,
    custom_endpoint: bool,
    error: S3Error,
) -> anyhow::Error {
    let raw = error.to_string();
    if raw.contains("ExpiredToken") || raw.contains("has expired") {
        anyhow::anyhow!(
            "the AWS session has expired — refresh the profile \
             (e.g. `aws sso login` / yawsso) and test again"
        )
    } else if !custom_endpoint
        && (raw.contains("No AWSAccessKey was presented")
            || raw.contains("AuthorizationHeaderMalformed")
            || raw.contains("PermanentRedirect"))
    {
        anyhow::anyhow!(
            "could not reach '{bucket}' in region '{region}' — check the region matches the bucket"
        )
    } else if raw.contains("AccessDenied") {
        anyhow::anyhow!("access denied to '{bucket}' — the profile lacks s3:ListBucket on it")
    } else if raw.contains("NoSuchBucket") {
        anyhow::anyhow!("bucket '{bucket}' does not exist")
    } else {
        anyhow::anyhow!("cannot access bucket '{bucket}': {error}")
    }
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
