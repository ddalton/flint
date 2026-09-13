//! The inbox/window cell (plan §2.2): ONE CAS document that is both the
//! HITL inbox and the barrier-window token.
//!
//! - The gateway appends an entry per UI write (object first, then the
//!   inbox CAS) — never a direct manifest edit.
//! - The syncer CAS-marks the window open (with a deadline + its
//!   epoch) at barrier intent time and clears it after the manifest
//!   CAS; every gateway replica checks the cell before admitting a UI
//!   write, which closes the stateless-two-replica race the review
//!   proved.
//! - A dead syncer cannot wedge HITL forever: the window carries a
//!   deadline, and a successor epoch may override a stale window.

use serde::{Deserialize, Serialize};

use flint_store::{
    crc64_nvme, GenerationStamps, ObjectStore, PutCondition, StoreError,
};

use super::{now_unix, LeanConfig, LeanError, LeanResult};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboxEntry {
    pub path: String,
    /// The object ETag the write produced — what consume fetches
    /// If-Match (a superseded entry is dropped, not an error).
    pub etag: String,
    /// Who wrote it (user identity from the gateway; audit surface).
    pub author: String,
    pub added_unix: u64,
    /// CRC-64/NVME (wire form) of the bytes the write produced, computed
    /// by the writer over what it sent — the gateway over the request
    /// body, a merge-preserved entry from the manifest it came from.
    /// Consume verifies the fetched bytes against it before they are
    /// written, which is the only verification a backend that attests
    /// no checksum (Ozone) gets. `None` when the writer had no bytes in
    /// hand (a draft promote is a server-side copy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crc64_b64: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Window {
    pub epoch: u64,
    pub deadline_unix: u64,
}

/// A verb asked for through the gateway door (§2.5, D14). Idempotent
/// STATE, not a queue: repeated sets before the syncer acts collapse
/// to the newest, which is why neither field needs a rate limit, an
/// exactly-once protocol, or a clearing CAS of its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerbRequest {
    pub requested_unix: u64,
    pub requestor: String,
}

/// A DECLARED removal (delete/rename design §3): a delete asked for from
/// OUTSIDE the pod — a UI, a backend embedding the gateway crate. The
/// caller cannot touch the tree and must never delete the object
/// itself (§9: a cited object deleted from outside wedges every
/// checkout), so it records INTENT here and the syncer performs it at
/// its next barrier: unlink, then cite out, then GC — one manifest
/// generation, and for a rename the same generation that cites the
/// destination.
///
/// A FIELD of the cell and not a tombstone `InboxEntry`, for the reason
/// `boundary_request` gives above: `consume_inbox` HEADs every entry's
/// object and is not taught a second shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Removal {
    pub path: String,
    pub author: String,
    pub requested_unix: u64,
    /// For a rename: where the bytes went. Audit trail and conflict
    /// message; never load-bearing for correctness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<String>,
    /// Filled in by the syncer when it REFUSED to perform this removal
    /// — the path had unpublished local edits, was not a file, or
    /// could not be contained. A refused removal is never retried: it
    /// stays here, so the human who asked can read why from any
    /// replica, until a newer removal of the path supersedes it or the
    /// caller withdraws it. Bounded by `REFUSED_REMOVALS_CAP`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<Refusal>,
}

/// Why the syncer did not perform a removal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Refusal {
    /// `removal-refused-dirty` | `removal-refused-not-a-file` |
    /// `removal-refused-containment`.
    pub kind: String,
    pub message: String,
    pub at_unix: u64,
}

