//! The version-surface conformance probe (lean boundary-verbs plan D8).
//!
//! It lives HERE, in the store crate, because it has two callers in two
//! crates that must never disagree: the sidecar runs it at startup and
//! REFUSES gated mode on failure, and the operator runs it on its
//! reconcile cadence and flips `BoundaryModeAccepted=False`. Two copies
//! of "is this bucket conformant?" would eventually answer differently,
//! and the failure that produces — a workspace the operator calls
//! healthy and the sidecar refuses to start — is worse than either
//! answer alone.
//!
//! What it refuses, and why refusal rather than degradation: a
//! project-scoped proxy that strips `x-amz-version-id` makes every
//! staged entry carry `None`, and citation then falls back to etag
//! semantics on a key whose CURRENT version is uncited — precisely the
//! torn view gated mode exists to prevent. Silent degradation into the
//! hazard is the one outcome that must be impossible.

use bytes::Bytes;

use crate::{crc64_nvme, GenerationStamps, ObjectStore, PutCondition, StoreError};

/// Run the probe against `key`. `Ok(())` means the whole surface is
/// present; `Err` names the exact step that failed, for a condition
/// message an operator can act on.
///
/// Callers must use DISTINCT keys per principal: the sidecar and the
/// operator probe on independent clocks, and a shared key would let two
/// conformant probes fail each other's `If-None-Match` write.
pub async fn probe_version_surface(store: &dyn ObjectStore, key: &str) -> Result<(), String> {
    let stamps = GenerationStamps {
        generation: 0,
        epoch: 0,
        flush_uuid: "version-probe".into(),
        boundary_source: None,
        posix: None,
    };
    let refuse = |m: &str| m.to_string();

    // The probe's OWN crash window, closed. It writes If-None-Match and
    // cleans up at the end; a crash — or one failed cleanup DELETE —
    // leaves the object behind and every later probe 412s on its first
    // write. On a gated workspace that would be a permanent startup
    // wedge produced by a transient error, so a leftover is swept.
    let b1 = Bytes::from_static(b"probe-1");
    let mut first = store
        .put_whole(key, b1.clone(), &PutCondition::IfNoneMatchAny, &stamps, crc64_nvme(b"probe-1"))
        .await;
    if matches!(first, Err(StoreError::PreconditionFailed(_))) {
        sweep(store, key).await;
        first = store
            .put_whole(key, b1, &PutCondition::IfNoneMatchAny, &stamps, crc64_nvme(b"probe-1"))
            .await;
    }
    let m1 = first.map_err(|_| refuse("cannot write the probe object"))?;
    // Everything from here down is the VERSION surface. The
    // conditional-write half above is what `probe_conditional_writes`
    // checks on its own, for callers that arbitrate but do not cite
    // versions.
    let v1 = m1.version_id.clone().ok_or_else(|| {
        refuse("PUT returned no x-amz-version-id (versioning off, or a proxy strips the header)")
    })?;

    let b2 = Bytes::from_static(b"probe-2");
    let m2 = store
        .put_whole(
            key,
            b2,
            &PutCondition::IfMatch(m1.etag.clone()),
            &stamps,
            crc64_nvme(b"probe-2"),
        )
        .await
        .map_err(|_| refuse("cannot supersede the probe object"))?;
    let v2 = m2.version_id.clone().ok_or_else(|| refuse("no version id on the second PUT"))?;
    if v1 == v2 {
        return Err(refuse("the backend reused one version id for two PUTs"));
    }

    // The first version must still be fetchable BY ID — this is the read
    // `pinned_reads` citations depend on.
    let (_, body) = store
        .get_version(key, &v1)
        .await
        .map_err(|_| refuse("version-scoped GET is unavailable"))?;
    if body.as_ref() != b"probe-1" {
        return Err(refuse("version-scoped GET returned the wrong generation"));
    }
    store
        .head_version(key, &v1)
        .await
        .map_err(|_| refuse("version-scoped HEAD is unavailable"))?;
    let listed = store
        .list_versions(key)
        .await
        .map_err(|_| refuse("ListObjectVersions is not permitted"))?;
    if listed.iter().filter(|v| v.key == key && !v.is_delete_marker).count() < 2 {
        return Err(refuse("ListObjectVersions did not report both generations"));
    }
    store
        .delete_version(key, &v1)
        .await
        .map_err(|_| refuse("version-scoped DELETE is unavailable"))?;
    if store.head_version(key, &v1).await.is_ok() {
        return Err(refuse("version-scoped DELETE did not remove the version"));
    }
    let _ = store.delete_version(key, &v2).await;
    // Belt and braces: leave the key with no versions at all, so a
    // partially-failed cleanup cannot wedge the NEXT probe either.
    sweep(store, key).await;
    Ok(())
}

