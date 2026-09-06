//! Deleting packs the snapshot no longer names, under
//! `LeanChunkGC`'s four rules with "chunk" read as "pack" (design
//! §10). The module the rules come from is machine-checked; what
//! follows is the instantiation, and each rule is load-bearing in a way
//! a mutation of that model rediscovers when it is dropped.
//!
//! 1. **List candidates FIRST, then read the reference set, and abort
//!    if it moved.** A sweep that listed after reading the snapshot
//!    could see a pack the newer snapshot names and the older one did
//!    not, and delete a live pack.
//! 2. **HEAD each candidate AT the delete** and require an age past
//!    the grace. A pack that appeared between the listing and the
//!    delete is younger than the grace and is left alone.
//! 3. **The grace must outlive the longest upload**, not the longest
//!    plausible one — which is why it is configuration and not a
//!    constant.
//! 4. **A re-upload refreshes the age**, which is why the upload path
//!    re-PUTs a pack it already believes is there instead of skipping
//!    it: skipping would leave a live pack looking like an orphan
//!    forever.
//!
//! The reference predicate is "named by the snapshot whose etag this
//! sweep read", never "contains the same objects". Pack names are
//! content-derived but differ across clients' delta settings, so two
//! packs holding the same objects have different names and neither is
//! evidence about the other.
//!
//! It sweeps clone bundles too, from the same reference set and under
//! the same rules.

use super::{restore, snapshot, ForgeError, ForgeResult, Syncer};

/// Abort every multipart upload still pending under the repository.
///
/// Called after the claim and between batches — the two moments this
/// process can have nothing of its own in flight, which is why there
/// is no grace: anything pending is a predecessor's (a crash left it;
/// the scale drill measured 384 MiB of parts per interrupted 2 GiB
/// push, billed until aborted, and forge had no sweep at all) or a
/// deposed straggler's, whose Complete now fails `NoSuchUpload` and
/// whose CAS would have 412'd regardless. The tier's claim-time sweep
/// is the shape (`tier::epoch::takeover_sweep`).
pub async fn abort_orphaned_uploads(sc: &Syncer) -> ForgeResult<usize> {
    sc.check_fence()?;
    // "Nothing of ours is in flight" is false while a fold uploads on
    // its task; aborting then would end the holder's own base rebuild.
    if sc.fold.is_some() {
        return Err(ForgeError::State("a fold is in flight; the upload sweep waits".into()));
    }
    let prefix = format!("{}/", sc.cfg.git_prefix());
    let pending = sc.store.list_uploads(&prefix).await?;
    for u in &pending {
        sc.store.abort_upload(&u.key, &u.upload_id).await?;
        eprintln!(
            "flint-forge: aborted a multipart upload left in flight on {} ({})",
            u.key, u.upload_id
        );
    }
    Ok(pending.len())
}

pub async fn sweep(sc: &mut Syncer) -> ForgeResult<usize> {
    sc.check_fence()?;
    if sc.fold.is_some() {
        return Err(ForgeError::State("a fold is in flight; the sweep waits".into()));
    }
    // In-flight uploads first: between batches nothing of ours is in
    // flight, so every one of them is an orphan.
    abort_orphaned_uploads(sc).await?;
    // Rule 1: candidates first…
    let listed = restore::list_pack_files(sc).await?;

    // …then the reference set, read after the listing. If it moved, a
    // repack or another writer changed the pack list under us and the
    // listing is no longer safe to judge against.
    let fresh = snapshot::load(sc.store.as_ref(), &sc.cfg).await?;
    let mine = sc.cell()?.etag.clone();
    if fresh.etag != mine {
        eprintln!(
            "flint-forge: snapshot moved during the sweep (seq {} -> {}); leaving every candidate \
             alone until the next pass",
            sc.cell()?.snap.seq,
            fresh.snap.seq
        );
        return Ok(0);
    }

    let stems: Vec<String> = fresh
        .snap
        .packs
        .iter()
        .map(|p| p.trim_end_matches(".pack").to_string())
        .collect();
    let now = super::now_unix();
    let grace = sc.cfg.orphan_grace_secs;
    let mut deleted = 0usize;

    // Bundles are swept by the same four rules and from the same
    // reference set. The grace matters more here than for a pack: a
    // client may be holding a presigned URL for one it was advertised
    // a moment ago, and deleting it under that client turns a clone
    // into a failed fetch and a fallback to the server — which is
    // correct but is exactly the load the bundle existed to avoid.
    let mut candidates = listed.clone();
    for obj in sc.store.list(&sc.cfg.bundle_prefix()).await? {
        if let Some(name) = obj.key.rsplit('/').next() {
            candidates.insert(
                name.to_string(),
                super::restore::PackObject {
                    key: obj.key.clone(),
                    size: obj.size,
                    etag: obj.etag.clone(),
                    last_modified_unix: obj.last_modified_unix,
                },
            );
        }
    }
    let live_bundles = fresh.snap.bundles.clone();

    for (name, obj) in candidates.iter() {
        if stems.iter().any(|s| name.starts_with(s.as_str())) {
            continue;
        }
        if live_bundles.iter().any(|b| b == name) {
            continue;
        }
        // A prefilter on the LISTED age: an object can be made younger
        // after the listing (a re-upload) but never older, so a listed
        // age under the grace cannot pass rule 2 at the delete, and the
        // HEAD is saved. Under tiers every push leaves two orphans and
        // the serial HEADs would hold the loop for minutes.
        if let Some(t) = obj.last_modified_unix {
            if now.saturating_sub(t) < grace {
                continue;
            }
        }
        // Rule 2: the age is read at the delete, from the store's own
        // clock — never from ours, and never from the listing, which
        // is by then as old as the sweep.
        let meta = match sc.store.head(&obj.key).await {
            Ok(m) => m,
            // Gone already: another pass, or a retry that lost its
            // response. Nothing to do and nothing to report.
            Err(flint_store::StoreError::NotFound(_)) => continue,
            Err(e) => return Err(e.into()),
        };
        let age_ok = match meta.last_modified_unix {
            Some(t) => now.saturating_sub(t) >= grace,
            // A store that will not say how old an object is cannot
            // authorise a delete. Fail closed: leaving an orphan costs
            // storage, deleting a live pack costs the repository.
            None => false,
        };
        if !age_ok {
            continue;
        }
        sc.store.delete(&obj.key).await?;
        deleted += 1;
    }
    if deleted > 0 {
        eprintln!("flint-forge: swept {deleted} object(s) past the {grace}s grace");
    }
    Ok(deleted)
}