/// Refused removals kept per cell before the oldest is evicted.
pub const REFUSED_REMOVALS_CAP: usize = 100;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InboxDoc {
    pub entries: Vec<InboxEntry>,
    pub window: Option<Window>,
    /// "Please publish" from outside the pod (§2.5). Deliberately a
    /// FIELD and not a fake no-object `InboxEntry`: `consume_inbox`
    /// HEADs `file_key(path)` for every entry, so an entry naming no
    /// object lands in the NotFound arm as a spurious
    /// `consume-object-missing` conflict — and special-casing the
    /// single most safety-critical function in the crate to avoid that
    /// is worse than either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_request: Option<VerbRequest>,
    /// "Please pull" from outside the pod — CARRIED, never performed
    /// (D14). A boundary publishes what is already on disk and touches
    /// no local file; `sync` re-derives the tree against the current
    /// remote manifest and DELETES local files for remotely-deleted
    /// paths. Performing that on a remote's say-so would upgrade what a
    /// leaked gateway bearer can do from "publish, plus hand over these
    /// N named objects" to "rewrite and delete across a running agent's
    /// tree, at my timing, under a scope I choose".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_request: Option<VerbRequest>,
    /// DECLARED removals, pending or refused (see `Removal`). At most
    /// one per path: a newer removal of a path supersedes an older one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removals: Vec<Removal>,
}

impl InboxDoc {
    /// A removal of `path` that is recorded and not yet performed nor
    /// refused — what a listing built over the manifest and the inbox
    /// subtracts, so a deleted or renamed-away file leaves the UI the
    /// moment the removal is recorded rather than when the syncer gets
    /// to it.
    pub fn pending_removal(&self, path: &str) -> Option<&Removal> {
        self.removals.iter().find(|r| r.path == path && r.refused.is_none())
    }
}

pub struct LoadedInbox {
    pub doc: InboxDoc,
    /// None ⇒ the cell does not exist yet (first CAS is If-None-Match:*).
    pub etag: Option<String>,
}

pub async fn load(store: &dyn ObjectStore, cfg: &LeanConfig) -> LeanResult<LoadedInbox> {
    match store.get_whole(&cfg.inbox_key(), None).await {
        Ok((meta, bytes)) => {
            let doc = serde_json::from_slice(&bytes)
                .map_err(|e| LeanError::State(format!("inbox parse: {e}")))?;
            Ok(LoadedInbox { doc, etag: Some(meta.etag) })
        }
        Err(StoreError::NotFound(_)) => Ok(LoadedInbox { doc: InboxDoc::default(), etag: None }),
        Err(e) => Err(e.into()),
    }
}

pub async fn cas_write(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    doc: &InboxDoc,
    expected: Option<&str>,
    epoch: u64,
) -> LeanResult<String> {
    let bytes = serde_json::to_vec_pretty(doc)
        .map_err(|e| LeanError::State(format!("inbox: {e}")))?;
    let crc = crc64_nvme(&bytes);
    let cond = match expected {
        Some(etag) => PutCondition::IfMatch(etag.to_string()),
        None => PutCondition::IfNoneMatchAny,
    };
    let stamps = GenerationStamps {
        generation: 0,
        epoch,
        flush_uuid: "inbox".into(),
        boundary_source: None,
        posix: None,
    };
    let meta = store.put_whole(&cfg.inbox_key(), bytes.into(), &cond, &stamps, crc).await?;
    Ok(meta.etag)
}

/// Whether a gateway may admit a UI write right now. A window past its
/// deadline does not block (the dead-syncer unwedge).
pub fn admits_hitl(doc: &InboxDoc) -> bool {
    match &doc.window {
        None => true,
        Some(w) => now_unix() > w.deadline_unix,
    }
}

