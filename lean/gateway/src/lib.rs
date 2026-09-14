// The README is the crate documentation, and its examples are the
// doctests. They connect to S3, so the README is the doc only when the
// `s3` feature is on; without it the crate says the one thing that
// matters and the doctests are not compiled against an API that is
// not there.
#![cfg_attr(feature = "s3", doc = include_str!("../README.md"))]
#![cfg_attr(
    not(feature = "s3"),
    doc = "The flint lean gateway as a library, built without the `s3` feature: the verbs on any `ObjectStore`. See the README for the full documentation."
)]

pub mod drafts;
#[cfg(feature = "http")]
pub mod http;
pub mod workspace;

pub use bytes::Bytes;

pub use crate::drafts::{user_ok, Draft, DraftMeta, DraftRow};
pub use flint_lean::inbox::{InboxDoc, InboxEntry, Refusal, Removal, VerbRequest, Window};
pub use flint_lean::manifest::{LeanEntry, LeanManifest};
pub use crate::workspace::{
    normalize_etag, path_ok, Accepted, Blob, Listed, PutFile, Snapshot, Status, VerbError, Workspace,
};
pub use flint_lean::{LeanConfig, LeanError, LEAN_DIR};
pub use flint_store::{crc64_nvme, crc64_to_b64, memory::MemoryStore, ObjectStore, StoreError};

#[cfg(feature = "s3")]
pub use flint_store::s3::S3Store;

/// Connect to a bucket with the ambient AWS environment (the SDK's
/// credential chain, `AWS_REGION`, `AWS_ENDPOINT_URL`). `endpoint`
/// overrides the endpoint and switches to path-style addressing — for
/// MinIO, Apache Ozone's S3 gateway, or any proxy. `None` is real S3.
///
/// One connection serves every workspace in the bucket: build one
/// [`Workspace`] per prefix on the returned store.
#[cfg(feature = "s3")]
pub async fn connect(
    bucket: &str,
    endpoint: Option<&str>,
) -> Result<std::sync::Arc<dyn ObjectStore>, StoreError> {
    let store = S3Store::connect(bucket.to_string(), endpoint.map(str::to_string)).await?;
    Ok(std::sync::Arc::new(store))
}

/// Connect with EXPLICIT credentials to an explicit endpoint, addressed
/// path-style — for test rigs and integrations against MinIO, Ozone's S3
/// gateway or localstack. Nothing is read from the environment, so an
/// AWS profile on the machine running the tests cannot stand in for the
/// key given here. See [`S3Store::with_credentials`].
#[cfg(feature = "s3")]
pub fn connect_with_credentials(
    bucket: &str,
    endpoint: &str,
    region: &str,
    access_key_id: &str,
    secret_access_key: &str,
) -> Result<std::sync::Arc<dyn ObjectStore>, StoreError> {
    let store = S3Store::with_credentials(bucket, endpoint, region, access_key_id, secret_access_key)?;
    Ok(std::sync::Arc::new(store))
}

