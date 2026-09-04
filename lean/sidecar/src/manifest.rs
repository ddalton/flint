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
    /// COMPACT, not pretty. This document is O(entries) and it moves on
    /// every path that matters: the pre-marker checkout GET, every
    /// barrier merge GET and CAS PUT, and every gateway read verb. The
    /// indentation was ~22% of those bytes and of the parse that
    /// follows. `mc cat | jq` — the operator's and every rig's only
    /// debugging surface for the CAS commit point — is unaffected.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("manifest serializes")
    }
    pub fn parse(bytes: &[u8]) -> Result<LeanManifest, String> {
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
    }
}

/// The bucket-current manifest + its document ETag (the CAS token).
/// The pointer document at `<prefix>/.flint/lean/current` — the ONLY
/// mutable metadata object under the pointer layout (design of record:
/// `docs/plans/flint-lean-manifest-pointer-design.md`).
///
/// Everything a reader needs before it decides whether to fetch entries
/// at all lives here, so the barrier's idle tick reads a few hundred
/// bytes instead of a document that runs to 264 MiB at 1M entries.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pointer {
    /// The manifest generation this pointer publishes. Bumped by a
    /// citation AND by a takeover rotation, which is why it is not the
    /// same number as `entries_seq`.
    pub seq: u64,
    /// The immutable object holding the entries of this generation.
    pub entries_key: String,
    /// The `seq` the entries object was written under. A rotation
    /// leaves this ALONE while bumping `seq`, which is exactly what
    /// lets a follower skip the entries GET after a takeover.
    pub entries_seq: u64,
    #[serde(default)]
    pub pinned_reads: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_source: Option<String>,
    /// The epoch that installed this pointer. Diagnostic; the fence is
    /// the CAS, not this field.
    pub epoch: u64,
}

/// What a writer must present to CAS. Carries the LAYOUT as well as the
/// tag, because a workspace mid-migration has a legacy manifest and no
/// pointer yet, and the two cases take different preconditions: an
/// If-Match on the object the writer read, versus an If-None-Match on a
/// pointer that must not already exist.
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestHandle {
    pub etag: String,
    /// True when the document was read from the LEGACY single-object
    /// key, i.e. this write is the migration.
    pub legacy: bool,
}

/// What the legacy key is overwritten with once a workspace has moved to
/// the pointer layout. It must NOT parse as a manifest: `LeanManifest`
/// gives `seq` and `entries` no serde default, so an OLD `flint-sync`
/// reading this gets `LeanError::State` and refuses.
///
/// Deleting the legacy key instead would be a data-loss shape, not a
/// tidy-up: `manifest::load` maps a missing object to `Ok(None)`, `None`
/// means FIRST WRITE, and a barrier answers that with
/// `IfNoneMatchAny` — so an old syncer would conclude the project is
/// empty and re-seed over it.
pub const REFUSAL_DOC: &str = concat!(
    "{\"moved\":\".flint/lean/current\",",
    "\"note\":\"this workspace uses the manifest pointer layout; upgrade flint-sync\"}"
);

pub struct LoadedManifest {
    pub manifest: LeanManifest,
    /// The CAS handle: the POINTER's etag under the pointer layout, the
    /// legacy object's etag before migration.
    pub etag: String,
    /// The object's own Last-Modified. The GET already carries it; a
    /// caller that wants the coherence stamp must not pay a second
    /// request for what this one threw away (review: U31).
    pub last_modified_unix: Option<u64>,
    /// `None` ⇒ this workspace is still on the legacy single-object
    /// layout and the next write migrates it.
    pub pointer: Option<Pointer>,
}

impl LoadedManifest {
    /// The handle a writer presents to CAS over this read.
    pub fn handle(&self) -> ManifestHandle {
        ManifestHandle { etag: self.etag.clone(), legacy: self.pointer.is_none() }
    }
}

/// Read the pointer alone. The barrier's idle tick wants exactly this
/// and nothing else: a few hundred bytes that answer "did anything
/// move, and were the ENTRIES part of it".
pub struct LoadedPointer {
    pub pointer: Pointer,
    pub etag: String,
    /// The pointer object's own Last-Modified — the citation's clock,
    /// which /status reports and which the GET already carries.
    pub last_modified_unix: Option<u64>,
}

