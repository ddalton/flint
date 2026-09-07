//! The snapshot: the one mutable object the server trusts (design §3).
//!
//! Packs are immutable and content-named by git, so nothing in the
//! bucket needs a conditional write except this. Refs live here rather
//! than in a `packed-refs` object, which is what makes a batch that
//! moves thirty refs one CAS and makes a concurrent reader's view
//! whole: a restore never sees half a batch.
//!
//! The etag of the object this syncer last read or wrote is the token
//! for its next CAS. Under the writer lock a 412 therefore cannot mean
//! "someone else's push"; it can only mean a second server holds this
//! repository, which is a fence rather than a retry.

use std::collections::BTreeMap;

use bytes::Bytes;
use flint_store::{
    crc64_nvme, GenerationStamps, ObjectStore, PutCondition, StoreError,
};

use super::{ForgeConfig, ForgeError, ForgeResult};

/// Bumped only for a change an older syncer could MISREAD. A reader
/// that meets a higher version refuses rather than parsing what it can
/// and concluding the repository is empty — the fail-closed rule every
/// flint layout change follows.
pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub version: u32,
    /// Monotonic per write. Its only job is to make a rotation change
    /// the object's bytes (and therefore its etag) when the content
    /// would otherwise be identical.
    pub seq: u64,
    /// The lease epoch under which this snapshot was written.
    pub epoch: u64,
    /// Full ref name -> object id. `refs/for/*` is never stored: it is
    /// a request, not a ref.
    pub refs: BTreeMap<String, String>,
    /// Pack file names (`pack-<sha>.pack`), without a path. The
    /// `.idx`, `.bitmap` and `.rev` beside each are implied and are
    /// uploaded with it.
    pub packs: Vec<String>,
    /// Clone bundles offered as bundle URIs (§8), newest last.
    #[serde(default)]
    pub bundles: Vec<String>,
    /// The commit whose tree the legible export last published (§9).
    #[serde(default)]
    pub exported_commit: Option<String>,
    /// Which syncer wrote this, and when by its own clock. Diagnostic
    /// only — no decision reads them, because a deposed writer's clock
    /// is exactly the one that cannot be trusted.
    pub writer: String,
    pub unix: u64,
}

impl Snapshot {
    pub fn empty() -> Self {
        Snapshot {
            version: SNAPSHOT_VERSION,
            seq: 0,
            epoch: 0,
            refs: BTreeMap::new(),
            packs: Vec::new(),
            bundles: Vec::new(),
            exported_commit: None,
            writer: String::new(),
            unix: 0,
        }
    }

    /// What the bucket says this ref is. The batch's staleness check
    /// reads this and the local ref, and requires both.
    pub fn oid(&self, name: &str) -> Option<&str> {
        self.refs.get(name).map(|s| s.as_str())
    }
}

/// A snapshot together with the etag that entitles its holder to
/// replace it. `etag: None` means the object did not exist when it was
/// read, so the next write is `If-None-Match: *` — which closes the
/// create race with a second server exactly as `If-Match` closes the
/// update race.
#[derive(Debug, Clone)]
pub struct Cell {
    pub snap: Snapshot,
    pub etag: Option<String>,
}

impl Cell {
    pub fn condition(&self) -> PutCondition {
        match &self.etag {
            Some(e) => PutCondition::IfMatch(e.clone()),
            None => PutCondition::IfNoneMatchAny,
        }
    }
}

/// Read the snapshot. An absent object is a fresh repository, not an
/// error: the first push creates it under `If-None-Match: *`.
pub async fn load(store: &dyn ObjectStore, cfg: &ForgeConfig) -> ForgeResult<Cell> {
    match store.get_whole(&cfg.snapshot_key(), None).await {
        Ok((meta, body)) => {
            let snap: Snapshot = serde_json::from_slice(&body).map_err(|e| {
                // Unparseable is REFUSED, never "empty". Serving a
                // repository whose pointer we cannot read would
                // re-seed the bucket from a local cache that may be
                // empty, which is the one outcome no operator can undo.
                ForgeError::Refused(format!(
                    "snapshot {} is unparseable ({e}) — refusing to serve or overwrite it",
                    cfg.snapshot_key()
                ))
            })?;
            if snap.version > SNAPSHOT_VERSION {
                return Err(ForgeError::Refused(format!(
                    "snapshot {} is version {} and this syncer speaks {SNAPSHOT_VERSION} — \
                     refusing to serve a layout it may misread",
                    cfg.snapshot_key(),
                    snap.version
                )));
            }
            Ok(Cell { snap, etag: Some(meta.etag) })
        }
        Err(StoreError::NotFound(_)) => Ok(Cell { snap: Snapshot::empty(), etag: None }),
        Err(e) => Err(e.into()),
    }
}

