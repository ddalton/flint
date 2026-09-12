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
    MetadataDirective, NoncurrentVersionExpiration,
};
use tracing::{info, warn};

/// S3's multipart granularity (the A11 part grid consumes these).
pub const S3_MIN_PART: u64 = 5 * 1024 * 1024;
pub const S3_MAX_PARTS: usize = 10_000;

pub struct S3Store {
    client: aws_sdk_s3::Client,
    /// Used only by [`ObjectStore::presign_put`]; see `connect`.
    presign_client: aws_sdk_s3::Client,
    bucket: String,
    /// The single-request ceiling for [`ObjectStore::copy_object`].
    /// S3's `CopyObject` tops out at 5 GiB; above it the copy goes
    /// through the MPU + `UploadPartCopy` path. Settable so a test can
    /// drive the MPU arm without a 5 GiB fixture — a threshold that only
    /// the fixture can cross is a threshold nothing exercises.
    copy_whole_max: u64,
    /// Parts of ONE object uploaded concurrently by `compose_generation`.
    /// `1` is the historical behaviour: a strictly sequential loop.
    ///
    /// Honoured ONLY when the caller supplied `ComposeSpec::crc64`. When
    /// the store must accumulate the full-object checksum it reads parts
    /// in order and a CRC-64 accumulated out of order is a different
    /// number — so forge's one-pass push (`crc64: None`) keeps the
    /// sequential path untouched no matter what this says.
    ///
    /// Measured on runcr 2026-09-10 (i4i.large, us-west-1, n=3): lean's
    /// barrier of 6 x 1 GiB took 36.05-37.30 s against 19.43-19.46 s for
    /// a 32-way `aws s3 cp` of the same bytes on the same node, and of
    /// 4 GiB + 2k files 61.09-61.46 s against 18.99-20.13 s. The gap is
    /// this loop: `fanout` spreads work ACROSS objects, so a tree whose
    /// critical path is one large object gets no concurrency at all.
    part_parallelism: usize,
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
        // Bound TIME TO FIRST BYTE. The default provider sets only a
        // connect timeout (~3.1 s), and the SDK's stalled-stream
        // protection arms AFTER headers arrive — so nothing at all
        // bounded the wait for a response that never starts. One pooled
        // connection to a silently-dead peer (spot reclamation is the
        // routine event on this fleet) then hangs one slot of the
        // checkout fan-out, and because the fan-out is collected as a
        // whole, the ENTIRE checkout waits and the agent-start marker
        // never lands. The startupProbe burns its full derived budget,
        // restarts the container, and the resume row re-reads the tree.
        //
        // `read_timeout` is first-byte-only, so it is safe to set
        // globally. An `operation_attempt_timeout` would NOT be: it
        // would guillotine a legitimately long `upload_part`.
        // Retries are unaffected and safe here — every lean write is
        // conditional, so a retried publish cannot double-apply.
        b = b.timeout_config(
            aws_sdk_s3::config::timeout::TimeoutConfig::builder()
                .connect_timeout(std::time::Duration::from_secs(3))
                .read_timeout(std::time::Duration::from_secs(10))
                .build(),
        );
        let endpoint_for_presign = endpoint.clone();
        if let Some(ep) = endpoint {
            // Custom endpoints (MinIO/localstack) need path-style —
            // virtual-hosted addressing would resolve the bucket as a
            // DNS label of the rig host.
            b = b.endpoint_url(ep).force_path_style(true);
        }
        // A SECOND client, for presigning writes only.
        //
        // Since SDK 1.66 the default is `WhenSupported`, which adds an
        // `x-amz-checksum-crc32` header to PutObject — and a presigned
        // URL signs the headers it was built with. A git-lfs client
        // handed such a URL does not send that header, so the
        // signature does not match and S3 answers 403 with nothing in
        // it about checksums. `WhenRequired` leaves it off, and it is
        // scoped to this client rather than set globally because every
        // other write in flint passes its CRC-64 EXPLICITLY and must
        // keep doing so.
        let mut pb = aws_sdk_s3::config::Builder::from(&base);
        pb = pb.timeout_config(
            aws_sdk_s3::config::timeout::TimeoutConfig::builder()
                .connect_timeout(std::time::Duration::from_secs(3))
                .read_timeout(std::time::Duration::from_secs(10))
                .build(),
        );
        if let Some(ep) = endpoint_for_presign {
            pb = pb.endpoint_url(ep).force_path_style(true);
        }
        pb = pb.request_checksum_calculation(
            aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
        );
        let presign_client = aws_sdk_s3::Client::from_conf(pb.build());

