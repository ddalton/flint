//! Undo points (X15, `docs/plans/flint-forge-simplification-2026-09-05.md`).
//!
//! Forge keeps one live state: the snapshot is replaced in place,
//! versioning is off, the repository has no reflog that survives a
//! restart, and the base rebuild drops what nothing reaches. So a
//! force-push or a branch delete is unrecoverable at the storage layer
//! once the sweep takes the packs — the one leg the walgit comparison
//! lost outright (P11: walgit replayed its log to the pre-force tip;
//! forge had nothing to replay).
//!
//! The cheapest shape that closes it, and the one the note's X15 row
//! names: keep an immutable copy of the snapshot BEFORE the batch that
//! would make a state unreachable, and teach the sweep to treat the
//! packs those copies name as referenced. Three rules:
//!
//! 1. **Only a destructive batch writes one.** A fast-forward loses
//!    nothing — the old tip is an ancestor of the new one, so the
//!    objects stay reachable and the packs stay named. A delete or a
//!    non-fast-forward move is the only way a state leaves the live
//!    set, and only those pay the extra PUT. An ordinary push's
//!    fixed cost is unchanged.
//! 2. **The copy is written BEFORE the CAS.** A crash between the two
//!    leaves a copy of a state that is still live, which costs one
//!    small object; the other order loses the undo point exactly when
//!    the push that needed it landed.
//! 3. **The copy is a reference, not a backup.** It names packs; it
//!    does not hold objects. What makes it work is the sweep reading
//!    those names, and what bounds it is `undo_window_secs`: past the
//!    window the copy is deleted, and its packs become orphans like
//!    any other on the pass after.
//!
//! Recovery is deliberately NOT automatic here. The listing (this
//! module's `list`, the binary's `--undo-list`) tells an operator what
//! states exist and which refs they held; putting one back is a
//! decision with a person behind it.

use std::collections::BTreeSet;

use bytes::Bytes;
use flint_store::{crc64_nvme, GenerationStamps, ObjectStore, PutCondition, StoreError};

use super::{snapshot, ForgeConfig, ForgeError, ForgeResult};

/// One recoverable state: the snapshot as it was before a destructive
/// batch replaced it.
#[derive(Debug, Clone)]
pub struct UndoPoint {
    pub seq: u64,
    pub key: String,
    /// The store's clock on the copy, not ours.
    pub unix: Option<u64>,
    pub snap: snapshot::Snapshot,
}

/// Whether `updates` would make any state unreachable: a deletion, or a
/// move whose old tip is not an ancestor of the new one. The batch has
/// already run `is_ancestor` for its policy check; this repeats it for
/// the accepted set, which is small and already in the page cache.
pub async fn is_destructive(
    git: &super::gitcmd::Git,
    updates: &[super::gitcmd::RefUpdate],
) -> ForgeResult<bool> {
    for u in updates {
        // A create has no old state — the batch spells it as the zero
        // oid or as empty, and neither is a commit `is_ancestor` can
        // resolve.
        let had_state = !u.old_oid.is_empty() && !super::gitcmd::is_zero(&u.old_oid);
        if super::gitcmd::is_zero(&u.new_oid) {
            if had_state {
                return Ok(true); // a delete
            }
            continue;
        }
        if !had_state {
            continue; // a create
        }
        if !git.is_ancestor(&u.old_oid, &u.new_oid).await? {
            return Ok(true); // a rewind or a rewrite
        }
    }
    Ok(false)
}

/// Write the copy of `cell`'s snapshot, keyed by its seq. Unconditional
/// and idempotent: that seq is written once by construction (the CAS
/// that follows makes the next one), so a retry rewrites identical
/// bytes.
pub async fn write_point(
    store: &dyn ObjectStore,
    cfg: &ForgeConfig,
    snap: &snapshot::Snapshot,
) -> ForgeResult<String> {
    let key = cfg.undo_key(snap.seq);
    let body = serde_json::to_vec(snap)
        .map_err(|e| ForgeError::State(format!("undo point will not serialise: {e}")))?;
    let crc = crc64_nvme(&body);
    let stamps = GenerationStamps {
        generation: snap.seq,
        epoch: snap.epoch,
        flush_uuid: uuid::Uuid::new_v4().to_string(),
        boundary_source: None,
        posix: None,
    };
    store
        .put_whole(&key, Bytes::from(body), &PutCondition::Unconditional, &stamps, crc)
        .await?;
    Ok(key)
}

/// Every undo point in the bucket, newest first. One LIST plus one
/// small GET each; the caller bounds how many it reads.
pub async fn list(
    store: &dyn ObjectStore,
    cfg: &ForgeConfig,
    limit: usize,
) -> ForgeResult<Vec<UndoPoint>> {
    let mut listed = store.list(&cfg.undo_prefix()).await?;
    // The key's tail is the seq; sort numerically, not lexically, or
    // seq 9 outranks seq 10.
    listed.sort_by_key(|o| std::cmp::Reverse(seq_of(&o.key).unwrap_or(0)));
    let mut out = Vec::new();
    for obj in listed.into_iter().take(limit) {
        let Some(seq) = seq_of(&obj.key) else { continue };
        let (_, bytes) = match store.get_whole(&obj.key, None).await {
            Ok(v) => v,
            Err(StoreError::NotFound(_)) => continue,
            Err(e) => return Err(e.into()),
        };
        let snap: snapshot::Snapshot = match serde_json::from_slice(&bytes) {
            Ok(s) => s,
            // A copy this syncer cannot parse is not a reason to stop
            // serving; it is also not evidence its packs are free, so
            // the caller keeps the pack names it already has.
            Err(e) => {
                eprintln!("flint-forge: undo point {} unreadable: {e}", obj.key);
                continue;
            }
        };
        out.push(UndoPoint { seq, key: obj.key, unix: obj.last_modified_unix, snap });
    }
    Ok(out)
}

fn seq_of(key: &str) -> Option<u64> {
    key.rsplit('/').next()?.trim_end_matches(".json").parse().ok()
}

/// The pack stems the undo points protect, and the copies that are past
/// the window (which the caller deletes). A copy the store will not
/// date is kept: an object of unknown age cannot authorise a delete,
/// the same rule the pack sweep applies.
pub struct Referenced {
    pub stems: BTreeSet<String>,
    pub expired: Vec<String>,
}

pub async fn referenced(
    store: &dyn ObjectStore,
    cfg: &ForgeConfig,
    now: u64,
) -> ForgeResult<Referenced> {
    let mut stems = BTreeSet::new();
    let mut expired = Vec::new();
    if cfg.undo_window_secs == 0 {
        // Undo off: the copies are not written, and any left by a
        // previous configuration are ordinary orphans.
        return Ok(Referenced { stems, expired });
    }
    for p in list(store, cfg, cfg.undo_max_points).await? {
        let old = p.unix.map(|t| now.saturating_sub(t) >= cfg.undo_window_secs).unwrap_or(false);
        if old {
            expired.push(p.key);
            continue;
        }
        for pack in &p.snap.packs {
            stems.insert(pack.trim_end_matches(".pack").to_string());
        }
    }
    Ok(Referenced { stems, expired })
}