/// Remove every version of the probe key. Errors are ignored on
/// purpose: this is hygiene, and the probe's verdict must come from the
/// probe, not from a cleanup DELETE.
async fn sweep(store: &dyn ObjectStore, key: &str) {
    if let Ok(vs) = store.list_versions(key).await {
        for v in vs.iter().filter(|v| v.key == key) {
            let _ = store.delete_version(key, &v.version_id).await;
        }
    }
}

/// The CONDITIONAL-WRITE surface alone — a strict subset of
/// [`probe_version_surface`], for callers that arbitrate with
/// `If-Match`/`If-None-Match` but never cite a version id.
///
/// The flint-lite tier is exactly that caller. Its A6 arbitration is
/// compare-and-swap: the flusher publishes under `If-Match` on the
/// generation it read, and a 412 is how it tells its own torn flush
/// from a genuinely foreign overwrite. Against a store that ACCEPTS the
/// conditional headers and ignores them, every one of those PUTs
/// succeeds unconditionally and arbitration degrades — silently — to
/// last-writer-wins. Nothing else in the tier would notice.
///
/// Deliberately does NOT require bucket versioning: a lite bucket needs
/// no version ids, and demanding them would refuse a perfectly good
/// deployment. That is the whole reason this is not just a call to the
/// bigger probe.
pub async fn probe_conditional_writes(store: &dyn ObjectStore, key: &str) -> Result<(), String> {
    let stamps = GenerationStamps {
        generation: 0,
        epoch: 0,
        flush_uuid: "cond-probe".into(),
        boundary_source: None,
        posix: None,
    };
    let refuse = |m: &str| m.to_string();
    let body = |b: &'static [u8]| (Bytes::from_static(b), crc64_nvme(b));

    // Leftovers from a crashed probe would 412 the first write below
    // and read as a conformant store. Clear first.
    let _ = store.delete(key).await;

    let (b1, c1) = body(b"cond-probe-1");
    let m1 = store
        .put_whole(key, b1, &PutCondition::IfNoneMatchAny, &stamps, c1)
        .await
        .map_err(|_| refuse("cannot write the probe object"))?;

    // If-None-Match:* over an object that now EXISTS must be refused.
    // A store that ignores the header answers Ok here, and that is the
    // silent degradation this probe exists to catch.
    let (b2, c2) = body(b"cond-probe-2");
    match store.put_whole(key, b2, &PutCondition::IfNoneMatchAny, &stamps, c2).await {
        Err(StoreError::PreconditionFailed(_)) => {}
        Ok(_) => {
            let _ = store.delete(key).await;
            return Err(refuse(
                "If-None-Match:* was ACCEPTED over an existing object — this store does not \
                 enforce conditional writes, so tier arbitration would silently degrade to \
                 last-writer-wins",
            ));
        }
        Err(e) => {
            let _ = store.delete(key).await;
            return Err(refuse(&format!("If-None-Match:* failed unexpectedly: {e}")));
        }
    }

    // If-Match on a STALE etag must be refused...
    let (b3, c3) = body(b"cond-probe-3");
    let stale = PutCondition::IfMatch("\"0000000000000000000000000000dead\"".to_string());
    match store.put_whole(key, b3, &stale, &stamps, c3).await {
        Err(StoreError::PreconditionFailed(_)) => {}
        Ok(_) => {
            let _ = store.delete(key).await;
            return Err(refuse(
                "If-Match on a STALE etag was ACCEPTED — a foreign overwrite would be \
                 indistinguishable from this hub's own publish",
            ));
        }
        Err(e) => {
            let _ = store.delete(key).await;
            return Err(refuse(&format!("If-Match(stale) failed unexpectedly: {e}")));
        }
    }

    // ...and on the CURRENT etag it must succeed, or the tier could
    // never publish a second generation at all. Without this leg a
    // store that refused every conditional PUT would score as
    // conformant.
    let (b4, c4) = body(b"cond-probe-4");
    let r = store.put_whole(key, b4, &PutCondition::IfMatch(m1.etag.clone()), &stamps, c4).await;
    let _ = store.delete(key).await;
    r.map_err(|e| refuse(&format!("If-Match on the CURRENT etag was refused: {e}")))?;
    Ok(())
}

