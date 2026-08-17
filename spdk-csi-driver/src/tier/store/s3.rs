//! The S3 ObjectStore backend (A6/A8/A9).
//!
//! Everything conditional: PutObject and CompleteMultipartUpload carry
//! If-Match / If-None-Match:* (GA 2024-11); UploadPartCopy carries
//! x-amz-copy-source-if-match on the base generation; every publish is
//! stamped x-amz-meta-flint-* and carries a FULL_OBJECT CRC-64/NVME
//! that S3 validates SERVER-SIDE (a mismatch fails the publish — the
//! multipart ETag is never trusted as content identity). Every failure
//! path of compose aborts its MPU (A9); the epoch is a CAS re-PUT
//! lease object whose Last-Modified is the takeover clock (A8).
//!
//! Endpoint override + path-style addressing are supported for MinIO /
//! localstack rigs; real-S3 verification is the step-4 acceptance gate.

use super::*;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    AbortIncompleteMultipartUpload, BucketLifecycleConfiguration, BucketVersioningStatus,
    ChecksumAlgorithm, ChecksumType, CompletedMultipartUpload, CompletedPart,
    ExpirationStatus, LifecycleRule, LifecycleRuleFilter,
};
use tracing::{info, warn};

/// S3's multipart granularity (the A11 part grid consumes these).
pub const S3_MIN_PART: u64 = 5 * 1024 * 1024;
pub const S3_MAX_PARTS: usize = 10_000;

pub struct S3Store {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl S3Store {
    /// Build from the ambient AWS environment (credentials chain,
    /// region, AWS_ENDPOINT_URL). `endpoint`+path-style are for MinIO
    /// rigs; None means real S3.
    pub async fn connect(bucket: String, endpoint: Option<String>) -> StoreResult<Self> {
        let base = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        let mut b = aws_sdk_s3::config::Builder::from(&base);
        if let Some(ep) = endpoint {
            // Custom endpoints (MinIO/localstack) need path-style —
            // virtual-hosted addressing would resolve the bucket as a
            // DNS label of the rig host.
            b = b.endpoint_url(ep).force_path_style(true);
        }
        let client = aws_sdk_s3::Client::from_conf(b.build());
        Ok(S3Store { client, bucket })
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    fn stamps_meta(stamps: &GenerationStamps) -> HashMap<String, String> {
        stamps.to_meta().into_iter().collect()
    }

    /// `bucket/key` for x-amz-copy-source, percent-encoded (the SDK
    /// does not encode this header's key for us).
    fn copy_source(&self, key: &str) -> String {
        let mut enc = String::with_capacity(key.len() + self.bucket.len() + 1);
        enc.push_str(&self.bucket);
        enc.push('/');
        for b in key.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                    enc.push(b as char)
                }
                _ => enc.push_str(&format!("%{:02X}", b)),
            }
        }
        enc
    }
}

/// Map an SDK service error onto the trait's error contract by HTTP
/// status + S3 error code.
fn map_err<E>(ctx: &str, err: aws_sdk_s3::error::SdkError<E>) -> StoreError
where
    E: ProvideErrorMetadata + std::fmt::Debug + 'static,
{
    if let aws_sdk_s3::error::SdkError::ServiceError(ref se) = err {
        let status = se.raw().status().as_u16();
        let code = se.err().code().unwrap_or("").to_string();
        let msg = format!("{}: {} {}", ctx, status, code);
        return match (status, code.as_str()) {
            (412, _) => StoreError::PreconditionFailed(msg),
            (409, "ConditionalRequestConflict") => StoreError::Conflict(msg),
            (_, "NoSuchUpload") => StoreError::NoSuchUpload(msg),
            (404, _) | (_, "NoSuchKey") => StoreError::NotFound(msg),
            (_, "BadDigest") | (_, "InvalidDigest") | (_, "XAmzContentChecksumMismatch") => {
                StoreError::ChecksumMismatch(msg)
            }
            _ => StoreError::Other(format!("{}: {}", msg, err)),
        };
    }
    StoreError::Other(format!("{}: {}", ctx, err))
}

fn dt_unix(dt: Option<&aws_sdk_s3::primitives::DateTime>) -> Option<u64> {
    dt.map(|d| d.secs().max(0) as u64)
}