/// The GATEWAY side: land a UI write. The object PUT must already have
/// happened (object first, inbox second — a crash between leaves an
/// orphan object, never a tracked-but-absent entry). Refuses while a
/// live barrier window is open. CAS-retries the append.
pub async fn gateway_append(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    entry: InboxEntry,
) -> LeanResult<()> {
    for _ in 0..5 {
        let loaded = load(store, cfg).await?;
        if !admits_hitl(&loaded.doc) {
            return Err(LeanError::State(
                "barrier window open — retry after the window deadline".into(),
            ));
        }
        let mut doc = loaded.doc;
        // A newer write to the same path supersedes the queued one.
        doc.entries.retain(|e| e.path != entry.path);
        doc.entries.push(entry.clone());
        match cas_write(store, cfg, &doc, loaded.etag.as_deref(), 0).await {
            Ok(_) => return Ok(()),
            Err(LeanError::Store(StoreError::PreconditionFailed(_))) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(LeanError::State("inbox append lost 5 CAS races".into()))
}

/// One removal per path: a newer one supersedes an older one, pending
/// or refused — the older is that caller's own earlier intent, which
/// the newer replaces (the rule `gateway_append` applies to entries).
fn record_removal(doc: &mut InboxDoc, r: Removal) {
    doc.removals.retain(|x| x.path != r.path);
    doc.removals.push(r);
}

/// The GATEWAY side: record DECLARED removals (delete/rename design
/// §3), all in ONE CAS. The caller must have verified the paths exist
/// (cited or tracked) and must never delete the objects itself.
///
/// NOT window-gated, deliberately (§12): a write races the barrier that
/// is about to publish the tree, but a removal touches no object and no
/// path at the moment it is recorded — the same argument that left
/// `gateway_request` ungated. A removal recorded while a window is open
/// is simply performed by the NEXT barrier.
pub async fn gateway_remove(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    removals: Vec<Removal>,
) -> LeanResult<()> {
    for _ in 0..5 {
        let loaded = load(store, cfg).await?;
        let mut doc = loaded.doc;
        for r in &removals {
            record_removal(&mut doc, r.clone());
        }
        match cas_write(store, cfg, &doc, loaded.etag.as_deref(), 0).await {
            Ok(_) => return Ok(()),
            Err(LeanError::Store(StoreError::PreconditionFailed(_))) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(LeanError::State("inbox removal lost 5 CAS races".into()))
}

/// The GATEWAY side of a RENAME (§6): the destination entries and the
/// source removals land in ONE CAS, so the cell never holds half a
/// rename. The destination objects must already have been copied
/// (create first, removal second — §5). Window-gated like
/// `gateway_append`, because it carries entries.
pub async fn gateway_rename(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    entries: Vec<InboxEntry>,
    removals: Vec<Removal>,
) -> LeanResult<()> {
    for _ in 0..5 {
        let loaded = load(store, cfg).await?;
        if !admits_hitl(&loaded.doc) {
            return Err(LeanError::State(
                "barrier window open — retry after the window deadline".into(),
            ));
        }
        let mut doc = loaded.doc;
        for e in &entries {
            doc.entries.retain(|x| x.path != e.path);
            doc.entries.push(e.clone());
        }
        for r in &removals {
            record_removal(&mut doc, r.clone());
        }
        match cas_write(store, cfg, &doc, loaded.etag.as_deref(), 0).await {
            Ok(_) => return Ok(()),
            Err(LeanError::Store(StoreError::PreconditionFailed(_))) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(LeanError::State("inbox rename lost 5 CAS races".into()))
}

/// The GATEWAY side: take back a removal of `path`, pending or refused.
/// `Ok(false)` when there was none. Best effort against a barrier that
/// is already performing it: the cell says whether the record was
/// still there, the listing says what happened.
pub async fn withdraw_removal(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    path: &str,
) -> LeanResult<bool> {
    for _ in 0..5 {
        let loaded = load(store, cfg).await?;
        let mut doc = loaded.doc;
        let before = doc.removals.len();
        doc.removals.retain(|r| r.path != path);
        if doc.removals.len() == before {
            return Ok(false);
        }
        match cas_write(store, cfg, &doc, loaded.etag.as_deref(), 0).await {
            Ok(_) => return Ok(true),
            Err(LeanError::Store(StoreError::PreconditionFailed(_))) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(LeanError::State("inbox withdraw lost 5 CAS races".into()))
}

/// The SYNCER side: what became of the removals a pass looked at.
/// `applied` are dropped from the cell; `refused` are annotated in
/// place (their `refused` field is the answer), never retried, and
/// capped at `REFUSED_REMOVALS_CAP` with the oldest evicted. A removal
/// that was superseded meanwhile (same path, newer `requested_unix`)
/// is left alone — the newer intent gets its own pass.
pub async fn settle_removals(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    epoch: u64,
    applied: &[Removal],
    refused: &[Removal],
) -> LeanResult<()> {
    if applied.is_empty() && refused.is_empty() {
        return Ok(());
    }
    let same = |a: &Removal, b: &Removal| a.path == b.path && a.requested_unix == b.requested_unix;
    for _ in 0..5 {
        let loaded = load(store, cfg).await?;
        let mut doc = loaded.doc.clone();
        let mut changed = false;
        doc.removals.retain(|r| {
            let drop = applied.iter().any(|a| same(a, r));
            changed |= drop;
            !drop
        });
        for r in doc.removals.iter_mut() {
            if r.refused.is_none() {
                if let Some(f) = refused.iter().find(|f| same(f, r)) {
                    r.refused = f.refused.clone();
                    changed = true;
                }
            }
        }
        loop {
            let refused_now = doc.removals.iter().filter(|r| r.refused.is_some()).count();
            if refused_now <= REFUSED_REMOVALS_CAP {
                break;
            }
            let oldest = doc
                .removals
                .iter()
                .enumerate()
                .filter(|(_, r)| r.refused.is_some())
                .min_by_key(|(_, r)| r.refused.as_ref().map(|f| f.at_unix).unwrap_or(0))
                .map(|(i, _)| i);
            match oldest {
                Some(i) => {
                    doc.removals.remove(i);
                    changed = true;
                }
                None => break,
            }
        }
        if !changed {
            return Ok(());
        }
        match cas_write(store, cfg, &doc, loaded.etag.as_deref(), epoch).await {
            Ok(_) => return Ok(()),
            Err(LeanError::Store(StoreError::PreconditionFailed(_))) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(LeanError::State("inbox removal settle lost 5 CAS races".into()))
}

/// Which verb a gateway request is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedVerb {
    Boundary,
    Sync,
}

/// The GATEWAY side of §2.5: set one of the two request fields under
/// the same CAS discipline every other inbox write uses.
///
/// Deliberately NOT window-gated. `admits_hitl` exists because a HITL
/// object write races the barrier that is about to publish the tree; a
/// verb request touches no object and no path — refusing it during a
/// window would make "please publish" fail precisely while a publish is
/// in flight, which is the least useful moment to say no.
pub async fn gateway_request(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    verb: RequestedVerb,
    requestor: &str,
) -> LeanResult<VerbRequest> {
    let req = VerbRequest {
        requested_unix: now_unix(),
        requestor: requestor.chars().take(128).collect(),
    };
    for _ in 0..5 {
        let loaded = load(store, cfg).await?;
        let mut doc = loaded.doc;
        // Newest wins: the field is state, so a burst collapses instead
        // of queueing. This is what makes a rate limit unnecessary on
        // the transport (the HONOR is still min-interval'd and budgeted
        // like any other sentinel).
        match verb {
            RequestedVerb::Boundary => doc.boundary_request = Some(req.clone()),
            RequestedVerb::Sync => doc.sync_request = Some(req.clone()),
        }
        match cas_write(store, cfg, &doc, loaded.etag.as_deref(), 0).await {
            Ok(_) => return Ok(req),
            Err(LeanError::Store(StoreError::PreconditionFailed(_))) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(LeanError::State("inbox verb request lost 5 CAS races".into()))
}

/// The SYNCER side: open the barrier window (the intent). Succeeds
/// over a closed cell, an expired window, or a LOWER epoch's stale
/// window; refuses a live window at our own or a higher epoch.
pub async fn open_window(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    epoch: u64,
    deadline_unix: u64,
) -> LeanResult<LoadedInbox> {
    for _ in 0..5 {
        let loaded = load(store, cfg).await?;
        if let Some(w) = &loaded.doc.window {
            let expired = now_unix() > w.deadline_unix;
            if w.epoch > epoch {
                return Err(LeanError::Fenced(format!(
                    "window held by higher epoch {} (ours {})",
                    w.epoch, epoch
                )));
            }
            if w.epoch == epoch && !expired {
                // Our own live window (a crashed earlier attempt inside
                // the deadline): adopt it.
                return Ok(loaded);
            }
        }
        let mut doc = loaded.doc.clone();
        doc.window = Some(Window { epoch, deadline_unix });
        match cas_write(store, cfg, &doc, loaded.etag.as_deref(), epoch).await {
            Ok(etag) => {
                return Ok(LoadedInbox { doc, etag: Some(etag) });
            }
            Err(LeanError::Store(StoreError::PreconditionFailed(_))) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(LeanError::State("window open lost 5 CAS races".into()))
}

/// Drop integrated entries (after they are durably in the baseline).
/// Entries that arrived after the consume are preserved.
pub async fn drop_entries(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    epoch: u64,
    consumed: &[InboxEntry],
) -> LeanResult<()> {
    for _ in 0..5 {
        let loaded = load(store, cfg).await?;
        let mut doc = loaded.doc.clone();
        let before = doc.entries.len();
        doc.entries.retain(|e| !consumed.contains(e));
        if doc.entries.len() == before {
            return Ok(());
        }
        match cas_write(store, cfg, &doc, loaded.etag.as_deref(), epoch).await {
            Ok(_) => return Ok(()),
            Err(LeanError::Store(StoreError::PreconditionFailed(_))) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(LeanError::State("inbox drop lost 5 CAS races".into()))
}

/// Clear the window (after the manifest CAS) and, in the same CAS,
/// queue `queued` entries (the merge-preserved foreign entries handed
/// to the next consume). Entries that arrived mid-barrier are
/// preserved.
pub async fn clear_window(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    epoch: u64,
    queued: &[InboxEntry],
) -> LeanResult<()> {
    clear_window_settling(store, cfg, epoch, queued, &[], &[]).await
}

/// `clear_window`, and in the SAME CAS drop what this barrier
/// integrated: the entries it consumed and the DECLARED removals it
/// performed. Both stay in the cell until here — after the manifest
/// CAS — for two reasons that are one. A consumed entry's only other
/// record is the pod's emptyDir baseline, which a pod REPLACEMENT
/// takes with it; dropped at the window-open commitment, a HITL write
/// acked and consumed but not yet cited had nothing in the bucket
/// tracking it through the whole upload phase, and a successor's
/// checkout was blind to it. And a listing that subtracts pending
/// removals must keep hiding the path for exactly as long as the
/// manifest cites it, not reappear it for the length of the uploads.
/// A crash before this CAS leaves both in the cell; the next
/// incarnation re-consumes and re-declares, idempotently (the consume
/// skips an entry its baseline already holds, and a declared path
/// already absent re-declares as it stands).
pub async fn clear_window_settling(
    store: &dyn ObjectStore,
    cfg: &LeanConfig,
    epoch: u64,
    queued: &[InboxEntry],
    consumed: &[InboxEntry],
    applied_removals: &[Removal],
) -> LeanResult<()> {
    for _ in 0..5 {
        let loaded = load(store, cfg).await?;
        let mut doc = loaded.doc.clone();
        if let Some(w) = &doc.window {
            if w.epoch > epoch {
                return Err(LeanError::Fenced(format!(
                    "window rotated to higher epoch {} (ours {})",
                    w.epoch, epoch
                )));
            }
        }
        doc.window = None;
        doc.entries.retain(|e| !consumed.contains(e));
        for q in queued {
            if !doc.entries.iter().any(|e| e.path == q.path && e.etag == q.etag) {
                doc.entries.push(q.clone());
            }
        }
        doc.removals.retain(|r| {
            !applied_removals
                .iter()
                .any(|a| a.path == r.path && a.requested_unix == r.requested_unix)
        });
        match cas_write(store, cfg, &doc, loaded.etag.as_deref(), epoch).await {
            Ok(_) => return Ok(()),
            Err(LeanError::Store(StoreError::PreconditionFailed(_))) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(LeanError::State("window clear lost 5 CAS races".into()))
}