/// The cross-key copy surface (`copy_object`). Run against a REAL
/// bucket; the memory double cannot tell you whether `CopyObject`
/// honours `x-amz-copy-source-if-match`, whether `MetadataDirective:
/// REPLACE` actually replaces, or whether the MPU arm's
/// `UploadPartCopy` assembles what we think.
///
/// `key` is a PREFIX: the probe writes `<key>.src` and `<key>.dst` and
/// removes both. As with the other probes, distinct keys per principal.
///
/// `force_mpu_over` lets a caller drive the MPU arm without a 5 GiB
/// object by lowering the store's single-request ceiling first — the
/// arm that has never executed anywhere is exactly the one a probe
/// should reach.
pub async fn probe_cross_key_copy(store: &dyn ObjectStore, key: &str) -> Result<(), String> {
    let src = format!("{key}.src");
    let dst = format!("{key}.dst");
    let body = Bytes::from_static(b"cross-key copy probe payload, non-uniform: \x01\x02\x03");
    let src_crc = crc64_nvme(&body);
    let stamps = |g: u64, f: &str| GenerationStamps {
        generation: g,
        epoch: 0,
        flush_uuid: f.into(),
        boundary_source: None,
        posix: None,
    };

    // Clean start: a leftover from a torn probe must not fail the
    // If-None-Match writes below (the leftover-object lesson the
    // conditional-writes probe already learned).
    let _ = store.delete(&src).await;
    let _ = store.delete(&dst).await;

    let s = store
        .put_whole(&src, body.clone(), &PutCondition::IfNoneMatchAny, &stamps(9, "probe-src"), src_crc)
        .await
        .map_err(|e| format!("seed the copy source: {e}"))?;

    // 1. A stale source guard must REFUSE. Nothing may land.
    match store
        .copy_object(&src, Some("\"not-the-etag\""), &dst, &PutCondition::IfNoneMatchAny, &stamps(1, "probe-stale"))
        .await
    {
        Err(StoreError::PreconditionFailed(_)) => {}
        Ok(_) => {
            let _ = store.delete(&dst).await;
            let _ = store.delete(&src).await;
            return Err("copy with a STALE source etag succeeded — the copy-source guard is \
                        not enforced, so a rename can move bytes the caller never read"
                .into());
        }
        Err(e) => {
            let _ = store.delete(&src).await;
            return Err(format!("copy with a stale source etag failed with the wrong error: {e}"));
        }
    }
    if store.head(&dst).await.is_ok() {
        let _ = store.delete(&dst).await;
        let _ = store.delete(&src).await;
        return Err("a REFUSED copy still landed an object at the destination".into());
    }

    // 2. The real copy.
    let d = store
        .copy_object(&src, Some(&s.etag), &dst, &PutCondition::IfNoneMatchAny, &stamps(1, "probe-dst"))
        .await
        .map_err(|e| format!("copy {src} -> {dst}: {e}"))?;

    // 3. Bytes identical, checksum identical.
    let (dmeta, got) = store.get_whole(&dst, None).await.map_err(|e| format!("read back {dst}: {e}"))?;
    if got != body {
        return Err(format!("the copy is not byte-identical: {} bytes vs {}", got.len(), body.len()));
    }
    if crc64_nvme(&got) != src_crc {
        return Err("the copy's CONTENT checksum differs from the source's".into());
    }
    if d.crc64_b64 != s.crc64_b64 {
        return Err(format!(
            "the copy REPORTS a different checksum than the source: {:?} vs {:?} — identical \
             bytes cannot have different CRCs, so one of the two is not describing its object",
            d.crc64_b64, s.crc64_b64
        ));
    }

    // 4. Stamps are the DESTINATION's. This is the one a double cannot
    //    check: `MetadataDirective::REPLACE` is a server behaviour.
    let gen = dmeta.meta.get(GenerationStamps::META_GEN).cloned().unwrap_or_default();
    let flush = dmeta.meta.get(GenerationStamps::META_FLUSH_UUID).cloned().unwrap_or_default();
    if gen != "1" || flush != "probe-dst" {
        return Err(format!(
            "the copy inherited the SOURCE's stamps (gen={gen:?} flush={flush:?}, wanted \
             gen=\"1\" flush=\"probe-dst\") — MetadataDirective did not replace, so one \
             object's publish history is filed under another object's key"
        ));
    }

    // 5. The source survives: a copy, not a move.
    if store.head(&src).await.is_err() {
        return Err("the SOURCE is gone after a copy".into());
    }

    // 6. An occupied destination under If-None-Match must refuse.
    match store
        .copy_object(&src, Some(&s.etag), &dst, &PutCondition::IfNoneMatchAny, &stamps(2, "probe-clobber"))
        .await
    {
        Err(StoreError::PreconditionFailed(_)) => {}
        Ok(_) => {
            return Err("a copy CLOBBERED an existing destination under If-None-Match — a \
                        rename would silently overwrite whatever it landed on"
                .into())
        }
        Err(e) => return Err(format!("copy onto an occupied destination: wrong error: {e}")),
    }

    let _ = store.delete(&dst).await;
    let _ = store.delete(&src).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;

    /// The probe must PASS on a store that behaves. This is the leg
    /// that matters most in practice: the failure mode I was closest to
    /// shipping was a probe that over-demanded — `probe_version_surface`
    /// requires bucket VERSIONING, which a lite bucket does not need,
    /// so wiring that one into the hub would have refused perfectly
    /// good deployments at startup.
    ///
    /// The negative direction — a store that ACCEPTS conditional
    /// headers and ignores them — cannot be built here without a full
    /// non-enforcing `ObjectStore` double; it belongs in the real-S3 /
    /// MinIO acceptance drill, pointed at a backend that actually
    /// misbehaves. What stands in for it in-process is the probe's own
    /// fourth leg: `If-Match` on the CURRENT etag must SUCCEED, so a
    /// store that simply refused every conditional PUT could not score
    /// as conformant either.
    #[tokio::test]
    async fn a_conformant_store_passes_the_conditional_write_probe() {
        let store = MemoryStore::new();
        probe_conditional_writes(&store, "t/.flint/probe/conditional-writes")
            .await
            .expect("MemoryStore enforces conditional writes and must pass");
    }

    /// Leftovers from a crashed probe must not read as conformance: the
    /// first `If-None-Match:*` would 412 against them, which is the
    /// same answer a working store gives to leg 2. Without the opening
    /// sweep the probe would pass on a store it never actually tested.
    #[tokio::test]
    async fn a_leftover_probe_object_does_not_fake_a_pass() {
        let store = MemoryStore::new();
        let key = "t/.flint/probe/leftover";
        let stamps = GenerationStamps {
            generation: 0,
            epoch: 0,
            flush_uuid: "leftover".into(),
            boundary_source: None,
            posix: None,
        };
        store
            .put_whole(
                key,
                Bytes::from_static(b"stale"),
                &PutCondition::IfNoneMatchAny,
                &stamps,
                crc64_nvme(b"stale"),
            )
            .await
            .expect("seed the leftover");

        probe_conditional_writes(&store, key).await.expect("must still probe correctly");
    }

    /// The probe must PASS a conformant store...
    #[tokio::test]
    async fn cross_key_copy_probe_passes_on_a_conformant_store() {
        let store = crate::memory::MemoryStore::new();
        probe_cross_key_copy(&store, "probe/copy").await.expect("the double is conformant");
    }

    /// ...and must FAIL a store that gets the one thing wrong that a
    /// memory double can never catch on its own: `MetadataDirective`.
    /// A probe nothing can fail is a probe that proves nothing.
    #[tokio::test]
    async fn cross_key_copy_probe_catches_inherited_stamps() {
        use crate::{
            BootstrapReport, ComposeSpec, EpochLease, EpochState, ListedObject, ObjectMeta,
            PendingUpload, StoreResult,
        };
        struct InheritsStamps(crate::memory::MemoryStore);
        #[async_trait::async_trait]
        impl ObjectStore for InheritsStamps {
            async fn copy_object(
                &self,
                src_key: &str,
                src_if_match: Option<&str>,
                dst_key: &str,
                condition: &PutCondition,
                _stamps: &GenerationStamps,
            ) -> crate::StoreResult<crate::ObjectMeta> {
                // `MetadataDirective: COPY`: the destination wears the
                // SOURCE's generation and flush_uuid.
                let src = self.0.head(src_key).await?;
                let inherited =
                    GenerationStamps::from_meta(&src.meta).unwrap_or(GenerationStamps {
                        generation: 0,
                        epoch: 0,
                        flush_uuid: String::new(),
                        boundary_source: None,
                        posix: None,
                    });
                self.0.copy_object(src_key, src_if_match, dst_key, condition, &inherited).await
            }
            async fn put_whole( &self, key: &str, body: Bytes, condition: &PutCondition, stamps: &GenerationStamps, crc64: u64, ) -> StoreResult<ObjectMeta> {
                self.0.put_whole(key, body, condition, stamps, crc64).await
            }
            async fn compose_generation(&self, spec: &ComposeSpec<'_>) -> StoreResult<ObjectMeta> {
                self.0.compose_generation(spec).await
            }
            async fn head(&self, key: &str) -> StoreResult<ObjectMeta> {
                self.0.head(key).await
            }
            async fn get_whole(&self, key: &str, if_match: Option<&str>) -> StoreResult<(ObjectMeta, Bytes)> {
                self.0.get_whole(key, if_match).await
            }
            async fn get_range( &self, key: &str, offset: u64, len: u64, if_match: &str, ) -> StoreResult<Bytes> {
                self.0.get_range(key, offset, len, if_match).await
            }
            async fn list(&self, prefix: &str) -> StoreResult<Vec<ListedObject>> {
                self.0.list(prefix).await
            }
            async fn delete(&self, key: &str) -> StoreResult<()> {
                self.0.delete(key).await
            }
            async fn list_uploads(&self, prefix: &str) -> StoreResult<Vec<PendingUpload>> {
                self.0.list_uploads(prefix).await
            }
            async fn abort_upload(&self, key: &str, upload_id: &str) -> StoreResult<()> {
                self.0.abort_upload(key, upload_id).await
            }
            async fn bootstrap(&self, prefix: &str) -> StoreResult<BootstrapReport> {
                self.0.bootstrap(prefix).await
            }
            async fn epoch_read(&self, key: &str) -> StoreResult<Option<EpochState>> {
                self.0.epoch_read(key).await
            }
            async fn epoch_acquire( &self, key: &str, holder_id: &str, supersede: Option<&EpochState>, ) -> StoreResult<EpochLease> {
                self.0.epoch_acquire(key, holder_id, supersede).await
            }
            async fn epoch_renew( &self, key: &str, lease: &EpochLease, echo: Option<&str>, ) -> StoreResult<EpochLease> {
                self.0.epoch_renew(key, lease, echo).await
            }
            async fn epoch_release(&self, key: &str, lease: &EpochLease) -> StoreResult<()> {
                self.0.epoch_release(key, lease).await
            }
            fn min_part_size(&self) -> u64 {
                self.0.min_part_size()
            }
            fn max_parts(&self) -> usize {
                self.0.max_parts()
            }
        }

        let store = InheritsStamps(crate::memory::MemoryStore::new());
        let err = probe_cross_key_copy(&store, "probe/copy")
            .await
            .expect_err("inherited stamps must be caught");
        assert!(err.contains("inherited the SOURCE's stamps"), "{err}");
    }
}