/// Read one local range on the blocking pool (parts can be hundreds of
/// MiB; never on the executor).
async fn read_local(path: &std::path::Path, offset: u64, len: u64) -> StoreResult<Bytes> {
    let pb = path.to_path_buf();
    let label = pb.display().to_string();
    tokio::task::spawn_blocking(move || -> std::io::Result<Bytes> {
        use std::os::unix::fs::FileExt;
        let f = std::fs::File::open(&pb)?;
        let mut buf = vec![0u8; len as usize];
        f.read_exact_at(&mut buf, offset)?;
        Ok(Bytes::from(buf))
    })
    .await
    .map_err(|e| StoreError::Other(format!("local read join: {}", e)))?
    .map_err(|e| {
        StoreError::Other(format!("local read {} [{}, +{}): {}", label, offset, len, e))
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct EpochBody {
    holder_id: String,
    epoch: u64,
    renewed_unix: u64,
    /// Guarantees every put changes the object's ETag (the CAS token).
    /// S3's PUT ETag is the body's MD5, and `renewed_unix` alone is
    /// second-granular — the real-S3 drill caught two renews in one
    /// second reproducing the token. Liveness is still judged on S3's
    /// Last-Modified, never on this.
    salt: String,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[async_trait]
impl ObjectStore for S3Store {
    async fn put_whole(
        &self,
        key: &str,
        body: Bytes,
        condition: &PutCondition,
        stamps: &GenerationStamps,
        crc64: u64,
    ) -> StoreResult<ObjectMeta> {
        let size = body.len() as u64;
        let crc_b64 = crc64_to_b64(crc64);
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body))
            .set_metadata(Some(Self::stamps_meta(stamps)))
            .checksum_algorithm(ChecksumAlgorithm::Crc64Nvme)
            .checksum_crc64_nvme(crc_b64.clone());
        req = match condition {
            PutCondition::IfMatch(etag) => req.if_match(etag),
            PutCondition::IfNoneMatchAny => req.if_none_match("*"),
        };
        let resp = req.send().await.map_err(|e| map_err("put_whole", e))?;
        Ok(ObjectMeta {
            etag: resp.e_tag().unwrap_or_default().to_string(),
            size,
            crc64_b64: Some(resp.checksum_crc64_nvme().unwrap_or(crc_b64.as_str()).to_string()),
            meta: Self::stamps_meta(stamps),
            last_modified_unix: None,
            storage_class: None, // this tier publishes STANDARD
        })
    }

    async fn compose_generation(&self, spec: &ComposeSpec<'_>) -> StoreResult<ObjectMeta> {
        if spec.parts.is_empty() {
            return Err(StoreError::Other("compose: no parts".into()));
        }
        if spec.parts.len() > S3_MAX_PARTS {
            return Err(StoreError::Other(format!(
                "compose: {} parts exceeds S3's {}",
                spec.parts.len(),
                S3_MAX_PARTS
            )));
        }
        let create = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(spec.key)
            .set_metadata(Some(Self::stamps_meta(&spec.stamps)))
            .checksum_algorithm(ChecksumAlgorithm::Crc64Nvme)
            .checksum_type(ChecksumType::FullObject)
            .send()
            .await
            .map_err(|e| map_err("create_multipart_upload", e))?;
        let upload_id = create
            .upload_id()
            .ok_or_else(|| StoreError::Other("create MPU returned no upload id".into()))?
            .to_string();

        match self.compose_parts_and_complete(spec, &upload_id).await {
            Ok(meta) => Ok(meta),
            Err(e) => {
                // A9: no failure path leaves the assembly pending. The
                // abort itself racing the lifecycle rule (or a takeover
                // sweep) is fine — absent is success.
                if let Err(ab) = self.abort_upload(spec.key, &upload_id).await {
                    warn!(
                        "tier: MPU abort after failed compose of {} also failed ({}); \
                         the A9 lifecycle rule is the backstop",
                        spec.key, ab
                    );
                }
                Err(e)
            }
        }
    }

    async fn head(&self, key: &str) -> StoreResult<ObjectMeta> {
        let resp = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled)
            .send()
            .await
            .map_err(|e| map_err("head", e))?;
        Ok(ObjectMeta {
            etag: resp.e_tag().unwrap_or_default().to_string(),
            size: resp.content_length().unwrap_or(0).max(0) as u64,
            crc64_b64: resp.checksum_crc64_nvme().map(|s| s.to_string()),
            meta: resp.metadata().cloned().unwrap_or_default(),
            last_modified_unix: dt_unix(resp.last_modified()),
            storage_class: resp.storage_class().map(|c| c.as_str().to_string()),
        })
    }

    async fn get_whole(
        &self,
        key: &str,
        if_match: Option<&str>,
    ) -> StoreResult<(ObjectMeta, Bytes)> {
        let mut req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled);
        if let Some(etag) = if_match {
            req = req.if_match(etag);
        }
        let resp = req.send().await.map_err(|e| map_err("get_whole", e))?;
        let meta = ObjectMeta {
            etag: resp.e_tag().unwrap_or_default().to_string(),
            size: resp.content_length().unwrap_or(0).max(0) as u64,
            crc64_b64: resp.checksum_crc64_nvme().map(|s| s.to_string()),
            meta: resp.metadata().cloned().unwrap_or_default(),
            last_modified_unix: dt_unix(resp.last_modified()),
            storage_class: resp.storage_class().map(|c| c.as_str().to_string()),
        };
        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|e| StoreError::Other(format!("get_whole body: {}", e)))?
            .into_bytes();
        Ok((meta, bytes))
    }

    async fn get_range(
        &self,
        key: &str,
        offset: u64,
        len: u64,
        if_match: &str,
    ) -> StoreResult<Bytes> {
        // RFC 9110 ranges are INCLUSIVE on both ends.
        let range = format!("bytes={}-{}", offset, offset + len - 1);
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .range(range)
            .if_match(if_match)
            .send()
            .await
            .map_err(|e| map_err("get_range", e))?;
        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|e| StoreError::Other(format!("get_range body: {}", e)))?
            .into_bytes();
        Ok(bytes)
    }

    async fn list(&self, prefix: &str) -> StoreResult<Vec<ListedObject>> {
        let mut out = Vec::new();
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .into_paginator()
            .send();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|e| map_err("list", e))?;
            for o in page.contents() {
                out.push(ListedObject {
                    key: o.key().unwrap_or_default().to_string(),
                    size: o.size().unwrap_or(0).max(0) as u64,
                    etag: o.e_tag().unwrap_or_default().to_string(),
                    last_modified_unix: dt_unix(o.last_modified()),
                });
            }
        }
        Ok(out)
    }

    async fn delete(&self, key: &str) -> StoreResult<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| map_err("delete", e))?;
        Ok(())
    }

    async fn list_uploads(&self, prefix: &str) -> StoreResult<Vec<PendingUpload>> {
        let mut out = Vec::new();
        let mut key_marker: Option<String> = None;
        let mut id_marker: Option<String> = None;
        loop {
            let resp = self
                .client
                .list_multipart_uploads()
                .bucket(&self.bucket)
                .prefix(prefix)
                .set_key_marker(key_marker.take())
                .set_upload_id_marker(id_marker.take())
                .send()
                .await
                .map_err(|e| map_err("list_multipart_uploads", e))?;
            for u in resp.uploads() {
                out.push(PendingUpload {
                    key: u.key().unwrap_or_default().to_string(),
                    upload_id: u.upload_id().unwrap_or_default().to_string(),
                    initiated_unix: dt_unix(u.initiated()),
                });
            }
            if resp.is_truncated() == Some(true) {
                key_marker = resp.next_key_marker().map(|s| s.to_string());
                id_marker = resp.next_upload_id_marker().map(|s| s.to_string());
            } else {
                return Ok(out);
            }
        }
    }

    async fn abort_upload(&self, key: &str, upload_id: &str) -> StoreResult<()> {
        match self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            // Already reaped (lifecycle rule or a racing sweep): fine.
            Err(e) => match map_err("abort_upload", e) {
                StoreError::NoSuchUpload(_) | StoreError::NotFound(_) => Ok(()),
                other => Err(other),
            },
        }
    }

    async fn bootstrap(&self, prefix: &str) -> StoreResult<BootstrapReport> {
        let mut report = BootstrapReport::default();

        // Reachability first: everything else's failures are then real.
        if let Err(e) = self.client.head_bucket().bucket(&self.bucket).send().await {
            report.errors.push(format!("bucket {} unreachable: {}", self.bucket, e));
            return Ok(report);
        }

        // Versioning posture (A9: recommend ON + noncurrent expiry as
        // the recovery window; loud-degrade otherwise).
        match self.client.get_bucket_versioning().bucket(&self.bucket).send().await {
            Ok(v) => match v.status() {
                Some(BucketVersioningStatus::Enabled) => {
                    report.notes.push("versioning: Enabled".into())
                }
                other => report.warnings.push(format!(
                    "versioning is {:?}: a foreign overwrite has NO recovery window \
                     (A9 recommends Versioning ON + NoncurrentVersionExpiration)",
                    other
                )),
            },
            Err(e) => report
                .warnings
                .push(format!("cannot read versioning posture: {}", e)),
        }

        // Lifecycle: verify/create the AbortIncompleteMultipartUpload
        // rule, scoped to our prefix (1-7 days per A9; we use 3).
        self.bootstrap_lifecycle(prefix, &mut report).await;

        // Encryption note (A9: Bucket Keys under SSE-KMS, else every
        // UploadPartCopy source pays a kms:Decrypt).
        match self.client.get_bucket_encryption().bucket(&self.bucket).send().await {
            Ok(enc) => {
                let uses_kms_without_bucket_key = enc
                    .server_side_encryption_configuration()
                    .map(|c| {
                        c.rules().iter().any(|r| {
                            let kms = r
                                .apply_server_side_encryption_by_default()
                                .map(|d| {
                                    matches!(
                                        d.sse_algorithm(),
                                        aws_sdk_s3::types::ServerSideEncryption::AwsKms
                                    )
                                })
                                .unwrap_or(false);
                            kms && r.bucket_key_enabled() != Some(true)
                        })
                    })
                    .unwrap_or(false);
                if uses_kms_without_bucket_key {
                    report.warnings.push(
                        "SSE-KMS without Bucket Keys: every flush part pays a KMS call \
                         (A9 requires Bucket Keys under SSE-KMS)"
                            .into(),
                    );
                }
            }
            Err(_) => { /* no encryption config readable — nothing to check */ }
        }

        // IAM probe: the MPU hygiene surface must work BEFORE the
        // first crash needs it (create + list + abort under a probe
        // key inside our prefix).
        let probe_key = format!("{}flint-bootstrap-probe", prefix);
        match self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(&probe_key)
            .send()
            .await
        {
            Ok(created) => {
                let id = created.upload_id().unwrap_or_default().to_string();
                if let Err(e) = self.list_uploads(prefix).await {
                    report.errors.push(format!(
                        "s3:ListBucketMultipartUploads denied — the A9 startup sweep \
                         cannot run: {}",
                        e
                    ));
                }
                if let Err(e) = self.abort_upload(&probe_key, &id).await {
                    report.errors.push(format!(
                        "s3:AbortMultipartUpload denied — failed flushes would leak \
                         MPUs until the lifecycle rule: {}",
                        e
                    ));
                }
            }
            Err(e) => report
                .errors
                .push(format!("cannot create a probe MPU (s3:PutObject): {}", e)),
        }

        info!(
            "tier: bucket bootstrap for {}/{}: {} notes, {} warnings, {} errors",
            self.bucket,
            prefix,
            report.notes.len(),
            report.warnings.len(),
            report.errors.len()
        );
        Ok(report)
    }

    async fn epoch_read(&self, key: &str) -> StoreResult<Option<EpochState>> {
        match self.get_whole(key, None).await {
            Ok((meta, bytes)) => {
                let body: EpochBody = serde_json::from_slice(&bytes)
                    .map_err(|e| StoreError::Other(format!("epoch body: {}", e)))?;
                Ok(Some(EpochState {
                    holder_id: body.holder_id,
                    epoch: body.epoch,
                    token: meta.etag,
                    last_renew_unix: meta.last_modified_unix,
                }))
            }
            Err(StoreError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn epoch_acquire(
        &self,
        key: &str,
        holder_id: &str,
        supersede: Option<&EpochState>,
    ) -> StoreResult<EpochLease> {
        let epoch = supersede.map_or(1, |s| s.epoch + 1);
        self.epoch_put(
            key,
            holder_id,
            epoch,
            match supersede {
                None => PutCondition::IfNoneMatchAny,
                Some(s) => PutCondition::IfMatch(s.token.clone()),
            },
        )
        .await
    }

    async fn epoch_renew(&self, key: &str, lease: &EpochLease) -> StoreResult<EpochLease> {
        self.epoch_put(
            key,
            &lease.holder_id,
            lease.epoch,
            PutCondition::IfMatch(lease.token.clone()),
        )
        .await
    }

    async fn epoch_release(&self, key: &str, lease: &EpochLease) -> StoreResult<()> {
        // S3 DELETE has no If-Match; verify-then-delete is the best a
        // CAS-object epoch offers (the residual race is A8's
        // documented heartbeat window; Azure's native lease closes it).
        let state = self.epoch_read(key).await?;
        match state {
            Some(s) if s.token == lease.token => self.delete(key).await,
            _ => Err(StoreError::PreconditionFailed(
                "epoch_release: not the holder anymore".into(),
            )),
        }
    }

    fn min_part_size(&self) -> u64 {
        S3_MIN_PART
    }

    fn max_parts(&self) -> usize {
        S3_MAX_PARTS
    }
}

impl S3Store {
    async fn compose_parts_and_complete(
        &self,
        spec: &ComposeSpec<'_>,
        upload_id: &str,
    ) -> StoreResult<ObjectMeta> {
        let mut completed: Vec<CompletedPart> = Vec::with_capacity(spec.parts.len());
        let mut expect = 0u64;
        for (i, p) in spec.parts.iter().enumerate() {
            let part_number = (i + 1) as i32;
            let (off, len) = match p {
                PartSource::Local { offset, len } | PartSource::BaseCopy { offset, len } => {
                    (*offset, *len)
                }
            };
            if off != expect {
                return Err(StoreError::Other(format!(
                    "compose: part {} at offset {} but object is contiguous to {}",
                    i, off, expect
                )));
            }
            expect = off + len;
            match p {
                PartSource::Local { offset, len } => {
                    let bytes = read_local(spec.local_path, *offset, *len).await?;
                    let resp = self
                        .client
                        .upload_part()
                        .bucket(&self.bucket)
                        .key(spec.key)
                        .upload_id(upload_id)
                        .part_number(part_number)
                        .checksum_algorithm(ChecksumAlgorithm::Crc64Nvme)
                        .body(ByteStream::from(bytes))
                        .send()
                        .await
                        .map_err(|e| map_err("upload_part", e))?;
                    completed.push(
                        CompletedPart::builder()
                            .part_number(part_number)
                            .set_e_tag(resp.e_tag().map(|s| s.to_string()))
                            .set_checksum_crc64_nvme(
                                resp.checksum_crc64_nvme().map(|s| s.to_string()),
                            )
                            .build(),
                    );
                }
                PartSource::BaseCopy { offset, len } => {
                    let base_etag = spec.base_etag.as_deref().ok_or_else(|| {
                        StoreError::Other("compose: BaseCopy without base_etag".into())
                    })?;
                    let resp = self
                        .client
                        .upload_part_copy()
                        .bucket(&self.bucket)
                        .key(spec.key)
                        .upload_id(upload_id)
                        .part_number(part_number)
                        .copy_source(self.copy_source(spec.base_key.unwrap_or(spec.key)))
                        .copy_source_if_match(base_etag)
                        .copy_source_range(format!("bytes={}-{}", offset, offset + len - 1))
                        .send()
                        .await
                        .map_err(|e| map_err("upload_part_copy", e))?;
                    let cp = resp.copy_part_result();
                    completed.push(
                        CompletedPart::builder()
                            .part_number(part_number)
                            .set_e_tag(cp.and_then(|c| c.e_tag()).map(|s| s.to_string()))
                            .set_checksum_crc64_nvme(
                                cp.and_then(|c| c.checksum_crc64_nvme())
                                    .map(|s| s.to_string()),
                            )
                            .build(),
                    );
                }
            }
        }

        let crc_b64 = crc64_to_b64(spec.crc64);
        let mut req = self
            .client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(spec.key)
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder().set_parts(Some(completed)).build(),
            )
            // Server-side validation of the WHOLE object against local
            // truth — copied parts included. This is the publish-time
            // integrity gate; the ETag never plays this role.
            .checksum_type(ChecksumType::FullObject)
            .checksum_crc64_nvme(crc_b64.clone())
            .mpu_object_size(expect as i64);
        req = match &spec.condition {
            PutCondition::IfMatch(etag) => req.if_match(etag),
            PutCondition::IfNoneMatchAny => req.if_none_match("*"),
        };
        let resp = req
            .send()
            .await
            .map_err(|e| map_err("complete_multipart_upload", e))?;
        Ok(ObjectMeta {
            etag: resp.e_tag().unwrap_or_default().to_string(),
            size: expect,
            crc64_b64: Some(
                resp.checksum_crc64_nvme().unwrap_or(crc_b64.as_str()).to_string(),
            ),
            meta: Self::stamps_meta(&spec.stamps),
            last_modified_unix: None,
            storage_class: None, // this tier publishes STANDARD
        })
    }

    async fn epoch_put(
        &self,
        key: &str,
        holder_id: &str,
        epoch: u64,
        condition: PutCondition,
    ) -> StoreResult<EpochLease> {
        let body = Bytes::from(
            serde_json::to_vec(&EpochBody {
                holder_id: holder_id.to_string(),
                epoch,
                renewed_unix: now_unix(),
                salt: uuid::Uuid::new_v4().to_string(),
            })
            .unwrap(),
        );
        let crc = crc64_nvme(&body);
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body))
            .checksum_algorithm(ChecksumAlgorithm::Crc64Nvme)
            .checksum_crc64_nvme(crc64_to_b64(crc));
        req = match &condition {
            PutCondition::IfMatch(etag) => req.if_match(etag),
            PutCondition::IfNoneMatchAny => req.if_none_match("*"),
        };
        let resp = req.send().await.map_err(|e| map_err("epoch_put", e))?;
        Ok(EpochLease {
            holder_id: holder_id.to_string(),
            epoch,
            token: resp.e_tag().unwrap_or_default().to_string(),
        })
    }

    async fn bootstrap_lifecycle(&self, prefix: &str, report: &mut BootstrapReport) {
        const RULE_ID: &str = "flint-tier-abort-mpu";
        let existing = self
            .client
            .get_bucket_lifecycle_configuration()
            .bucket(&self.bucket)
            .send()
            .await;
        let mut rules: Vec<LifecycleRule> = match existing {
            Ok(cfg) => cfg.rules().to_vec(),
            // A bucket with no lifecycle config answers 404
            // NoSuchLifecycleConfiguration (the SDK's Display hides the
            // code — map by status, the real-S3 drill caught the
            // string-match version of this skipping rule creation).
            Err(e) => match map_err("get_lifecycle", e) {
                StoreError::NotFound(_) => Vec::new(),
                other => {
                    report.warnings.push(format!(
                        "cannot read lifecycle configuration ({}); the MPU abort rule \
                         is UNVERIFIED — a crashed flush's parts bill until aborted",
                        other
                    ));
                    return;
                }
            },
        };

        let covered = rules.iter().any(|r| {
            r.status() == &ExpirationStatus::Enabled
                && r.abort_incomplete_multipart_upload()
                    .and_then(|a| a.days_after_initiation())
                    .is_some_and(|d| (1..=7).contains(&d))
                && r.filter()
                    .and_then(|f| f.prefix())
                    .map(|p| prefix.starts_with(p) || p.is_empty())
                    .unwrap_or(true)
        });
        if covered {
            report.notes.push("lifecycle: MPU abort rule present".into());
            return;
        }

        rules.push(
            LifecycleRule::builder()
                .id(RULE_ID)
                .status(ExpirationStatus::Enabled)
                .filter(LifecycleRuleFilter::builder().prefix(prefix).build())
                .abort_incomplete_multipart_upload(
                    AbortIncompleteMultipartUpload::builder()
                        .days_after_initiation(3)
                        .build(),
                )
                .build()
                .expect("static lifecycle rule must build"),
        );
        match self
            .client
            .put_bucket_lifecycle_configuration()
            .bucket(&self.bucket)
            .lifecycle_configuration(
                BucketLifecycleConfiguration::builder()
                    .set_rules(Some(rules))
                    .build()
                    .expect("lifecycle configuration must build"),
            )
            .send()
            .await
        {
            Ok(_) => report
                .notes
                .push(format!("lifecycle: created {} (3 days, prefix {})", RULE_ID, prefix)),
            Err(e) => report.warnings.push(format!(
                "cannot create the MPU abort lifecycle rule ({}); crashed-flush parts \
                 bill until manually aborted",
                e
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::arbitrate::{arbitrate, IntentProbe, Verdict};

    /// REAL-S3 acceptance drill — the step-4 gate ("tested against
    /// real S3, including induced 412s in both self-torn and
    /// genuinely-foreign flavors"). Ignored by default; needs a bucket
    /// and ambient credentials:
    ///
    ///   FLINT_TIER_S3_VERIFY_BUCKET=<bucket> AWS_PROFILE=<profile> \
    ///     cargo test --release --lib tier::store::s3 -- --ignored --nocapture
    ///
    /// Everything happens under a fresh `flint-tier-verify/<uuid>/`
    /// prefix and is deleted at the end; the bootstrap's lifecycle
    /// rule (scoped to that prefix) is the only thing that outlives
    /// the run on a shared bucket. Optional:
    /// FLINT_TIER_S3_VERIFY_ENDPOINT for a MinIO rig.
    #[tokio::test]
    #[ignore = "touches a real S3 bucket (set FLINT_TIER_S3_VERIFY_BUCKET)"]
    async fn real_s3_acceptance() {
        let Ok(bucket) = std::env::var("FLINT_TIER_S3_VERIFY_BUCKET") else {
            println!("SKIP: FLINT_TIER_S3_VERIFY_BUCKET not set");
            return;
        };
        let endpoint = std::env::var("FLINT_TIER_S3_VERIFY_ENDPOINT").ok();
        let store = S3Store::connect(bucket.clone(), endpoint).await.unwrap();
        let prefix = format!("flint-tier-verify/{}/", uuid::Uuid::new_v4());
        println!("=== real-S3 drill on s3://{}/{} ===", bucket, prefix);

        // Raw client for the out-of-band roles (foreign writer,
        // crashed uploader).
        let raw = {
            let base = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;
            aws_sdk_s3::Client::new(&base)
        };

        // ── A9 bootstrap ─────────────────────────────────────────────
        let report = store.bootstrap(&prefix).await.unwrap();
        println!("bootstrap notes: {:?}", report.notes);
        println!("bootstrap warnings: {:?}", report.warnings);
        assert!(report.ok(), "bootstrap errors: {:?}", report.errors);

        // ── conditional PUT, both flavors, against real S3 ──────────
        let key = format!("{}file.bin", prefix);
        let gen1_body = Bytes::from(vec![0xA5u8; 12 * 1024 * 1024]);
        let gen1_crc = crc64_nvme(&gen1_body);
        let stamps1 = GenerationStamps { generation: 1, epoch: 1, flush_uuid: "uuid-g1".into() };
        let gen1 = store
            .put_whole(&key, gen1_body.clone(), &PutCondition::IfNoneMatchAny, &stamps1, gen1_crc)
            .await
            .unwrap();
        println!("gen1 published: etag={} crc={:?}", gen1.etag, gen1.crc64_b64);
        assert_eq!(gen1.crc64_b64.as_deref(), Some(crc64_to_b64(gen1_crc).as_str()),
            "S3's full-object CRC64NVME must equal local truth");

        let err = store
            .put_whole(&key, gen1_body.clone(), &PutCondition::IfNoneMatchAny, &stamps1, gen1_crc)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)),
            "If-None-Match:* on an existing key must 412, got {:?}", err);
        println!("412 create-race flavor: OK");

        let err = store
            .put_whole(&key, gen1_body.clone(), &PutCondition::IfMatch("\"stale\"".into()), &stamps1, gen1_crc)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)));
        println!("412 stale-If-Match flavor: OK");

        // Wrong claimed checksum must FAIL the publish server-side.
        let err = store
            .put_whole(&format!("{}bad-crc", prefix), Bytes::from_static(b"x"),
                &PutCondition::IfNoneMatchAny, &stamps1, 0xDEAD)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::ChecksumMismatch(_) | StoreError::Other(_)),
            "a wrong CRC must not publish, got {:?}", err);
        println!("server-side checksum rejection: OK ({:?})", err);

        // HEAD: stamps roundtrip + checksum surfaced.
        let h = store.head(&key).await.unwrap();
        assert_eq!(GenerationStamps::from_meta(&h.meta), Some(stamps1.clone()));
        assert_eq!(h.size, gen1_body.len() as u64);
        println!("HEAD stamps + size roundtrip: OK");

        // Guarded GET (the hydration posture's input).
        let err = store.get_whole(&key, Some("\"stale\"")).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)));
        let (_, got) = store.get_whole(&key, Some(&gen1.etag)).await.unwrap();
        assert_eq!(got, gen1_body);
        println!("guarded GET both flavors: OK");

        // ── compose: MPU + UploadPartCopy + conditional Complete ────
        const MB: u64 = 1024 * 1024;
        let dir = tempfile::TempDir::new().unwrap();
        let local = dir.path().join("local.bin");
        let mut gen2_content = vec![0xA5u8; (12 * MB) as usize];
        gen2_content[..(6 * MB) as usize].fill(0x5A); // first 6 MiB dirty
        std::fs::write(&local, &gen2_content).unwrap();
        let gen2_crc = crc64_nvme(&gen2_content);
        let stamps2 = GenerationStamps { generation: 2, epoch: 1, flush_uuid: "uuid-g2".into() };
        let spec = ComposeSpec {
            key: &key,
            local_path: &local,
            parts: vec![
                PartSource::Local { offset: 0, len: 6 * MB },
                PartSource::BaseCopy { offset: 6 * MB, len: 6 * MB },
            ],
            base_key: None,
            base_etag: Some(gen1.etag.clone()),
            condition: PutCondition::IfMatch(gen1.etag.clone()),
            stamps: stamps2.clone(),
            crc64: gen2_crc,
        };
        let gen2 = store.compose_generation(&spec).await.unwrap();
        println!("gen2 composed: etag={} crc={:?}", gen2.etag, gen2.crc64_b64);
        assert!(gen2.etag.contains('-'), "multipart etag should carry a part count: {}", gen2.etag);
        assert_eq!(gen2.crc64_b64.as_deref(), Some(crc64_to_b64(gen2_crc).as_str()),
            "FULL_OBJECT CRC across Local + BaseCopy parts must equal local truth");
        let (_, got2) = store.get_whole(&key, Some(&gen2.etag)).await.unwrap();
        assert_eq!(got2.as_ref(), gen2_content.as_slice(), "composed bytes must equal local truth");
        println!("compose content + checksum verification: OK");

        // ── the self-torn drill ─────────────────────────────────────
        // The first compose plays "the torn Complete that landed"; this
        // naive retry (same intent, same If-Match on gen1) must hit a
        // REAL 412 (at part-copy or Complete), abort its MPU, and
        // arbitration must ADOPT — never re-upload, never page anyone.
        let err = store.compose_generation(&spec).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)),
            "the retry must be fenced with 412, got {:?}", err);
        assert!(store.list_uploads(&prefix).await.unwrap().is_empty(),
            "the fenced compose must have aborted its MPU (A9)");
        let verdict = arbitrate(&store, &IntentProbe {
            key: &key, to_gen: 2, flush_uuid: "uuid-g2", base_etag: Some(&gen1.etag),
        }).await.unwrap();
        match &verdict {
            Verdict::AdoptOwn(m) => assert_eq!(m.etag, gen2.etag),
            other => panic!("self-torn must adopt, got {:?}", other),
        }
        println!("self-torn 412 → abort → AdoptOwn: OK");

        // ── the genuinely-foreign drill ─────────────────────────────
        // An outside writer (raw client, no stamps) replaces the
        // object; the guarded publish 412s; arbitration names it
        // Foreign.
        raw.put_object()
            .bucket(&bucket)
            .key(&key)
            .body(aws_sdk_s3::primitives::ByteStream::from_static(b"foreign bytes"))
            .send()
            .await
            .expect("raw foreign put");
        let err = store.compose_generation(&ComposeSpec {
            condition: PutCondition::IfMatch(gen2.etag.clone()),
            base_etag: Some(gen2.etag.clone()),
            stamps: GenerationStamps { generation: 3, epoch: 1, flush_uuid: "uuid-g3".into() },
            ..spec.clone()
        }).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)),
            "publish over a foreign overwrite must 412, got {:?}", err);
        let verdict = arbitrate(&store, &IntentProbe {
            key: &key, to_gen: 3, flush_uuid: "uuid-g3", base_etag: Some(&gen2.etag),
        }).await.unwrap();
        assert!(matches!(verdict, Verdict::Foreign(Some(_))),
            "foreign overwrite must be named foreign, got {:?}", verdict);
        println!("foreign 412 → Foreign verdict: OK");

        // ── crashed-uploader sweep drill ────────────────────────────
        let orphan_key = format!("{}orphan.bin", prefix);
        let created = raw.create_multipart_upload()
            .bucket(&bucket).key(&orphan_key).send().await.expect("raw MPU create");
        let orphan_id = created.upload_id().unwrap().to_string();
        let pending = store.list_uploads(&prefix).await.unwrap();
        assert!(pending.iter().any(|p| p.upload_id == orphan_id),
            "the sweep must see the crashed assembly");
        store.abort_upload(&orphan_key, &orphan_id).await.unwrap();
        assert!(store.list_uploads(&prefix).await.unwrap().is_empty());
        store.abort_upload(&orphan_key, &orphan_id).await.unwrap(); // idempotent
        println!("MPU sweep (list → abort → idempotent re-abort): OK");

        // ── epoch CAS drill ─────────────────────────────────────────
        let ekey = format!("{}.flint-epoch", prefix);
        let l1 = store.epoch_acquire(&ekey, "hub-a", None).await.unwrap();
        assert_eq!(l1.epoch, 1);
        let err = store.epoch_acquire(&ekey, "hub-b", None).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)),
            "blind second claim must 412, got {:?}", err);
        let l1b = store.epoch_renew(&ekey, &l1).await.unwrap();
        assert_ne!(l1b.token, l1.token, "renew must rotate the CAS token");
        let err = store.epoch_renew(&ekey, &l1).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)), "stale renew must 412");
        let observed = store.epoch_read(&ekey).await.unwrap().unwrap();
        assert_eq!(observed.holder_id, "hub-a");
        assert!(observed.last_renew_unix.is_some(), "S3's clock must stamp the lease");
        let l2 = store.epoch_acquire(&ekey, "hub-b", Some(&observed)).await.unwrap();
        assert_eq!(l2.epoch, 2);
        let err = store.epoch_renew(&ekey, &l1b).await.unwrap_err();
        assert!(matches!(err, StoreError::PreconditionFailed(_)), "deposed renew must 412");
        store.epoch_release(&ekey, &l2).await.unwrap();
        assert!(store.epoch_read(&ekey).await.unwrap().is_none());
        println!("epoch CAS lifecycle (claim/renew/depose/release): OK");

        // ── cleanup ─────────────────────────────────────────────────
        for o in store.list(&prefix).await.unwrap() {
            store.delete(&o.key).await.unwrap();
        }
        assert!(store.list(&prefix).await.unwrap().is_empty());
        println!("cleanup: prefix emptied");
        println!("=== real-S3 drill PASSED ===");
    }
}
