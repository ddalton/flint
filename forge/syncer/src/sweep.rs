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

use super::{restore, snapshot, ForgeResult, Syncer};

pub async fn sweep(sc: &mut Syncer) -> ForgeResult<usize> {
    sc.check_fence()?;
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

    for (name, key) in listed.iter() {
        if stems.iter().any(|s| name.starts_with(s.as_str())) {
            continue;
        }
        // Rule 2: the age is read at the delete, from the store's own
        // clock — never from ours, and never from the listing, which
        // is by then as old as the sweep.
        let meta = match sc.store.head(key).await {
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
        sc.store.delete(key).await?;
        deleted += 1;
    }
    if deleted > 0 {
        eprintln!("flint-forge: swept {deleted} pack file(s) past the {grace}s grace");
    }
    Ok(deleted)
}