        let client = aws_sdk_s3::Client::from_conf(b.build());
        Ok(S3Store {
            client,
            presign_client,
            bucket,
            part_parallelism: 1,
            // S3's documented CopyObject ceiling.
            copy_whole_max: 5 * 1024 * 1024 * 1024,
        })
    }

    /// Upload this many parts of one object concurrently (see the field).
    /// Clamped to at least 1; `1` restores the sequential loop.
    pub fn with_part_parallelism(mut self, n: usize) -> Self {
        self.part_parallelism = n.max(1);
        self
    }

    /// Lower the single-request copy ceiling (see the field). Tests use
    /// it to reach the MPU arm without a multi-gigabyte object.
    pub fn with_copy_whole_max(mut self, n: u64) -> Self {
        self.copy_whole_max = n;
        self
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
        return match classify(status, &code, msg.clone()) {
            Some(mapped) => mapped,
            None => StoreError::Other(format!("{}: {}", msg, err)),
        };
    }
    StoreError::Other(format!("{}: {}", ctx, err))
}

/// The status/code decision table, split out of `map_err` so it can be
/// tested without constructing an `SdkError`.
///
/// `None` means "no specific contract applies" and the caller wraps it
/// in `Other` with the full SDK debug text attached.
fn classify(status: u16, code: &str, msg: String) -> Option<StoreError> {
    Some(match (status, code) {
        (412, _) => StoreError::PreconditionFailed(msg),
        (409, "ConditionalRequestConflict") => StoreError::Conflict(msg),
        (_, "NoSuchUpload") => StoreError::NoSuchUpload(msg),
        // A missing BUCKET is not a missing key, and folding it into
        // NotFound was a data-integrity trap. `NotFound` means "this
        // object does not exist yet", which every caller reads as
        // FIRST WRITE: `manifest::load` answers `Ok(None)`, a lean
        // checkout writes its completion marker over an empty tree, and
        // the agent starts work against nothing — discovering only at
        // its first publish that there was never a bucket to write to.
        // A missing container means the configuration is wrong or the
        // bucket was deleted, and the only safe answer is to fail.
        // Measured on the kind rig: MinIO restarted, lost its bucket,
        // and every lean workspace checked out "successfully" as empty.
        (_, "NoSuchBucket") => StoreError::Other(format!("bucket does not exist: {msg}")),
        (404, _) | (_, "NoSuchKey") => StoreError::NotFound(msg),
        // 401/403 and their named codes. These used to fall through to
        // `Other`, where the epoch heartbeat counted them as ordinary
        // renew failures — so an expired session token or a rotated key
        // fenced the hub and exited, logging about lease windows.
        (401, _)
        | (403, _)
        | (_, "ExpiredToken")
        | (_, "ExpiredTokenException")
        | (_, "InvalidAccessKeyId")
        | (_, "SignatureDoesNotMatch")
        | (_, "AccessDenied")
        | (_, "RequestTimeTooSkewed")
        | (_, "InvalidToken") => StoreError::Auth(msg),
        (_, "BadDigest") | (_, "InvalidDigest") | (_, "XAmzContentChecksumMismatch") => {
            StoreError::ChecksumMismatch(msg)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    /// Every way S3 says "you may not do this" must land on `Auth`, and
    /// nothing else may.
    #[test]
    fn refusals_are_classified_apart_from_failures() {
        let m = || "ctx: x".to_string();
        for (status, code) in [
            (403u16, ""),
            (401, ""),
            (403, "AccessDenied"),
            (400, "ExpiredToken"),
            (400, "InvalidAccessKeyId"),
            (403, "SignatureDoesNotMatch"),
            (403, "RequestTimeTooSkewed"),
        ] {
            assert!(
                matches!(classify(status, code, m()), Some(StoreError::Auth(_))),
                "({status}, {code:?}) must classify as Auth",
            );
        }

        // The contracts that already existed must not have moved — an
        // arm ordered above them would silently capture them, and 412
        // in particular routes to arbitration rather than an operator.
        assert!(matches!(classify(412, "", m()), Some(StoreError::PreconditionFailed(_))));
        assert!(matches!(classify(404, "", m()), Some(StoreError::NotFound(_))));
        // A missing BUCKET must never read as a missing key: NotFound
        // means "first write" to every caller, and a lean checkout would
        // start an agent against an empty tree it can never publish.
        assert!(
            matches!(classify(404, "NoSuchBucket", m()), Some(StoreError::Other(_))),
            "NoSuchBucket classified as NotFound — a missing bucket would read as an empty workspace"
        );
        assert!(matches!(classify(409, "ConditionalRequestConflict", m()), Some(StoreError::Conflict(_))));
        assert!(matches!(classify(400, "BadDigest", m()), Some(StoreError::ChecksumMismatch(_))));

        // And a genuine transient must NOT be an auth refusal, or the
        // new message would send every operator hunting a credential
        // during an outage.
        assert!(classify(503, "SlowDown", m()).is_none(), "5xx is not a refusal");
        assert!(classify(500, "InternalError", m()).is_none());
    }
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
    /// Set by a clean shutdown: the holder finished its final flush,
    /// fenced itself, and will never publish under this epoch again.
    /// A successor may supersede it IMMEDIATELY instead of waiting out
    /// a lease it knows is dead. `serde(default)` keeps cells written
    /// by older hubs readable — they simply read as not-released.
    #[serde(default)]
    released: bool,
    /// The holder's observed-state echo (`LeaseEcho`), opaque here —
    /// the operator parses it, the store just carries it. `default`
    /// keeps cells written by older binaries readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    echo: Option<String>,
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
            PutCondition::Unconditional => req,
        };
        let resp = req.send().await.map_err(|e| map_err("put_whole", e))?;
        Ok(ObjectMeta {
            etag: resp.e_tag().unwrap_or_default().to_string(),
            size,
            crc64_b64: Some(resp.checksum_crc64_nvme().unwrap_or(crc_b64.as_str()).to_string()),
            meta: Self::stamps_meta(stamps),
            last_modified_unix: None,
            storage_class: None, // this tier publishes STANDARD
            // S3 returns `x-amz-version-id` on every PUT to a versioned
            // bucket; this used to be discarded, which is why gated
            // citation costs no extra request once it is kept. `None`
            // means unversioned — or a proxy stripping the header, which
            // the conformance probe REFUSES rather than degrading into.
            version_id: resp.version_id().map(|v| v.to_string()),
        })
    }

    async fn copy_object(
        &self,
        src_key: &str,
        src_if_match: Option<&str>,
        dst_key: &str,
        condition: &PutCondition,
        stamps: &GenerationStamps,
    ) -> StoreResult<ObjectMeta> {
        // HEAD the source for its SIZE and its CRC. The size picks the
        // transport; the CRC is the destination's, unchanged, because a
        // copy cannot alter bytes and a checksum describes bytes.
        let src = self.head(src_key).await?;
        if let Some(want) = src_if_match {
            if src.etag != want {
                return Err(StoreError::PreconditionFailed(format!(
                    "copy source {src_key} is at {}, not {want}",
                    src.etag
                )));
            }
        }
        // Belt AND braces, deliberately: the check above judges what the
        // caller read, and `copy_source_if_match` closes the window
        // between this HEAD and the copy. Same shape as the gateway's
        // HEAD-then-conditional-PUT.
        let guard = src_if_match.map(|s| s.to_string()).or_else(|| Some(src.etag.clone()));

        if src.size <= self.copy_whole_max {
            let mut req = self
                .client
                .copy_object()
                .bucket(&self.bucket)
                .key(dst_key)
                .copy_source(self.copy_source(src_key))
                // REPLACE, never COPY: the source's user metadata carries
                // ITS generation, epoch and flush_uuid, and a destination
                // wearing them files one object's publish history under
                // another object's key.
                .metadata_directive(MetadataDirective::Replace)
                .set_metadata(Some(Self::stamps_meta(stamps)))
                .checksum_algorithm(ChecksumAlgorithm::Crc64Nvme);
            if let Some(g) = &guard {
                req = req.copy_source_if_match(g);
            }
            req = match condition {
                PutCondition::IfMatch(etag) => req.if_match(etag),
                PutCondition::IfNoneMatchAny => req.if_none_match("*"),
                PutCondition::Unconditional => req,
            };
            let resp = req.send().await.map_err(|e| map_err("copy_object", e))?;
            let r = resp.copy_object_result();
            return Ok(ObjectMeta {
                etag: r.and_then(|c| c.e_tag()).unwrap_or_default().to_string(),
                size: src.size,
                // The source's, and it must be: identical bytes. When
                // the backend echoes one, prefer it — a disagreement is
                // the backend telling us the copy is not what we think.
                crc64_b64: r
                    .and_then(|c| c.checksum_crc64_nvme())
                    .map(|s| s.to_string())
                    .or(src.crc64_b64),
                meta: Self::stamps_meta(stamps),
                last_modified_unix: None,
                storage_class: None,
                version_id: resp.version_id().map(|v| v.to_string()),
            });
        }

        // Over the single-request ceiling: the same MPU the flusher
        // uses, every part a server-side range copy. This is the FIRST
        // caller of `ComposeSpec::base_key` — the field has existed for
        // the A7 re-key flush and nothing has ever set it.
        let part = self.min_part_size().max(1);
        let mut parts = vec![];
        let mut off = 0u64;
        while off < src.size {
            let len = part.min(src.size - off);
            parts.push(PartSource::BaseCopy { offset: off, len });
            off += len;
        }
        let crc = src
            .crc64_b64
            .as_deref()
            .and_then(crc64_from_b64)
            .ok_or_else(|| {
                StoreError::Other(format!(
                    "copy {src_key}: the source carries no CRC-64, and a copied part cannot \
                     have one accumulated from it — refusing to publish {dst_key} unchecked"
                ))
            })?;
        let spec = ComposeSpec {
            key: dst_key,
            // Unused: every part is a BaseCopy, so nothing is read
            // locally. Named so a future Local part fails loudly.
            local_path: std::path::Path::new("/nonexistent/copy-object-reads-nothing-locally"),
            parts,
            base_key: Some(src_key),
            base_etag: guard,
            condition: condition.clone(),
            stamps: stamps.clone(),
            crc64: Some(crc),
            progress: None,
        };
        self.compose_generation(&spec).await
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
            version_id: resp.version_id().map(|v| v.to_string()),
        })
    }

    /// SigV4 query-string signing. The credential the URL carries is
    /// the SYNCER's, scoped to one GET of one key for the TTL — which
    /// is why the TTL is short and re-signed rather than set to S3's
    /// seven-day maximum: the URL is handed to every agent that asks
    /// for a clone, and it is a bearer token for that object until it
    /// expires.
    async fn presign_get(&self, key: &str, ttl_secs: u64) -> StoreResult<String> {
        let cfg = aws_sdk_s3::presigning::PresigningConfig::expires_in(
            std::time::Duration::from_secs(ttl_secs),
        )
        .map_err(|e| StoreError::Other(format!("presign config: {e}")))?;
        let req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(cfg)
            .await
            .map_err(|e| map_err("presign_get", e))?;
        Ok(req.uri().to_string())
    }

    async fn presign_put(&self, key: &str, ttl_secs: u64) -> StoreResult<String> {
        let cfg = aws_sdk_s3::presigning::PresigningConfig::expires_in(
            std::time::Duration::from_secs(ttl_secs),
        )
        .map_err(|e| StoreError::Other(format!("presign config: {e}")))?;
        let req = self
            .presign_client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(cfg)
            .await
            .map_err(|e| map_err("presign_put", e))?;
        Ok(req.uri().to_string())
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
            version_id: resp.version_id().map(|v| v.to_string()),
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

    /// As `get_range`, but hands back the SDK's own frames instead of
    /// copying them into one buffer. `into_segments` is
    /// `self.0.into_inner().into_iter()` — no allocation, no copy.
    async fn get_range_segments(
        &self,
        key: &str,
        offset: u64,
        len: u64,
        if_match: &str,
    ) -> StoreResult<Vec<Bytes>> {
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
            .map_err(|e| map_err("get_range_segments", e))?;
        let agg = resp
            .body
            .collect()
            .await
            .map_err(|e| StoreError::Other(format!("get_range_segments body: {}", e)))?;
        Ok(agg.into_segments().collect())
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

    async fn head_version(&self, key: &str, version_id: &str) -> StoreResult<ObjectMeta> {
        let resp = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .version_id(version_id)
            .checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled)
            .send()
            .await
            .map_err(|e| map_err("head_version", e))?;
        Ok(ObjectMeta {
            etag: resp.e_tag().unwrap_or_default().to_string(),
            size: resp.content_length().unwrap_or(0).max(0) as u64,
            crc64_b64: resp.checksum_crc64_nvme().map(|s| s.to_string()),
            meta: resp.metadata().cloned().unwrap_or_default(),
            last_modified_unix: dt_unix(resp.last_modified()),
            storage_class: resp.storage_class().map(|c| c.as_str().to_string()),
            version_id: resp.version_id().map(|v| v.to_string()),
        })
    }

    async fn get_version(&self, key: &str, version_id: &str) -> StoreResult<(ObjectMeta, Bytes)> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .version_id(version_id)
            .checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled)
            .send()
            .await
            .map_err(|e| map_err("get_version", e))?;
        let meta = ObjectMeta {
            etag: resp.e_tag().unwrap_or_default().to_string(),
            size: resp.content_length().unwrap_or(0).max(0) as u64,
            crc64_b64: resp.checksum_crc64_nvme().map(|s| s.to_string()),
            meta: resp.metadata().cloned().unwrap_or_default(),
            last_modified_unix: dt_unix(resp.last_modified()),
            storage_class: resp.storage_class().map(|c| c.as_str().to_string()),
            version_id: resp.version_id().map(|v| v.to_string()),
        };
        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|e| StoreError::Other(format!("get_version body: {}", e)))?
            .into_bytes();
        Ok((meta, bytes))
    }

    async fn delete_version(&self, key: &str, version_id: &str) -> StoreResult<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .version_id(version_id)
            .send()
            .await
            .map_err(|e| map_err("delete_version", e))?;
        Ok(())
    }

    async fn list_versions(&self, prefix: &str) -> StoreResult<Vec<ListedVersion>> {
        let mut out = Vec::new();
        let mut key_marker: Option<String> = None;
        let mut vid_marker: Option<String> = None;
        loop {
            let mut req =
                self.client.list_object_versions().bucket(&self.bucket).prefix(prefix);
            if let Some(k) = &key_marker {
                req = req.key_marker(k);
            }
            if let Some(v) = &vid_marker {
                req = req.version_id_marker(v);
            }
            let resp = req.send().await.map_err(|e| map_err("list_versions", e))?;
            for v in resp.versions() {
                out.push(ListedVersion {
                    key: v.key().unwrap_or_default().to_string(),
                    version_id: v.version_id().unwrap_or_default().to_string(),
                    etag: v.e_tag().unwrap_or_default().to_string(),
                    size: v.size().unwrap_or(0).max(0) as u64,
                    is_current: v.is_latest().unwrap_or(false),
                    is_delete_marker: false,
                    last_modified_unix: dt_unix(v.last_modified()),
                });
            }
            for d in resp.delete_markers() {
                out.push(ListedVersion {
                    key: d.key().unwrap_or_default().to_string(),
                    version_id: d.version_id().unwrap_or_default().to_string(),
                    etag: String::new(),
                    size: 0,
                    is_current: d.is_latest().unwrap_or(false),
                    is_delete_marker: true,
                    last_modified_unix: dt_unix(d.last_modified()),
                });
            }
            if resp.is_truncated().unwrap_or(false) {
                key_marker = resp.next_key_marker().map(|s| s.to_string());
                vid_marker = resp.next_version_id_marker().map(|s| s.to_string());
                if key_marker.is_none() && vid_marker.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(out)
    }

    async fn list_uploads(&self, prefix: &str) -> StoreResult<Vec<PendingUpload>> {
        let mut out = Vec::new();
        let mut key_marker: Option<String> = None;
        let mut id_marker: Option<String> = None;
        loop {
            // NO server-side Prefix: MinIO's ListMultipartUploads
            // returns an upload for its EXACT key as prefix but NOT
            // for a directory-style prefix (the chaos drill caught
            // the A8 abort-sweep silently seeing nothing there, so
            // kill -9 orphans survived every restart). Real S3
            // honors both — list bucket-wide and filter in code,
            // correct on either store. Buckets are per-volume, so
            // bucket-wide is the prefix's world plus control objects.
            let resp = self
                .client
                .list_multipart_uploads()
                .bucket(&self.bucket)
                .set_key_marker(key_marker.take())
                .set_upload_id_marker(id_marker.take())
                .send()
                .await
                .map_err(|e| map_err("list_multipart_uploads", e))?;
            for u in resp.uploads() {
                let key = u.key().unwrap_or_default();
                if !key.starts_with(prefix) {
                    continue;
                }
                out.push(PendingUpload {
                    key: key.to_string(),
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

    async fn lifecycle_rules(&self) -> StoreResult<Vec<LifecycleView>> {
        let cfg = match self
            .client
            .get_bucket_lifecycle_configuration()
            .bucket(&self.bucket)
            .send()
            .await
        {
            Ok(cfg) => cfg,
            // A bucket with no lifecycle config answers 404
            // NoSuchLifecycleConfiguration. That is a real, positive
            // answer: nothing is reaping noncurrent versions. Every
            // OTHER error stays an error — "I cannot see the rules"
            // must never read as "there are none", because gated mode
            // is accepted on the strength of that answer.
            Err(e) => match map_err("get_lifecycle", e) {
                StoreError::NotFound(_) => return Ok(vec![]),
                other => return Err(other),
            },
        };
        Ok(cfg
            .rules()
            .iter()
            .map(|r| LifecycleView {
                id: r.id().unwrap_or_default().to_string(),
                enabled: r.status() == &ExpirationStatus::Enabled,
                // Both filter shapes carry a prefix; the legacy
                // top-level `prefix` is still what many real buckets
                // use, and reading only `filter` would report a
                // fleet-wide destroyer as scoped to nothing.
                prefix: r
                    .filter()
                    .and_then(|f| f.prefix())
                    .map(|p| p.to_string())
                    .or_else(|| {
                        // Deprecated in the SDK, alive in real buckets:
                        // a fleet-wide rule written years ago carries
                        // the top-level prefix, and reading only
                        // `filter` would report the destroyer as
                        // scoped to nothing.
                        #[allow(deprecated)]
                        r.prefix().map(|p| p.to_string())
                    })
                    .unwrap_or_default(),
                noncurrent_days: r
                    .noncurrent_version_expiration()
                    .and_then(|n| n.noncurrent_days())
                    .map(|d| d as u64),
                expired_delete_marker: r
                    .expiration()
                    .and_then(|e| e.expired_object_delete_marker())
                    .unwrap_or(false),
            })
            .collect())
    }

    async fn ensure_noncurrent_retention(
        &self,
        prefix: &str,
        days: u64,
    ) -> StoreResult<RetentionOutcome> {
        let rule_id =
            format!("flint-lean-noncurrent-{}", prefix.trim_end_matches('/').replace('/', "-"));
        let existing = self
            .client
            .get_bucket_lifecycle_configuration()
            .bucket(&self.bucket)
            .send()
            .await;
        let mut rules: Vec<LifecycleRule> = match existing {
            Ok(cfg) => cfg.rules().to_vec(),
            Err(e) => match map_err("get_lifecycle", e) {
                StoreError::NotFound(_) => Vec::new(),
                // Refuse rather than write blind: PutBucketLifecycle-
                // Configuration is FULL-REPLACE, so appending to a list
                // we could not read would delete the MPU-abort rule and
                // every rule the customer owns.
                other => return Err(other),
            },
        };
        let want_days = i32::try_from(days)
            .map_err(|_| StoreError::Other(format!("retention {days} days does not fit")))?;
        if rules.iter().any(|r| {
            r.id() == Some(rule_id.as_str())
                && r.status() == &ExpirationStatus::Enabled
                && r.noncurrent_version_expiration().and_then(|n| n.noncurrent_days())
                    == Some(want_days)
        }) {
            return Ok(RetentionOutcome { rule_id, noncurrent_days: days, created: false });
        }
        rules.retain(|r| r.id() != Some(rule_id.as_str()));
        rules.push(
            LifecycleRule::builder()
                .id(&rule_id)
                .status(ExpirationStatus::Enabled)
                .filter(LifecycleRuleFilter::builder().prefix(prefix).build())
                .noncurrent_version_expiration(
                    NoncurrentVersionExpiration::builder().noncurrent_days(want_days).build(),
                )
                .expiration(
                    aws_sdk_s3::types::LifecycleExpiration::builder()
                        .expired_object_delete_marker(true)
                        .build(),
                )
                .build()
                .map_err(|e| StoreError::Other(format!("retention rule: {e}")))?,
        );
        self.client
            .put_bucket_lifecycle_configuration()
            .bucket(&self.bucket)
            .lifecycle_configuration(
                BucketLifecycleConfiguration::builder()
                    .set_rules(Some(rules))
                    .build()
                    .map_err(|e| StoreError::Other(format!("lifecycle configuration: {e}")))?,
            )
            .send()
            .await
            .map_err(|e| map_err("put_lifecycle", e))?;
        Ok(RetentionOutcome { rule_id, noncurrent_days: days, created: true })
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
                    released: body.released,
                    echo: body.echo,
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
            None,
        )
        .await
    }

    async fn epoch_renew(
        &self,
        key: &str,
        lease: &EpochLease,
        echo: Option<&str>,
    ) -> StoreResult<EpochLease> {
        self.epoch_put(
            key,
            &lease.holder_id,
            lease.epoch,
            PutCondition::IfMatch(lease.token.clone()),
            echo,
        )
        .await
    }

    async fn epoch_release(&self, key: &str, lease: &EpochLease) -> StoreResult<()> {
        // MARK, never delete. Deleting the cell would reset the epoch
        // counter — `epoch_acquire` computes the next epoch as
        // `supersede.map_or(1, |s| s.epoch + 1)`, so the next claimant
        // over an absent cell starts again at 1, and every publish
        // stamp plus `startup_reverify`'s monotonicity check would then
        // be comparing against numbers the volume has already used. The
        // released cell keeps its holder and epoch and simply says
        // "this holder is finished"; a successor supersedes at epoch+1
        // with no quiet wait.
        //
        // Guarded on the caller's own lease token, so a hub deposed
        // mid-shutdown cannot stamp `released` onto a live successor's
        // cell — that would invite a third hub to claim instantly while
        // the successor is serving.
        self.epoch_put_marked(
            key,
            &lease.holder_id,
            lease.epoch,
            PutCondition::IfMatch(lease.token.clone()),
            true,
            // A released cell reports no live sidecar: clearing the
            // echo is the point, not an omission.
            None,
        )
        .await
        .map(|_| ())
    }

    fn min_part_size(&self) -> u64 {
        S3_MIN_PART
    }

    fn max_parts(&self) -> usize {
        S3_MAX_PARTS
    }
}

impl S3Store {
    /// One `UploadPart` of local bytes. Factored out so the sequential
    /// and parallel arms of `compose_parts_and_complete` issue the SAME
    /// request — two copies of this would be two places to fix the next
    /// time a header changes, and only one of them would get fixed.
    async fn put_local_part(
        &self,
        spec: &ComposeSpec<'_>,
        upload_id: &str,
        part_number: i32,
        bytes: Bytes,
    ) -> StoreResult<CompletedPart> {
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
        Ok(CompletedPart::builder()
            .part_number(part_number)
            .set_e_tag(resp.e_tag().map(|s| s.to_string()))
            .set_checksum_crc64_nvme(resp.checksum_crc64_nvme().map(|s| s.to_string()))
            .build())
    }

    /// One part of a compose: read-or-copy, upload, and (when the store
    /// is computing the checksum) that part's OWN CRC over its OWN
    /// bytes. Independent of every other part by construction, which is
    /// what lets the caller run these concurrently.
    async fn compose_one_part(
        &self,
        spec: &ComposeSpec<'_>,
        upload_id: &str,
        part_number: i32,
        p: &PartSource,
        accumulate: bool,
    ) -> StoreResult<(i32, CompletedPart, Option<u64>, u64)> {
        match p {
            PartSource::Local { offset, len } => {
                let bytes = read_local(spec.local_path, *offset, *len).await?;
                // Hashed on a blocking thread while this part's PUT is
                // in flight, so hashing costs the upload nothing on the
                // wire.
                let hashing = if accumulate {
                    let b = bytes.clone();
                    Some(tokio::task::spawn_blocking(move || crc64_nvme(&b)))
                } else {
                    None
                };
                let cp = self.put_local_part(spec, upload_id, part_number, bytes).await?;
                let crc = match hashing {
                    Some(h) => Some(h.await.map_err(|e| {
                        StoreError::Other(format!("compose: checksum did not join: {e}"))
                    })?),
                    None => None,
                };
                spec.note_progress(*len);
                Ok((part_number, cp, crc, *len))
            }
            PartSource::BaseCopy { offset, len } => {
                let cp =
                    self.put_copy_part(spec, upload_id, part_number, *offset, *len).await?;
                spec.note_progress(*len);
                Ok((part_number, cp, None, *len))
            }
        }
    }

    /// One `UploadPartCopy` of a range of the base generation.
    async fn put_copy_part(
        &self,
        spec: &ComposeSpec<'_>,
        upload_id: &str,
        part_number: i32,
        offset: u64,
        len: u64,
    ) -> StoreResult<CompletedPart> {
        let base_etag = spec
            .base_etag
            .as_deref()
            .ok_or_else(|| StoreError::Other("compose: BaseCopy without base_etag".into()))?;
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
        Ok(CompletedPart::builder()
            .part_number(part_number)
            .set_e_tag(cp.and_then(|c| c.e_tag()).map(|s| s.to_string()))
            .set_checksum_crc64_nvme(cp.and_then(|c| c.checksum_crc64_nvme()).map(|s| s.to_string()))
            .build())
    }

    async fn compose_parts_and_complete(
        &self,
        spec: &ComposeSpec<'_>,
        upload_id: &str,
    ) -> StoreResult<ObjectMeta> {
        // Contiguity is validated over the WHOLE list BEFORE any part
        // moves. It used to be checked inside the upload loop, which was
        // fine while that loop was sequential — a parallel one would
        // already have bytes on the wire by the time part 7 turned out
        // to start at the wrong offset.
        let mut expect = 0u64;
        for (i, p) in spec.parts.iter().enumerate() {
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
        }

        // The full-object checksum, computed from the parts as they
        // are read when the caller did not bring one.
        let accumulate = spec.crc64.is_none();
        if accumulate && spec.parts.iter().any(|p| matches!(p, PartSource::BaseCopy { .. })) {
            return Err(StoreError::Other(
                "compose: the checksum cannot be accumulated over a copied part; supply crc64".into(),
            ));
        }

        // Accumulating NO LONGER pins this to the sequential loop. Each
        // part's CRC is taken over that part's own bytes, independently,
        // and the full-object value is FOLDED from them in part order by
        // `crc64_combine` — so a checksum the store computes itself is
        // now compatible with uploading parts concurrently. It used to
        // cost one or the other, and both callers paid: forge took the
        // single read and a sequential upload, lean took the parallel
        // upload and a whole extra pass over the file before it.
        let par = self.part_parallelism.max(1);

        // (part number, completed part, that part's own CRC, its length)
        let mut done: Vec<(i32, CompletedPart, Option<u64>, u64)> = if par <= 1 {
            let mut out = Vec::with_capacity(spec.parts.len());
            for (i, p) in spec.parts.iter().enumerate() {
                out.push(
                    self.compose_one_part(spec, upload_id, (i + 1) as i32, p, accumulate).await?,
                );
            }
            out
        } else {
            use futures::stream::StreamExt;
            // Every part is independent by construction: the list carries
            // explicit offsets and lengths, and the contiguity check above
            // already ran over all of them.
            // The futures are built by a LOOP, not by `.map(|..| async
            // move ..)`. As a closure this has to be higher-ranked over
            // the borrow of `PartSource` and is not, which rustc reports
            // from the trait method as the famously unhelpful
            // "implementation of `FnOnce` is not general enough".
            let mut futs = Vec::with_capacity(spec.parts.len());
            for (i, p) in spec.parts.iter().enumerate() {
                futs.push(self.compose_one_part(spec, upload_id, (i + 1) as i32, p, accumulate));
            }
            futures::stream::iter(futs)
                .buffer_unordered(par)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<StoreResult<Vec<_>>>()?
        };
        // CompleteMultipartUpload takes parts in ASCENDING part number,
        // and the fold below depends on the same order for a different
        // reason: `crc64_combine` composes A-then-B, so a shuffled grid
        // yields the checksum of a different object. `buffer_unordered`
        // yields in COMPLETION order, so this sort serves both.
        done.sort_by_key(|(n, _, _, _)| *n);

        // Fold left to right. `crc64_combine(acc, part, part_len)` is
        // the CRC of the concatenation, so folding over a CONTIGUOUS
        // grid is the full-object CRC — the contiguity check above is
        // what makes that true, not an assumption.
        let mut folded: Option<u64> = None;
        for (_, _, c, len) in &done {
            if let Some(c) = *c {
                folded = Some(match folded {
                    None => c,
                    Some(prev) => crc64_combine(prev, c, *len),
                });
            }
        }
        let completed: Vec<CompletedPart> = done.into_iter().map(|(_, cp, _, _)| cp).collect();

        let crc = match (spec.crc64, folded) {
            (Some(c), _) => c,
            (None, Some(a)) => a,
            (None, None) => {
                return Err(StoreError::Other("compose: no checksum to complete with".into()))
            }
        };
        let crc_b64 = crc64_to_b64(crc);
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
            PutCondition::Unconditional => req,
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
            version_id: resp.version_id().map(|v| v.to_string()),
        })
    }

    async fn epoch_put(
        &self,
        key: &str,
        holder_id: &str,
        epoch: u64,
        condition: PutCondition,
        echo: Option<&str>,
    ) -> StoreResult<EpochLease> {
        self.epoch_put_marked(key, holder_id, epoch, condition, false, echo).await
    }

    async fn epoch_put_marked(
        &self,
        key: &str,
        holder_id: &str,
        epoch: u64,
        condition: PutCondition,
        released: bool,
        echo: Option<&str>,
    ) -> StoreResult<EpochLease> {
        let body = Bytes::from(
            serde_json::to_vec(&EpochBody {
                holder_id: holder_id.to_string(),
                epoch,
                renewed_unix: now_unix(),
                salt: uuid::Uuid::new_v4().to_string(),
                released,
                echo: echo.map(|e| e.to_string()),
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
            PutCondition::Unconditional => req,
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

