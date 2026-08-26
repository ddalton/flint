//! The inbox/window cell (plan §2.2): ONE CAS document that is both the
//! HITL inbox and the barrier-window token.
//!
//! - The gateway appends an entry per UI write (object first, then the
//!   inbox CAS) — never a direct manifest edit.
//! - The sidecar CAS-marks the window open (with a deadline + its
//!   epoch) at barrier intent time and clears it after the manifest
//!   CAS; every gateway replica checks the cell before admitting a UI
//!   write, which closes the stateless-two-replica race the review
//!   proved.
//! - A dead sidecar cannot wedge HITL forever: the window carries a
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Window {
    pub epoch: u64,
    pub deadline_unix: u64,
}

/// A verb asked for through the gateway door (§2.5, D14). Idempotent
/// STATE, not a queue: repeated sets before the sidecar acts collapse
/// to the newest, which is why neither field needs a rate limit, an
/// exactly-once protocol, or a clearing CAS of its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerbRequest {
    pub requested_unix: u64,
    pub requestor: String,
}

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
/// deadline does not block (the dead-sidecar unwedge).
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

/// The SIDECAR side: open the barrier window (the intent). Succeeds
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
        for q in queued {
            if !doc.entries.iter().any(|e| e.path == q.path && e.etag == q.etag) {
                doc.entries.push(q.clone());
            }
        }
        match cas_write(store, cfg, &doc, loaded.etag.as_deref(), epoch).await {
            Ok(_) => return Ok(()),
            Err(LeanError::Store(StoreError::PreconditionFailed(_))) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(LeanError::State("window clear lost 5 CAS races".into()))
}
