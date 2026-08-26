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
