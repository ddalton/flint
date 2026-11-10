//! One way to build a store from configuration, whichever provider it
//! is: [`connect`] with a [`StoreSpec`].
//!
//! Every binary that used to call `S3Store::connect` builds through here,
//! so a provider is chosen in one place and an S3-only knob given to
//! another provider is refused by name rather than ignored.

use std::sync::Arc;

use crate::{ObjectStore, Provider, StoreError, StoreResult};

/// What a binary knows about its store before it connects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreSpec {
    pub provider: Provider,
    /// The bucket (S3, GCS) or container (Azure).
    pub bucket: String,
    /// An endpoint override: MinIO, Ozone or a proxy for S3 (path-style
    /// addressing), an emulator for the others. `None` is the provider's
    /// own service.
    pub endpoint: Option<String>,
    /// Parts of one object uploaded concurrently. 1 = sequential.
    pub part_parallelism: usize,
    /// The upload path's byte bound (`gate.rs`). `None` = no gate.
    pub upload_inflight_max_bytes: Option<u64>,
    /// S3 only: lower the single-request copy ceiling, so a drill reaches
    /// the multipart copy arm without a 5 GiB object. `None` = S3's 5 GiB.
    pub copy_whole_max: Option<u64>,
    /// S3 only: route GETs and HEADs through the raw read path
    /// (`rawread.rs`), which signs SigV4 by hand.
    pub raw_reads: bool,
    /// Where a backend that assembles a large object from temporary ones
    /// keeps them (GCS: `<root>.flint/lean/parts/`): lean's workspace
    /// prefix, with its `/`. S3 and Azure assemble inside the object and
    /// have no use for it. `None` is the bucket's root.
    pub scratch_root: Option<String>,
}

impl StoreSpec {
    pub fn new(provider: Provider, bucket: impl Into<String>) -> Self {
        StoreSpec {
            provider,
            bucket: bucket.into(),
            endpoint: None,
            part_parallelism: 1,
            upload_inflight_max_bytes: None,
            copy_whole_max: None,
            raw_reads: false,
            scratch_root: None,
        }
    }

    pub fn endpoint(mut self, endpoint: Option<String>) -> Self {
        self.endpoint = endpoint;
        self
    }

    /// Refuse what this provider cannot honour, before anything connects.
    pub fn check(&self) -> StoreResult<()> {
        if self.bucket.trim().is_empty() {
            return Err(StoreError::Other("store spec: the bucket is empty".into()));
        }
        if self.provider != Provider::S3 {
            let s3_only = [
                ("raw reads", self.raw_reads),
                ("a single-request copy ceiling", self.copy_whole_max.is_some()),
            ];
            if let Some((knob, _)) = s3_only.iter().find(|(_, set)| *set) {
                return Err(StoreError::Other(format!(
                    "store spec: {knob} is an S3 setting, and this store is {}",
                    self.provider
                )));
            }
        }
        Ok(())
    }
}

/// Build the store `spec` names.
pub async fn connect(spec: &StoreSpec) -> StoreResult<Arc<dyn ObjectStore>> {
    spec.check()?;
    match spec.provider {
        Provider::S3 => connect_s3(spec).await,
        Provider::Azure => connect_azure(spec).await,
        Provider::Gcs => connect_gcs(spec).await,
    }
}

/// The account and its key or SAS come from the environment the Azure
/// tools use (`AzureStore::from_env`); the bucket is the container, and
/// the endpoint the blob endpoint (Azurite's, for a rig).
#[cfg(feature = "azure")]
async fn connect_azure(spec: &StoreSpec) -> StoreResult<Arc<dyn ObjectStore>> {
    let mut s = crate::azure::AzureStore::from_env(spec.bucket.clone(), spec.endpoint.clone())?;
    if let Some(n) = spec.upload_inflight_max_bytes {
        s = s.with_upload_inflight_max_bytes(n);
    }
    Ok(Arc::new(s))
}

