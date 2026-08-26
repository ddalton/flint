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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InboxDoc {
    pub entries: Vec<InboxEntry>,
    pub window: Option<Window>,
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
