//! The lean subtree manifest: one JSON document, CAS-written, the
//! atomic commit point every reader trusts (checkout, sync, DR).
//!
//! Deliberately NOT `tier::manifest`: the hub's writer rebuilds the
//! whole document from its local walk and cannot merge (its Entry has
//! no author identity), which is the amputation engine the lean review
//! reproduced three ways. This writer merges three-way:
//! base = the manifest view at OUR last install, mine = this barrier's
//! walk, theirs = bucket-current. Foreign entries are preserved and
//! surfaced for the next consume; a local delete loses to a foreign
//! modify (delete/modify resolves conservative — the formal model's
//! GC-vs-merge counterexample is exactly this case).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use flint_store::{
    crc64_nvme, GenerationStamps, ObjectMeta, ObjectStore, PutCondition, StoreError,
};

use super::{LeanConfig, LeanResult};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeanEntry {
    /// Object key (under `<prefix>/files/`).
    pub key: String,
    pub etag: String,
    pub crc64_b64: Option<String>,
    pub size: u64,
    pub mode: u32,
    pub mtime_unix: i64,
    pub generation: u64,
    /// The publishing writer's lease epoch (0 = a gateway/HITL write).
    pub epoch: u64,
    /// The object VERSION this citation names (boundary-verbs plan D7).
    ///
    /// `None` = a legacy or unversioned entry ⇒ readers take today's
    /// `get_whole(key, If-Match etag)` path verbatim. Mixed manifests
    /// are NORMAL during rollout and after any bucket-versioning
    /// change, so both forms are permanent reader cases, not a
    /// migration state. **The version id ADDRESSES; the etag ATTESTS** —
    /// the etag stays and is still verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LeanManifest {
    pub seq: u64,
    /// path -> entry. Paths are workspace-relative, '/'-separated.
    pub entries: BTreeMap<String, LeanEntry>,
    /// Set by a GATED citation (D13). Under `pinned_reads` flint's
    /// readers resolve EXCLUSIVELY by the cited `version_id` and never
    /// S3-wins-adopt the current version.
    ///
    /// The rule is load-bearing, not a refinement: the moment the gated
    /// lane stages a path, the cited etag stops matching current, so
    /// without it EVERY gated checkout would 412 on every dirty path
    /// and adopt uncited mid-logical-change bytes — the versioned
    /// design would leak its own staging into readers through exactly
    /// the arm the whole mode exists to avoid.
    ///
    /// Unset for cadence, hybrid and legacy manifests, which therefore
    /// keep the shipped 412/S3-wins arm byte-for-byte.
    #[serde(default)]
    pub pinned_reads: bool,
    /// Which citation source installed this manifest (§2.4.1). Also
    /// stamped on the object's metadata so it is readable by HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_source: Option<String>,
}

impl LeanManifest {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self).expect("manifest serializes")
    }
    pub fn parse(bytes: &[u8]) -> Result<LeanManifest, String> {
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
    }
}

/// The bucket-current manifest + its document ETag (the CAS token).
pub struct LoadedManifest {
    pub manifest: LeanManifest,
    pub etag: String,
}