#[cfg(not(feature = "azure"))]
async fn connect_azure(_: &StoreSpec) -> StoreResult<Arc<dyn ObjectStore>> {
    Err(not_built(Provider::Azure))
}

/// The credentials come from the environment (`GcsStore::from_env`): a
/// service account's key, or an emulator's host.
#[cfg(feature = "gcs")]
async fn connect_gcs(spec: &StoreSpec) -> StoreResult<Arc<dyn ObjectStore>> {
    let mut s = crate::gcs::GcsStore::from_env(spec.bucket.clone(), spec.endpoint.clone())?
        .with_part_parallelism(spec.part_parallelism);
    if let Some(n) = spec.upload_inflight_max_bytes {
        s = s.with_upload_inflight_max_bytes(n);
    }
    if let Some(root) = &spec.scratch_root {
        s = s.with_scratch_root(root.clone());
    }
    Ok(Arc::new(s))
}

#[cfg(not(feature = "gcs"))]
async fn connect_gcs(_: &StoreSpec) -> StoreResult<Arc<dyn ObjectStore>> {
    Err(not_built(Provider::Gcs))
}

// Every backend built: nothing calls it.
#[cfg_attr(all(feature = "s3", feature = "azure", feature = "gcs"), allow(dead_code))]
fn not_built(p: Provider) -> StoreError {
    StoreError::Other(format!("the {p} backend is not built into this binary"))
}

#[cfg(feature = "s3")]
async fn connect_s3(spec: &StoreSpec) -> StoreResult<Arc<dyn ObjectStore>> {
    let mut s = crate::s3::S3Store::connect(spec.bucket.clone(), spec.endpoint.clone())
        .await?
        .with_part_parallelism(spec.part_parallelism);
    if let Some(n) = spec.upload_inflight_max_bytes {
        s = s.with_upload_inflight_max_bytes(n);
    }
    if let Some(n) = spec.copy_whole_max {
        s = s.with_copy_whole_max(n);
    }
    Ok(Arc::new(s.with_raw_reads(spec.raw_reads)?))
}

#[cfg(not(feature = "s3"))]
async fn connect_s3(_: &StoreSpec) -> StoreResult<Arc<dyn ObjectStore>> {
    Err(not_built(Provider::S3))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(spec: &StoreSpec) -> String {
        match spec.check() {
            Err(StoreError::Other(m)) => m,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn s3_only_knobs_are_refused_by_name_for_other_providers() {
        for p in [Provider::Azure, Provider::Gcs] {
            let mut spec = StoreSpec::new(p, "b");
            assert!(spec.check().is_ok(), "{p}: the plain spec is fine");
            spec.raw_reads = true;
            assert!(refusal(&spec).contains("raw reads"), "{p}");
            spec.raw_reads = false;
            spec.copy_whole_max = Some(1 << 20);
            assert!(refusal(&spec).contains("copy ceiling"), "{p}");
        }
        // The control: S3 takes both.
        let mut spec = StoreSpec::new(Provider::S3, "b");
        spec.raw_reads = true;
        spec.copy_whole_max = Some(1 << 20);
        assert!(spec.check().is_ok());
    }

    #[test]
    fn an_empty_bucket_is_refused() {
        assert!(refusal(&StoreSpec::new(Provider::S3, " ")).contains("bucket"));
    }

    #[tokio::test]
    async fn a_backend_this_binary_lacks_says_so() {
        let lacking: Vec<Provider> = [(Provider::Azure, cfg!(feature = "azure")), (Provider::Gcs, cfg!(feature = "gcs"))]
            .into_iter()
            .filter(|(_, built)| !built)
            .map(|(p, _)| p)
            .collect();
        for p in lacking {
            match connect(&StoreSpec::new(p, "b")).await {
                Err(StoreError::Other(m)) => assert!(m.contains(&format!("the {p} backend is not built")), "{m}"),
                Err(e) => panic!("{p}: {e}"),
                Ok(_) => panic!("{p}: connected to a backend that does not exist"),
            }
        }
    }
}