/// Replace the snapshot, guarded on the etag this holder last saw.
///
/// The caller passes the NEXT snapshot; `seq`, `epoch`, `writer` and
/// `unix` are stamped here so no call site can forget the bump that
/// makes a rotation's bytes differ.
pub async fn cas(
    store: &dyn ObjectStore,
    cfg: &ForgeConfig,
    cell: &Cell,
    mut next: Snapshot,
    epoch: u64,
    writer: &str,
) -> ForgeResult<Cell> {
    next.version = SNAPSHOT_VERSION;
    next.seq = cell.snap.seq + 1;
    next.epoch = epoch;
    next.writer = writer.to_string();
    next.unix = super::now_unix();
    let body = serde_json::to_vec(&next)
        .map_err(|e| ForgeError::State(format!("snapshot will not serialise: {e}")))?;
    let crc = crc64_nvme(&body);
    let stamps = GenerationStamps {
        generation: next.seq,
        epoch,
        flush_uuid: uuid::Uuid::new_v4().to_string(),
        boundary_source: None,
        posix: None,
    };
    let meta = store
        .put_whole(&cfg.snapshot_key(), Bytes::from(body), &cell.condition(), &stamps, crc)
        .await?;
    Ok(Cell { snap: next, etag: Some(meta.etag) })
}

/// The takeover rotation (design §5; lean's `rotate_for_takeover`, and
/// the mutation `LeanNoRotate` is why it exists).
///
/// A successor that restored and began serving without this leaves a
/// straggler holding an `If-Match` that is still valid: its next batch
/// would land, and the bucket would then hold refs the successor never
/// acknowledged and never restored. Rotating first — same content, new
/// seq, one small CAS — makes the straggler's token stale before the
/// successor serves a byte, and its next batch 412s into the fence.
///
/// It is needed ONLY for the unreleased-foreign takeover. A released
/// cell is a clean handoff and self-recognition means our own previous
/// container died with its writes, so rotating there is pure churn.
///
/// A repository nobody has published yet is rotated too, by CREATING
/// its empty snapshot under `If-None-Match: *`. The first shape
/// returned early here ("the first batch's If-None-Match is the fence"),
/// and `formal/ForgeSync.tla`'s first strict run found what that fence
/// actually fences: a straggler mid-batch on the old epoch lands its
/// create after the successor has restored and is serving, and it is
/// then the SUCCESSOR's first CAS that 412s — fenced by its own
/// predecessor's push. No loss (the restart restores that push), but
/// the rotation exists precisely so a straggler cannot fence its
/// successor, and a create is the rotation of an absent snapshot.
pub async fn rotate_for_takeover(
    store: &dyn ObjectStore,
    cfg: &ForgeConfig,
    epoch: u64,
    writer: &str,
) -> ForgeResult<Cell> {
    let cell = load(store, cfg).await?;
    let next = cell.snap.clone();
    match cas(store, cfg, &cell, next, epoch, writer).await {
        Ok(c) => {
            // A rotation moves the seq without moving anything else. A
            // follower needs the entry all the same: without it the
            // chain has a hole at every handover, and every handover is
            // exactly when a follower's warm repository is about to be
            // worth the most.
            super::log::record_rotation(store, cfg, &cell.snap, &c.snap).await;
            Ok(c)
        }
        // Losing the rotation race means another successor rotated
        // first and is now the holder. We are not entitled to serve.
        Err(ForgeError::Store(StoreError::PreconditionFailed(e))) => Err(ForgeError::Fenced(
            format!("lost the takeover rotation to another server: {e}"),
        )),
        Err(e) => Err(e),
    }
}