pub async fn load(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
) -> LeanResult<Option<LoadedManifest>> {
    match store.get_whole(&cfg.manifest_key(), None).await {
        Ok((meta, bytes)) => {
            let manifest = LeanManifest::parse(&bytes)
                .map_err(|e| super::LeanError::State(format!("manifest parse: {e}")))?;
            Ok(Some(LoadedManifest { manifest, etag: meta.etag }))
        }
        Err(StoreError::NotFound(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// CAS-write the manifest document. `expected`: None ⇒ first write
/// (If-None-Match:*); Some(etag) ⇒ If-Match. The caller owns 412
/// handling (merge-retry or fence — never blind re-seed).
pub async fn cas_write(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    m: &LeanManifest,
    expected: Option<&str>,
    epoch: u64,
    flush_uuid: &str,
) -> LeanResult<ObjectMeta> {
    cas_write_stamped(store, cfg, m, expected, epoch, flush_uuid, None).await
}

/// `cas_write` with an explicit boundary-source stamp on the manifest
/// OBJECT (§2.4.1): downstream consumers distinguish a declared-coherent
/// citation from a forced possibly-torn one from the bucket alone, for
/// the price of a HEAD rather than a GET of a manifest that can run to
/// hundreds of MiB.
pub async fn cas_write_stamped(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    m: &LeanManifest,
    expected: Option<&str>,
    epoch: u64,
    flush_uuid: &str,
    boundary_source: Option<&str>,
) -> LeanResult<ObjectMeta> {
    // The manifest's own document must agree with its object stamp:
    // a reader that GETs and a reader that HEADs must never disagree
    // about how coherent the citation claims to be.
    let mut m = m.clone();
    if boundary_source.is_some() {
        m.boundary_source = boundary_source.map(|s| s.to_string());
    }
    let m = &m;
    let bytes = m.to_bytes();
    let crc = crc64_nvme(&bytes);
    let cond = match expected {
        Some(etag) => PutCondition::IfMatch(etag.to_string()),
        None => PutCondition::IfNoneMatchAny,
    };
    let stamps = GenerationStamps {
        generation: m.seq,
        epoch,
        flush_uuid: flush_uuid.to_string(),
        boundary_source: boundary_source.map(|s| s.to_string()),
        posix: None,
    };
    Ok(store
        .put_whole(&cfg.manifest_key(), bytes.into(), &cond, &stamps, crc)
        .await?)
}

/// The three-way merge (the model's `inst`). Starts from THEIRS so
/// foreign entries survive by construction; applies my upserts; applies
/// my deletes only where theirs is unchanged since my merge base.
///
/// **`mine_upserts` and `mine_deletes` must be DISJOINT**, and that is
/// the caller's job, not this function's: which one wins depends on
/// which the tree saw LAST, and neither set carries that. The fused
/// barrier gets it free (both are computed from one scan); the gated
/// lane, whose stage and tombstones persist across ticks, cancels each
/// against the other as it observes them. Resolving an overlap here —
/// in either direction — cites a deleted file half the time and
/// amputates a live one the other half. TLC found both halves.
/// Returns the merged document plus the foreign entries a consume must
/// integrate next (present in theirs, changed vs base, not mine).
pub fn merge(
    base: &BTreeMap<String, String>,
    theirs: &LeanManifest,
    mine_upserts: &BTreeMap<String, LeanEntry>,
    mine_deletes: &BTreeSet<String>,
    parked: &BTreeSet<String>,
) -> (LeanManifest, Vec<(String, LeanEntry)>) {
    let mut merged = theirs.clone();
    merged.seq = theirs.seq + 1;
    // Set explicitly by the installing pass; never inherited from
    // whoever wrote last (a cadence barrier must not inherit a gated
    // predecessor's `pinned_reads`, and vice versa).
    merged.pinned_reads = false;
    merged.boundary_source = None;

    let mut foreign: Vec<(String, LeanEntry)> = vec![];
    for (p, e) in &theirs.entries {
        let changed = base.get(p).map(|b| b != &e.etag).unwrap_or(true);
        if changed && !mine_upserts.contains_key(p) && !parked.contains(p) {
            foreign.push((p.clone(), e.clone()));
        }
    }

    for (p, e) in mine_upserts {
        merged.entries.insert(p.clone(), e.clone());
    }
    for p in mine_deletes {
        if parked.contains(p) {
            continue;
        }

        let theirs_unchanged = match (theirs.entries.get(p), base.get(p)) {
            (Some(e), Some(b)) => &e.etag == b,
            (None, None) => true,
            // Present in theirs but not in base (foreign add), or
            // vanished from theirs (someone else already deleted):
            // either way our delete does not apply.
            _ => false,
        };
        if theirs_unchanged {
            merged.entries.remove(p);
        }
    }
    (merged, foreign)
}

/// Takeover fence rotation (plan §2.2): the successor CAS-rewrites the
/// manifest seq++ content-identical BEFORE serving, so any deposed
/// straggler's in-flight CAS 412s and stays failed. Returns the rotated
/// document's ETag (the successor's new CAS token); None if no manifest
/// exists yet (nothing to rotate — a fresh subtree).
pub async fn rotate_for_takeover(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    epoch: u64,
) -> LeanResult<Option<(LeanManifest, String)>> {
    for _ in 0..3 {
        let Some(loaded) = load(store, cfg).await? else {
            return Ok(None);
        };
        let mut rotated = loaded.manifest.clone();
        rotated.seq += 1;
        match cas_write(store, cfg, &rotated, Some(&loaded.etag), epoch, "takeover-rotation").await
        {
            Ok(meta) => return Ok(Some((rotated, meta.etag))),
            Err(super::LeanError::Store(StoreError::PreconditionFailed(_))) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(super::LeanError::State(
        "takeover rotation lost 3 CAS races — a live writer is still publishing".into(),
    ))
}
