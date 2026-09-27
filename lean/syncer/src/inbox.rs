//! The request cell: ONE CAS document per workspace carrying the two verb
//! requests a party outside the pod can make (plan §2.5) — "please
//! publish" and "please pull".
//!
//! It was also the HITL inbox and the barrier-window token. Under P2
//! (simplification step 5, 2026-09-25) a UI write COMMITS — the gateway
//! CASes the manifest itself (`manifest::commit_edit`) — so there are no
//! entries, no declared removals and no window any more.

use serde::{Deserialize, Serialize};

use flint_store::{
    crc64_nvme, GenerationStamps, ObjectStore, PutCondition, StoreError,
};

use super::{now_unix, LeanConfig, LeanError, LeanResult};

/// A verb asked for through the gateway door (§2.5, D14). Idempotent
/// STATE, not a queue: repeated sets before the syncer acts collapse
/// to the newest, which is why neither field needs a rate limit, an
/// exactly-once protocol, or a clearing CAS of its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerbRequest {
    pub requested_unix: u64,
    pub requestor: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InboxDoc {
    /// "Please publish" from outside the pod (§2.5).
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

/// A CAS on the cell that lost to another writer: 412, or S3's 409
/// ConditionalRequestConflict — what S3 answers a conditional write that
/// races another on the same key, and this is the most contended key a
/// workspace has. Every loop below re-reads and retries both. Matching
/// 412 alone failed the gateway's append after a LANDED UI write, and the
/// earlier acked write that one replaced was dropped as superseded at the
/// next consume (review 2026-09-18, H3). The lease and the manifest CAS
/// always treated the pair alike.
fn lost_race(e: &LeanError) -> bool {
    matches!(e, LeanError::Store(StoreError::PreconditionFailed(_) | StoreError::Conflict(_)))
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

/// Which verb a gateway request is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedVerb {
    Boundary,
    Sync,
}

/// The GATEWAY side of §2.5: set one of the two request fields under
/// the same CAS discipline every other inbox write uses.
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
            Err(e) if lost_race(&e) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(LeanError::State("inbox verb request lost 5 CAS races".into()))
}