pub async fn load_pointer(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
) -> LeanResult<Option<LoadedPointer>> {
    match store.get_whole(&cfg.current_key(), None).await {
        Ok((meta, bytes)) => {
            let pointer: Pointer = serde_json::from_slice(&bytes)
                .map_err(|e| super::LeanError::State(format!("manifest pointer parse: {e}")))?;
            Ok(Some(LoadedPointer { pointer, etag: meta.etag, last_modified_unix: meta.last_modified_unix }))
        }
        Err(StoreError::NotFound(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The whole document: pointer, then the generation it names. Falls
/// back to the legacy single-object key for a workspace that has not
/// been migrated — which every existing workspace is, exactly once.
pub async fn load(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
) -> LeanResult<Option<LoadedManifest>> {
    if let Some(LoadedPointer { pointer: p, etag, last_modified_unix: pointer_lm }) =
        load_pointer(store, cfg).await?
    {
        let (meta, bytes) = match store.get_whole(&p.entries_key, None).await {
            Ok(v) => v,
            // A pointer naming an object that is not there is not an
            // empty workspace — it is a broken one, and answering
            // `None` would invite a re-seed over a live project.
            Err(StoreError::NotFound(_)) => {
                return Err(super::LeanError::State(format!(
                    "manifest pointer at seq {} names {}, which does not exist — refusing to treat a broken \
                     pointer as an empty workspace",
                    p.seq, p.entries_key
                )))
            }
            Err(e) => return Err(e.into()),
        };
        let mut manifest = LeanManifest::parse(&bytes)
            .map_err(|e| super::LeanError::State(format!("manifest parse: {e}")))?;
        // The pointer is the authority for the fields it carries: a
        // rotation moves `seq` without rewriting the entries object, so
        // the generation's own copy is stale by construction.
        manifest.seq = p.seq;
        manifest.pinned_reads = p.pinned_reads;
        manifest.boundary_source = p.boundary_source.clone();
        return Ok(Some(LoadedManifest {
            manifest,
            etag,
            // The CITATION's clock is when the pointer moved, not when
            // the entries object happened to be written: a rotation
            // republishes the same entries under a new generation.
            last_modified_unix: pointer_lm.or(meta.last_modified_unix),
            pointer: Some(p),
        }));
    }
    match store.get_whole(&cfg.manifest_key(), None).await {
        Ok((meta, bytes)) => {
            let manifest = LeanManifest::parse(&bytes)
                .map_err(|e| super::LeanError::State(format!("manifest parse: {e}")))?;
            Ok(Some(LoadedManifest {
                manifest,
                etag: meta.etag,
                last_modified_unix: meta.last_modified_unix,
                pointer: None,
            }))
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
    expected: Option<&ManifestHandle>,
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
    expected: Option<&ManifestHandle>,
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
    let stamps = GenerationStamps {
        generation: m.seq,
        epoch,
        flush_uuid: flush_uuid.to_string(),
        // Stamp what the DOCUMENT carries, not merely what this call
        // passed. An explicit source has already been written into the
        // document above, so the two agree there either way; the case
        // this closes is a `None` caller over a document that already
        // names its clock — the gateway's HITL CAS relays a document it
        // did not author. Stamping `None` on those left the object
        // saying nothing while the document said `sentinel`, which is
        // exactly the GET/HEAD divergence the contract above forbids.
        boundary_source: m.boundary_source.clone(),
        posix: None,
    };

    // 1. The ENTRIES, to a fresh immutable key. `IfNoneMatchAny` is the
    //    whole safety story for this object: two writers that raced to
    //    the same seq cannot both land, and the loser is told so here
    //    rather than discovering it after it has already published.
    let entries_key = cfg.generation_key(m.seq, flush_uuid);
    match store
        .put_whole(&entries_key, bytes.into(), &PutCondition::IfNoneMatchAny, &stamps, crc)
        .await
    {
        Ok(_) => {}
        // The key carries this writer's flush uuid, so this is not a
        // race with another writer — it is a retry of OUR OWN write
        // whose pointer CAS we never learned the outcome of. The object
        // is byte-identical by construction; fall through to the CAS,
        // which is where the race is actually decided.
        Err(StoreError::PreconditionFailed(_)) | Err(StoreError::Conflict(_)) => {}
        Err(e) => return Err(e.into()),
    }

    // 2. The POINTER, which is what makes the publish visible and what
    //    decides the race. Everything above this line is invisible to
    //    every reader; a crash here leaves an orphan generation object
    //    that nobody reads and the sweep collects.
    let pointer = Pointer {
        seq: m.seq,
        entries_key: entries_key.clone(),
        entries_seq: m.seq,
        pinned_reads: m.pinned_reads,
        boundary_source: m.boundary_source.clone(),
        epoch,
    };
    put_pointer(store, cfg, &pointer, expected, &stamps).await
}

/// CAS the pointer. Split out because a takeover rotation writes ONLY
/// this — no entries object, no multi-MB anything.
async fn put_pointer(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    pointer: &Pointer,
    expected: Option<&ManifestHandle>,
    stamps: &GenerationStamps,
) -> LeanResult<ObjectMeta> {
    let body = serde_json::to_vec(pointer)
        .map_err(|e| super::LeanError::State(format!("pointer encode: {e}")))?;
    let crc = crc64_nvme(&body);
    // A LEGACY handle is a migration: the writer read the old single
    // object and the pointer does not exist yet, so the precondition is
    // "no pointer", not "the pointer I read". A concurrent migration
    // therefore loses here and retries against the layout that won,
    // instead of both of them believing they installed it.
    let cond = match expected {
        Some(h) if !h.legacy => PutCondition::IfMatch(h.etag.clone()),
        _ => PutCondition::IfNoneMatchAny,
    };
    let meta = store.put_whole(&cfg.current_key(), body.into(), &cond, stamps, crc).await?;
    if let Some(h) = expected.filter(|h| h.legacy) {
        // The workspace has moved. Poison the legacy key rather than
        // delete it — see REFUSAL_DOC. Conditional on the very object
        // this writer read, so a racing writer's view is never
        // clobbered, and best effort: the refusal doc is decorative to
        // every new reader, and the ONLY reader it speaks to is an old
        // binary that must refuse rather than re-seed.
        let bytes = REFUSAL_DOC.as_bytes().to_vec();
        let crc = crc64_nvme(&bytes);
        if let Err(e) = store
            .put_whole(
                &cfg.manifest_key(),
                bytes.into(),
                &PutCondition::IfMatch(h.etag.clone()),
                stamps,
                crc,
            )
            .await
        {
            eprintln!("flint-sync: could not poison the legacy manifest key after migration: {e}");
        }
    }
    Ok(meta)
}

/// Bump the generation without touching the entries.
///
/// This is what a takeover does, and under the single-object layout it
/// was a multi-MB GET + PUT per claim that also moved the ETag every
/// other syncer's no-change early exit compares against — so one claim
/// cost every follower a full fetch and parse of a document in which
/// nothing had changed. Here it is one small CAS: `seq` moves, so an
/// outstanding pointer handle goes stale exactly as before, and
/// `entries_seq` does NOT, so a follower can see that the entries are
/// the ones it already has.
pub async fn rotate_for_takeover(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    epoch: u64,
) -> LeanResult<Option<(LeanManifest, String)>> {
    for _ in 0..3 {
        // The legacy layout has no pointer to rotate: migrate by
        // rewriting the document once, then rotations are cheap forever.
        let Some(LoadedPointer { pointer: p, etag, .. }) = load_pointer(store, cfg).await? else {
            let Some(loaded) = load(store, cfg).await? else {
                return Ok(None);
            };
            let mut rotated = loaded.manifest.clone();
            rotated.seq += 1;
            let h = loaded.handle();
            match cas_write(store, cfg, &rotated, Some(&h), epoch, "takeover-rotation").await {
                Ok(meta) => return Ok(Some((rotated, meta.etag))),
                Err(super::LeanError::Store(StoreError::PreconditionFailed(_))) => continue,
                Err(e) => return Err(e),
            }
        };
        let next = Pointer { seq: p.seq + 1, epoch, ..p.clone() };
        let stamps = GenerationStamps {
            generation: next.seq,
            epoch,
            flush_uuid: "takeover-rotation".to_string(),
            boundary_source: next.boundary_source.clone(),
            posix: None,
        };
        let h = ManifestHandle { etag, legacy: false };
        match put_pointer(store, cfg, &next, Some(&h), &stamps).await {
            Ok(meta) => {
                // The document the caller gets back is the standing one
                // with the rotated seq — the entries were not read and
                // did not need to be.
                let mut m = LeanManifest::default();
                m.seq = next.seq;
                m.pinned_reads = next.pinned_reads;
                m.boundary_source = next.boundary_source.clone();
                return Ok(Some((m, meta.etag)));
            }
            Err(super::LeanError::Store(StoreError::PreconditionFailed(_))) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(super::LeanError::State(
        "takeover rotation lost 3 CAS races — a live writer is still publishing".into(),
    ))
}

/// How many superseded generations to keep behind the live one.
///
/// Not zero, and not one. A reader GETs the pointer and then GETs the
/// object it names, and those are two requests with a gap between them:
/// reaping the generation a reader has just resolved would turn a
/// perfectly ordinary read into a `NotFound`. A handful is a few hundred
/// KiB at any realistic entry count and buys that gap several publishes.
pub const KEEP_GENERATIONS: usize = 5;

/// How long a generation NEWER than the live one is left alone before it
/// is treated as an orphan.
///
/// An object above the live generation is one of two things: a publish
/// that is in flight right now — its entries written, its pointer CAS
/// not yet landed — or the wreckage of one that died in that window. The
/// two are indistinguishable from a listing, and deleting the first
/// breaks a live publish, so age is the only honest discriminator. Well
/// past any plausible manifest PUT, including a 264 MiB one over a slow
/// link.
pub const ORPHAN_GRACE_SECS: u64 = 3600;

/// Delete superseded generation objects, and orphans left by a publish
/// that died between its entries PUT and its pointer CAS.
///
/// Immutable metadata that is never collected is a leak, and this one
/// grows by a whole manifest per publish.
///
/// The two cases are NOT symmetric, which the first cut of this got
/// wrong. BELOW the live generation, everything is superseded by
/// definition and the only question is how much slack to leave a reader
/// mid-resolve — that is `KEEP_GENERATIONS`. ABOVE it, an object may be
/// a publish still in flight, and a keep-window is exactly the wrong
/// tool: an orphan at a high seq sorts to the front and the window
/// protects it forever while costing a real generation its slot. Age
/// decides that case instead.
///
/// Best effort and off the critical path: a failure here costs storage,
/// never correctness, so it warns rather than failing a publish.
pub async fn sweep_generations(store: &dyn ObjectStore, cfg: &LeanConfig) -> LeanResult<usize> {
    let Some(lp) = load_pointer(store, cfg).await? else {
        return Ok(0);
    };
    let live = lp.pointer.entries_key;
    let prefix = format!("{}/{}/manifests/", cfg.prefix, super::LEAN_DIR);
    let mut listed = store.list(&prefix).await?;
    // The key is `<seq:020>-<uuid>`, so lexical order IS generation
    // order and a plain sort is enough — the zero padding is doing this
    // job, which is why it is there.
    listed.sort_by(|a, b| a.key.cmp(&b.key));
    let now = super::now_unix();
    let live_ix = listed.iter().position(|o| o.key == live);

    let mut doomed: Vec<String> = Vec::new();
    for (i, o) in listed.iter().enumerate() {
        if o.key == live {
            continue;
        }
        match live_ix {
            // Newer than the live pointer: in flight, or wreckage.
            Some(ix) if i > ix => {
                let age = o.last_modified_unix.map(|t| now.saturating_sub(t));
                // No timestamp ⇒ leave it. A store that cannot date its
                // objects gets a leak, not a deleted live publish.
                if age.is_some_and(|a| a > ORPHAN_GRACE_SECS) {
                    doomed.push(o.key.clone());
                }
            }
            // Older than the live pointer: superseded. Keep a window.
            Some(ix) => {
                if ix.saturating_sub(i) > KEEP_GENERATIONS {
                    doomed.push(o.key.clone());
                }
            }
            // The pointer names an object that is not in the listing.
            // `load` already refuses that workspace; sweep nothing.
            None => return Ok(0),
        }
    }
    let mut removed = 0;
    for k in doomed {
        match store.delete(&k).await {
            Ok(()) => removed += 1,
            Err(e) => eprintln!("flint-sync: could not reap superseded generation {k}: {e}"),
        }
    }
    Ok(removed)
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


