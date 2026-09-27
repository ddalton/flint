//! The gateway's verbs as a LIBRARY: `Workspace` is one lean workspace
//! (one subtree prefix on one object store) and its methods are exactly
//! the verbs `flint-lean-gateway` serves over HTTP (`http.rs`), with
//! the transport taken off. A backend that already speaks to ten
//! workspaces through ten gateways holds ten `Workspace` values instead
//! — they share the store connection, hold no state of their own, and
//! answer the same questions with the same refusals.
//!
//! The HTTP layer is a thin skin over this module: every handler in
//! `http.rs` resolves the workspace id, calls one method here, and
//! maps `VerbError` to a status, an `error` code and the headers the
//! wire carries. That mapping (`VerbError::status`, `VerbError::code`)
//! lives here too, so an embedder that serves its own REST surface can
//! answer its frontend with the codes the gateway would have used and
//! nothing on the frontend has to change.
//!
//! What this module deliberately is NOT: a client of the gateway. It
//! talks to the BUCKET, with the same CAS cells the syncer uses, and it
//! needs the same credentials the gateway needed. Every guarantee the
//! gateway makes — the object first and its citation second, each UI
//! verb one manifest CAS at epoch 0 that never waits on the writers'
//! lease (P2), epoch-validated syncer verbs — is made here.

use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use flint_store::{crc64_nvme, crc64_to_b64, GenerationStamps, ObjectStore, PutCondition, ReadOnly, StoreError};

use flint_lean::inbox::{self, InboxDoc, RequestedVerb, VerbRequest};
use flint_lean::manifest::{self, LeanEntry, LeanManifest, LoadedManifest};

/// A UI save's fresh handle, ready to cite (`Workspace::commit_save`).
pub(crate) struct Save<'a> {
    pub path: &'a str,
    pub key: String,
    pub etag: String,
    pub crc64_b64: String,
    pub size: u64,
    pub mtime_unix: i64,
    pub flush: String,
}


use flint_lean::{now_unix, LeanConfig, LeanError, WHOLE_PUT_MAX};

/// One lean workspace: a subtree prefix on an object store, seen from
/// outside the pod. Cheap to build and to clone (an `Arc` and a
/// config); build one per workspace the embedder serves and keep them
/// for as long as the store connection lives.
#[derive(Clone)]
pub struct Workspace {
    store: Arc<dyn ObjectStore>,
    cfg: LeanConfig,
    max_put_bytes: u64,
    /// Built by `read_only`: every writer answers `VerbError::ReadOnly`,
    /// and `store` is wrapped in `flint_store::ReadOnly`.
    read_only: bool,
    /// The manifest this workspace last loaded (M8). A server shares one
    /// per workspace across requests (`with_manifest_cache`).
    manifests: Arc<ManifestCache>,
}

/// The manifest a gateway last loaded, keyed by the pointer etag it was
/// loaded at (M8 of the 2026-09-24 simplification analysis). A document is
/// immutable per pointer etag, so a read checks the pointer — a few hundred
/// bytes — and reuses the entries while it has not moved. Under P2 the
/// gateway is a publisher and every UI read went through a full load.
#[derive(Default)]
pub struct ManifestCache(std::sync::Mutex<Option<Arc<LoadedManifest>>>);

/// One `ManifestCache` per workspace prefix, for a server that builds a
/// `Workspace` per request.
#[derive(Default)]
pub struct ManifestCaches(std::sync::Mutex<std::collections::BTreeMap<String, Arc<ManifestCache>>>);

impl ManifestCaches {
    pub fn for_prefix(&self, prefix: &str) -> Arc<ManifestCache> {
        self.0.lock().unwrap().entry(prefix.to_string()).or_default().clone()
    }
}

/// What a HITL write brings besides its bytes. The preconditions are
/// HTTP-shaped on purpose — a backend forwarding a browser's `If-Match`
/// hands the header value over verbatim and gets the gateway's exact
/// judgement of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PutFile {
    /// Accepted and not recorded: no UI verb and no mount write records
    /// an author, so the two flows stay uniform.
    pub author: Option<String>,
    /// `If-Match`: the entity-tag the caller read, or `*`. An overwrite
    /// MUST carry one — see `VerbError::PreconditionRequired`.
    pub if_match: Option<String>,
    /// `If-None-Match`: only `*` is honoured (create if absent).
    pub if_none_match: Option<String>,
}

/// A file read: the bytes and the entity-tag they carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub etag: String,
    pub body: Bytes,
}

/// The sync verb's one-stop read: the cited manifest, and the cell's
/// standing boundary and sync requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub manifest: LeanManifest,
    /// `None` ⇒ nothing has been published yet.
    pub manifest_etag: Option<String>,
    pub inbox: InboxDoc,
}

/// One row of a file listing: what a browser shows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Listed {
    pub path: String,
    pub etag: String,
    /// Always known: the manifest cites every listed path.
    pub size: Option<u64>,
    /// Always true since P2 (a UI verb commits); kept for the wire.
    pub cited: bool,
}

impl Snapshot {
    /// The listing a file browser shows: the manifest's citations. A UI
    /// save, delete or rename COMMITS (P2), so the document is the whole
    /// story the moment the verb returns.
    pub fn listing(&self) -> Vec<Listed> {
        self.manifest
            .entries
            .iter()
            .map(|(p, e)| Listed { path: p.clone(), etag: e.etag.clone(), size: Some(e.size), cited: true })
            .collect()
    }
}

/// The RPO observability surface: seq, the epoch cell, and the
/// standing verb requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub seq: Option<u64>,
    /// The publish fence: its epoch advances once per barrier, and
    /// `holder_id`/`holder_released` name the LAST barrier's writer and
    /// whether its commit section is over (the cell is at rest between
    /// barriers, so `released: true` is the normal idle reading, not an
    /// absent syncer). Nothing in the bucket tells a live idle syncer
    /// from a dead one, and nothing here needs to: a write is durable
    /// and tracked either way, and `last_cited_seq` moving is what says
    /// a syncer is citing.
    pub epoch: Option<u64>,
    pub holder_id: Option<String>,
    pub holder_released: Option<bool>,
    pub now_unix: u64,
    /// The last manifest seq a boundary installed.
    pub last_cited_seq: Option<u64>,
    pub manifest_stamp_unix: Option<u64>,
    /// Which clock installed it: `sentinel`, `sentinel-deferred`,
    /// `cadence` or `drain`. A reader that cares whether the view it is
    /// about to take was DECLARED coherent by the agent or taken by the
    /// floor can tell, from the bucket.
    pub boundary_source: Option<String>,
    /// Whether a boundary/sync request is standing (§2.5).
    pub boundary_request: Option<VerbRequest>,
    pub sync_request: Option<VerbRequest>,
}

/// A recorded verb request. `status` is always `recorded`, never
/// `done`: the syncer decides when.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Accepted {
    pub status: String,
    pub verb: String,
    pub requested_unix: u64,
    pub requestor: String,
    pub note: String,
}

/// Why a verb refused. `Display` is the message the gateway puts on the
/// wire; `status()` and `code()` are the status and the `error` field
/// it answers with, so an embedder can keep the contract its frontend
/// already speaks.
#[derive(Debug, thiserror::Error)]
pub enum VerbError {
    /// 400 `bad-path`: traversal, absolute, reserved namespace, or an
    /// empty segment. Carries the path.
    #[error("{0}")]
    BadPath(String),
    /// 400 `bad-user`: a draft user id that is not one clean key
    /// segment. Carries the user.
    #[error("{0}")]
    BadUser(String),
    /// 400 `bad-precondition`: a precondition shape the verb refuses
    /// to guess at.
    #[error("{0}")]
    BadPrecondition(&'static str),
    /// 428 `precondition-required`: the file exists and the write
    /// carried no `If-Match`. The gateway's own fresh HEAD closes only
    /// its HEAD-to-PUT window and gives the CALLER nothing: two
    /// browsers that each read v1 and then write would both succeed,
    /// and the second would silently win.
    #[error("the file already exists; send If-Match with the version you read")]
    PreconditionRequired,
    /// 412 `file-changed`: the caller's `If-Match` did not hold.
    /// `current` is the entity-tag it should have sent (`None` when
    /// the file is gone), which the wire also puts on the `etag`
    /// header.
    #[error("the file changed since you read it; re-read it and try again")]
    FileChanged { current: Option<String> },
    /// 409 `concurrent-write`: the precondition held when judged and
    /// the object moved inside the HEAD-to-PUT window. Retrying the
    /// same request can succeed, which is the opposite of the advice a
    /// 412 carries.
    #[error("the object changed under this write; re-read and retry")]
    ConcurrentWrite,
    /// 409 `moved`: the object moved past the tracked version; retry.
    #[error("the object moved past the tracked version; retry")]
    Moved,
    /// 404 `no-such-file`. Carries the path.
    #[error("{0}")]
    NoSuchFile(String),
    /// 410 `foreign-write`: a sole-writer workspace whose object no
    /// longer carries the cited etag — something other than its
    /// publisher wrote it.
    #[error("the manifest cites {path} at an etag the object no longer carries, and this workspace is published by a sole writer — something other than its publisher wrote that object")]
    ForeignWrite { path: String },
    /// 413 `payload-too-large`: the body is over the whole-object cap.
    /// The HTTP gateway refuses this at the body filter; the library
    /// refuses it here so an embedder cannot exceed the cap by
    /// forgetting to check.
    #[error("body is {size} bytes; the whole-object cap is {max}")]
    TooLarge { size: u64, max: u64 },
    /// 409 `destination-exists`: a rename's destination is already a
    /// file here — cited, tracked, or an untracked object this crate
    /// did not put there. `current` is its entity-tag.
    #[error("{path} already exists; a rename does not overwrite (current {current:?})")]
    DestinationExists { path: String, current: Option<String> },
    /// 404 `no-draft`. The message says which shape: no draft at all,
    /// a body whose meta never landed, or a meta whose body is gone.
    #[error("{0}")]
    NoDraft(String),
    /// 409 `draft-stale`: `files/<path>` moved since the draft was
    /// taken. The draft is KEPT. `current` is what is there now (the
    /// wire's `x-flint-current-etag`), `None` when the file is gone.
    #[error("{message}")]
    DraftStale { current: Option<String>, message: String },
    /// 409 `draft-moved`: the draft body or the file under it moved
    /// during the promote (a second tab re-saved it). Retryable.
    #[error("the draft of {0} or the file under it moved during this promote; retry")]
    DraftMoved(String),
    /// 403 `stale-epoch`: a syncer verb claiming an epoch that is not
    /// the cell's CURRENT epoch — the deposed straggler's door (P5).
    #[error("cell is at epoch {cell_epoch} (holder {holder_id}), request claims {claimed}")]
    StaleEpoch { cell_epoch: u64, holder_id: String, claimed: u64 },
    /// 403 `no-holder`: a syncer verb against a workspace with no lease
    /// cell at all.
    #[error("no lease cell exists for this workspace")]
    NoHolder,
    /// 403 `read-only`: the workspace was built with
    /// `Workspace::read_only` and this verb writes. Refused before any
    /// request: nothing was read or written. Not retryable — the caller's
    /// access refused it, not the workspace's state.
    #[error("this workspace is read-only; the verb writes and was not performed")]
    ReadOnly,
    /// 409 `cas-miss`: the manifest CAS lost. `current` is the etag
    /// the manifest carries now.
    #[error("current manifest etag: {current:?}")]
    CasMiss { current: Option<String> },
    /// 202 `citation-pending`: the write is durable and tracked, and
    /// the manifest does not cite it yet — the syncer has not run a
    /// barrier over it within the wait, or no syncer holds the lease.
    /// Only `wait_cited` produces this; `put_file` never does.
    #[error("{path} at {etag} is durable and tracked but not yet cited: {reason}")]
    CitationPending { path: String, etag: String, reason: String },
    /// 409 `superseded`: the manifest cites `path` at another etag — a
    /// later write won.
    #[error("{path} is cited at {cited_etag}, not at the awaited etag; a later write superseded it")]
    Superseded { path: String, cited_etag: String },
    /// 500 `encode`: a document this crate builds failed to serialise.
    #[error("{0}")]
    Encode(String),
    /// 502 `corrupt`: the bytes the store returned for a cited handle do not
    /// hash to the CRC the citation carries (M8). Nothing was served.
    #[error("{path}: the stored bytes hash to CRC-64 {got}, the citation says {want}; refusing to serve them")]
    Corrupt { path: String, want: String, got: String },
    /// 502 `store`: the object store failed. Retryable in general; the
    /// source says why.
    #[error("{0}")]
    Store(#[source] LeanError),
}

impl VerbError {
    /// The HTTP status `flint-lean-gateway` answers with.
    pub fn status(&self) -> u16 {
        match self {
            VerbError::BadPath(_) | VerbError::BadUser(_) | VerbError::BadPrecondition(_) => 400,
            VerbError::StaleEpoch { .. }
            | VerbError::NoHolder
            | VerbError::ReadOnly => 403,
            VerbError::NoSuchFile(_) | VerbError::NoDraft(_) => 404,
            VerbError::ConcurrentWrite
            | VerbError::Moved
            | VerbError::DraftStale { .. }
            | VerbError::DraftMoved(_)
            | VerbError::DestinationExists { .. }
            | VerbError::CasMiss { .. } => 409,
            VerbError::ForeignWrite { .. } => 410,
            VerbError::FileChanged { .. } => 412,
            VerbError::TooLarge { .. } => 413,
            VerbError::PreconditionRequired => 428,
            VerbError::CitationPending { .. } => 202,
            VerbError::Superseded { .. } => 409,
            VerbError::Encode(_) => 500,
            VerbError::Corrupt { .. } => 502,
            VerbError::Store(_) => 502,
        }
    }

    /// The `error` code on the wire.
    pub fn code(&self) -> &'static str {
        match self {
            VerbError::BadPath(_) => "bad-path",
            VerbError::BadUser(_) => "bad-user",
            VerbError::BadPrecondition(_) => "bad-precondition",
            VerbError::PreconditionRequired => "precondition-required",
            VerbError::FileChanged { .. } => "file-changed",
            VerbError::ConcurrentWrite => "concurrent-write",
            VerbError::Moved => "moved",
            VerbError::NoSuchFile(_) => "no-such-file",
            VerbError::ForeignWrite { .. } => "foreign-write",
            VerbError::TooLarge { .. } => "payload-too-large",
            VerbError::DestinationExists { .. } => "destination-exists",
            VerbError::NoDraft(_) => "no-draft",
            VerbError::DraftStale { .. } => "draft-stale",
            VerbError::DraftMoved(_) => "draft-moved",
            VerbError::StaleEpoch { .. } => "stale-epoch",
            VerbError::NoHolder => "no-holder",
            VerbError::ReadOnly => "read-only",
            VerbError::CasMiss { .. } => "cas-miss",
            VerbError::CitationPending { .. } => "citation-pending",
            VerbError::Superseded { .. } => "superseded",
            VerbError::Encode(_) => "encode",
            VerbError::Corrupt { .. } => "corrupt",
            VerbError::Store(_) => "store",
        }
    }

    /// The pacing hint a 409 carries: the wire's `Retry-After`. Every
    /// conflict says 2 s (callers poll; a default beats a stampede).
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            e if e.status() == 409 => Some(2),
            _ => None,
        }
    }

    /// The entity-tag a refusal names: what the caller should have sent
    /// (`file-changed`, on the wire's `etag` header) or what is there
    /// now (`draft-stale`, on `x-flint-current-etag`).
    pub fn current_etag(&self) -> Option<&str> {
        match self {
            VerbError::FileChanged { current }
            | VerbError::DraftStale { current, .. }
            | VerbError::DestinationExists { current, .. } => current.as_deref(),
            _ => None,
        }
    }

    /// True when the caller is expected to re-read and retry the same
    /// request (a conflict), false when the request itself is wrong.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            VerbError::ConcurrentWrite
                | VerbError::Moved
                | VerbError::DraftMoved(_)
                | VerbError::Store(_)
        )
    }
}

/// The blanket arm: any store or state failure the verb did not name
/// is a 502. Verbs that answer a `LeanError::Fenced` or `State` with
/// something more specific match those BEFORE reaching for `?`.
impl From<LeanError> for VerbError {
    fn from(e: LeanError) -> Self {
        VerbError::Store(e)
    }
}

impl From<StoreError> for VerbError {
    fn from(e: StoreError) -> Self {
        VerbError::Store(LeanError::Store(e))
    }
}

/// Workspace-relative path hygiene: no traversal, no absolute, no
/// reserved namespaces, no empty segments.
/// A path as the workspace tracks it: the handle its newest tracked or
/// cited bytes live at, their etag, and their CRC when a writer recorded
/// one (`Workspace::lookup`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tracked {
    pub key: String,
    pub etag: String,
    pub crc64_b64: Option<String>,
}

pub fn path_ok(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.split('/').any(|seg| {
            seg.is_empty()
                || seg == "."
                || seg == ".."
                || seg == flint_lean::STATE_DIR
                // The syncer's temp-sibling suffix (review 2026-09-18, C1):
                // the walk skips the name at every depth, so a write under
                // it was materialised by the consume, cited once as a first
                // absence, and collected as a delete at the next barrier.
                || seg.ends_with(flint_lean::scan::TMP_SUFFIX)
        })
        && !path.starts_with(".flint/")
        && path != ".flint"
}

/// The ONE entity-tag normalisation rule in this crate.
///
/// A function and not a closure because `drafts.rs` needs the same
/// rule, and the first version of that module wrote its own — stripping
/// the caller's quotes and then comparing the result against the
/// store's UNSTRIPPED etag, so every promote read as stale. Two rules
/// that must agree are one rule waiting to drift; this is the rule.
///
/// Normalise for COMPARISON only. What gets handed to `PutCondition`
/// is always the STORE's own form — see `judge_preconditions`, which
/// returns the store's `current`, never the caller's tag.
pub fn normalize_etag(v: &str) -> String {
    v.trim().trim_matches('"').to_string()
}

/// What the caller's preconditions demand of the object as it stands.
///
/// The taxonomy is forge's (`forge/syncer/src/fileapi.rs`), deliberately:
/// its file API and this one are supposed to be one shape, so a client
/// that speaks to a forge repository speaks to a lean workspace without
/// knowing which it has. The one case worth naming is
/// `PreconditionRequired` — an overwrite that carries no `If-Match` is
/// REFUSED rather than performed.
///
/// `Ok(Some(etag))` ⇒ condition the PUT on that etag (the STORE's own
/// form); `Ok(None)` ⇒ a create (`If-None-Match: *`).
pub fn judge_preconditions(
    current: Option<&str>,
    if_match: Option<&str>,
    if_none_match: Option<&str>,
) -> Result<Option<String>, VerbError> {
    let none_match_star = match if_none_match.map(normalize_etag) {
        None => false,
        Some(v) if v == "*" => true,
        // `If-None-Match: "<tag>"` on a write asks "unless it is still
        // exactly this", which needs its own arm to evaluate honestly.
        // Refusing beats accepting and checking something else.
        Some(_) => {
            return Err(VerbError::BadPrecondition(
                "If-None-Match on a write is supported only as `*` (create if absent)",
            ))
        }
    };
    let matched = if_match.map(normalize_etag);
    if matched.is_some() && none_match_star {
        return Err(VerbError::BadPrecondition(
            "If-Match and If-None-Match: * cannot both hold on one request",
        ));
    }
    match (current, matched) {
        // The whole point: an overwrite must name what it read.
        (Some(_), None) if !none_match_star => Err(VerbError::PreconditionRequired),
        (Some(cur), None) => Err(VerbError::FileChanged { current: Some(cur.to_string()) }),
        // BOTH sides are normalised. S3 hands back a QUOTED entity-tag
        // and that is exactly what `GET` puts on its `etag` header, so a
        // caller echoing the header back sends quotes — comparing a
        // stripped caller tag against an unstripped stored one 412s
        // every honest writer. It did, on the first run of the test.
        (Some(cur), Some(t)) if t == "*" || t == normalize_etag(cur) => Ok(Some(cur.to_string())),
        (Some(cur), Some(_)) => Err(VerbError::FileChanged { current: Some(cur.to_string()) }),
        // `If-Match` on a path that is not there is a stale caller: it
        // read a file that has since been deleted. `*` included — RFC
        // 9110 §13.1.1 makes it a demand that a representation exist.
        (None, Some(_)) => Err(VerbError::FileChanged { current: None }),
        (None, None) => Ok(None),
    }
}

impl Workspace {
    /// A workspace at `prefix` on `store`. The prefix is the subtree's
    /// bucket key prefix — the same string the syncer was started with
    /// (`FLINT_SYNC_PREFIX`) and the gateway's `id=prefix` mapping
    /// named. A trailing slash is dropped.
    pub fn new(store: Arc<dyn ObjectStore>, prefix: &str) -> Self {
        Workspace {
            store,
            // Verbs never touch a local tree; the root is unused.
            cfg: LeanConfig::new(prefix, "/nonexistent"),
            max_put_bytes: WHOLE_PUT_MAX,
            read_only: false,
            manifests: Arc::default(),
        }
    }

    /// The same workspace for a caller who may only read — a user whose
    /// role is read access (per-user access design §4.6). Every reading
    /// verb answers as on `new`; every writing verb — `put_file`,
    /// `remove_file(s)`, `rename_file(s)`,
    /// `request_boundary`, `request_sync`, `put_draft`, `delete_draft`,
    /// `promote_draft` and the syncer-facing four — answers
    /// `VerbError::ReadOnly` before it sends a request.
    ///
    /// Two layers. The typed refusal is the honest answer; the store is
    /// also wrapped in `flint_store::ReadOnly`, so a write that goes
    /// around the verbs — through `store()`, which is public — is refused
    /// too, with `StoreError::Auth`. Neither is the enforcement: hand a
    /// read-only workspace a store built on a credential that cannot
    /// write, and the bucket refuses whatever this crate misses.
    pub fn read_only(store: Arc<dyn ObjectStore>, prefix: &str) -> Self {
        let mut ws = Self::new(Arc::new(ReadOnly::new(store)), prefix);
        ws.read_only = true;
        ws
    }

    /// Built by `read_only`.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The first statement of every writing verb, before any request.
    pub(crate) fn writable(&self) -> Result<(), VerbError> {
        if self.read_only {
            return Err(VerbError::ReadOnly);
        }
        Ok(())
    }

    /// Whole-object ceiling for HITL writes and draft saves (default
    /// 64 MiB, the gateway's `FLINT_LEAN_GW_MAX_PUT_MB`). Multipart
    /// through this door is deferred, so a larger body is refused.
    pub fn with_max_put_bytes(mut self, max: u64) -> Self {
        self.max_put_bytes = max;
        self
    }

    /// Share a manifest cache (a server: one per workspace, across requests).
    pub fn with_manifest_cache(mut self, cache: Arc<ManifestCache>) -> Self {
        self.manifests = cache;
        self
    }

    pub fn prefix(&self) -> &str {
        &self.cfg.prefix
    }

    pub fn max_put_bytes(&self) -> u64 {
        self.max_put_bytes
    }

    pub fn store(&self) -> &Arc<dyn ObjectStore> {
        &self.store
    }

    pub fn config(&self) -> &LeanConfig {
        &self.cfg
    }

    pub(crate) fn check_size(&self, body: &Bytes) -> Result<(), VerbError> {
        let size = body.len() as u64;
        if size > self.max_put_bytes {
            return Err(VerbError::TooLarge { size, max: self.max_put_bytes });
        }
        Ok(())
    }

    // ── HITL / UI-facing ─────────────────────────────────────────────

    /// The UI save COMMITS (P2, simplification step 5, 2026-09-25): the
    /// bytes land at a fresh handle, then the gateway CASes the manifest
    /// itself and acknowledges AFTER the CAS. Returns the entity-tag.
    ///
    /// It never waits on the writers' lease or their commit window (the
    /// user's G1): a writer mid-commit loses its CAS instead and re-merges,
    /// so the pressure of heavy saving falls on the writers (G2). Nothing
    /// goes through the cell; a writer takes the save the way it takes a
    /// peer's publish, through its merge.
    ///
    /// Mine wins only over what the caller read: an overwrite must name
    /// the version (`PreconditionRequired` otherwise), and the name is
    /// judged against the CURRENT document at every CAS attempt, so a
    /// version written since the read is never replaced unseen —
    /// `FileChanged` names it, and the fresh handle is an orphan the sweep
    /// collects. A mirror (a sole-writer workspace) takes no UI writes
    /// (`ReadOnly`). A save that keeps losing its CAS to writers answers
    /// `ConcurrentWrite`, which a retry can clear.
    pub async fn put_file(
        &self,
        path: &str,
        body: Bytes,
        opts: &PutFile,
    ) -> Result<String, VerbError> {
        self.writable()?;
        if !path_ok(path) {
            return Err(VerbError::BadPath(path.to_string()));
        }
        self.check_size(&body)?;
        // Judged once before any bytes move, so a stale caller costs no PUT.
        let read = manifest::load(self.store.as_ref(), &self.cfg).await?;
        self.judge_save(read.as_ref(), path, opts)?;
        let prev = read.as_ref().and_then(|l| l.manifest.entries.get(path)).cloned();
        let crc = crc64_nvme(&body);
        let size = body.len() as u64;
        let added_unix = now_unix();
        let flush = flint_lean::ui_flush(added_unix);
        let key = self.cfg.handle_key(path, &flush);
        let stamps = GenerationStamps {
            generation: prev.as_ref().map(|e| e.generation).unwrap_or(0) + 1,
            epoch: 0, // a UI save carries no lease epoch
            flush_uuid: flush.clone(),
            boundary_source: None,
            posix: None,
        };
        let meta = self.store.put_whole(&key, body, &PutCondition::IfNoneMatchAny, &stamps, crc).await?;
        let save = Save {
            path,
            key,
            etag: meta.etag.clone(),
            crc64_b64: flint_store::crc64_to_b64(crc),
            size,
            mtime_unix: added_unix as i64,
            flush,
        };
        self.commit_save(&save, |doc| self.judge_save(doc, path, opts)).await?;
        Ok(meta.etag)
    }

    /// Cite a save's fresh handle in ONE manifest CAS (`commit_edit`).
    /// `judge` sees the document the CAS would replace; a refusal leaves
    /// the handle an orphan for the sweep and the document untouched.
    pub(crate) async fn commit_save(
        &self,
        save: &Save<'_>,
        judge: impl Fn(Option<&LoadedManifest>) -> Result<(), VerbError>,
    ) -> Result<(), VerbError> {
        self.commit_edit(&save.flush, |current, doc| {
            judge(current)?;
            let was = doc.entries.get(save.path);
            let entry = LeanEntry {
                key: save.key.clone(),
                etag: save.etag.clone(),
                crc64_b64: save.crc64_b64.clone(),
                size: save.size,
                mode: was.map(|e| e.mode).unwrap_or(0o644),
                mtime_unix: save.mtime_unix,
                generation: was.map(|e| e.generation).unwrap_or(0) + 1,
                epoch: 0,
            };
            doc.tombstones.remove(save.path);
            doc.entries.insert(save.path.to_string(), entry);
            Ok(())
        })
        .await
    }

    /// ONE manifest CAS for a UI edit (P2): load the current document,
    /// let `edit` judge it and change a copy (the next generation, with
    /// the installing party's fields reset, as `manifest::merge` does),
    /// CAS it in, and on a lost race do it all again against what won.
    /// Never takes the writers' lease and never waits on their window;
    /// `ConcurrentWrite` after `manifest::EDIT_CAS_ATTEMPTS` lost races.
    pub(crate) async fn commit_edit(
        &self,
        flush: &str,
        edit: impl Fn(Option<&LoadedManifest>, &mut LeanManifest) -> Result<(), VerbError>,
    ) -> Result<(), VerbError> {
        match manifest::commit_edit(self.store.as_ref(), &self.cfg, flush, edit).await {
            Ok(()) => Ok(()),
            Err(manifest::EditError::Refused(e)) => Err(e),
            Err(manifest::EditError::Store(e)) => Err(e.into()),
            Err(manifest::EditError::Contended) => Err(VerbError::ConcurrentWrite),
        }
    }

    /// A save's preconditions against a document: a mirror takes none; an
    /// overwrite must name what it read, and what it names must be what the
    /// document cites NOW.
    fn judge_save(&self, doc: Option<&LoadedManifest>, path: &str, opts: &PutFile) -> Result<(), VerbError> {
        if doc.is_some_and(|l| l.manifest.sole_writer) {
            return Err(VerbError::ReadOnly);
        }
        let current = doc.and_then(|l| l.manifest.entries.get(path)).map(|e| e.etag.clone());
        judge_preconditions(current.as_deref(), opts.if_match.as_deref(), opts.if_none_match.as_deref())?;
        Ok(())
    }

    /// Read the bytes the manifest cites for `path`, by their handle.
    /// Every UI edit commits (P2), so the citation is the newest version
    /// the workspace has, and a read costs one object fetch.
    pub async fn get_file(&self, path: &str) -> Result<Blob, VerbError> {
        if !path_ok(path) {
            return Err(VerbError::BadPath(path.to_string()));
        }
        // The bytes the manifest cites, read by their HANDLE (design
        // 2026-09-19); no cell is read. A handle is immutable, so a fetch
        // guarded on its etag fails only for a handle something overwrote from outside
        // (`moved`: a sole-writer workspace names the stranger), and a
        // handle that is gone is one the collector took after the
        // pointer moved on — re-resolve once, then it is a hole.
        let m = self.view().await?;
        let sole_writer = m.as_ref().map(|l| l.manifest.sole_writer).unwrap_or(false);
        let moved = |path: &str| -> VerbError {
            if sole_writer {
                VerbError::ForeignWrite { path: path.to_string() }
            } else {
                VerbError::Moved
            }
        };
        let Some(tracked) = self.lookup(m.as_deref(), path) else {
            return Err(VerbError::NoSuchFile(path.to_string()));
        };
        // M8: the bytes are verified against the CRC the citation carries,
        // as checkout's and the consume's fetches are. A mismatch is refused,
        // never served.
        let verified = |t: &Tracked, meta: flint_store::ObjectMeta, body: Bytes| -> Result<Blob, VerbError> {
            if let Some(want) = t.crc64_b64.as_deref().filter(|c| !c.is_empty()) {
                let got = crc64_to_b64(crc64_nvme(&body));
                if got != want {
                    return Err(VerbError::Corrupt { path: path.to_string(), want: want.to_string(), got });
                }
            }
            Ok(Blob { etag: meta.etag, body })
        };
        match self.store.get_whole(&tracked.key, Some(&tracked.etag)).await {
            Ok((meta, body)) => verified(&tracked, meta, body),
            Err(StoreError::PreconditionFailed(_)) => Err(moved(path)),
            Err(StoreError::NotFound(_)) => {
                let m = self.view().await?;
                let Some(again) = self.lookup(m.as_deref(), path) else {
                    return Err(VerbError::NoSuchFile(path.to_string()));
                };
                if again.key == tracked.key {
                    return Err(VerbError::NoSuchFile(path.to_string()));
                }
                match self.store.get_whole(&again.key, Some(&again.etag)).await {
                    Ok((meta, body)) => verified(&again, meta, body),
                    Err(StoreError::PreconditionFailed(_)) => Err(moved(path)),
                    Err(StoreError::NotFound(_)) => Err(VerbError::NoSuchFile(path.to_string())),
                    Err(e) => Err(e.into()),
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// `{manifest, manifest_etag, inbox}`: the sync verb's one-stop read.
    pub async fn snapshot(&self) -> Result<Snapshot, VerbError> {
        let m = manifest::load(self.store.as_ref(), &self.cfg).await?;
        let ib = inbox::load(self.store.as_ref(), &self.cfg).await?;
        let (manifest, manifest_etag) = match m {
            Some(l) => (l.manifest, Some(l.etag)),
            None => (Default::default(), None),
        };
        Ok(Snapshot { manifest, manifest_etag, inbox: ib.doc })
    }

    /// Seq, the epoch cell, standing requests.
    ///
    /// ONE manifest request for all the manifest-derived fields, and
    /// under the pointer layout it reads the POINTER — a few hundred
    /// bytes — rather than a document that runs to ~66 MiB at the 250k
    /// cap. A legacy workspace keeps a HEAD of the single object: every
    /// field it needs rides the stamps `cas_write_stamped` writes
    /// (`generation` IS the seq), and the stamp and the document agree
    /// by construction.
    pub async fn status(&self) -> Result<Status, VerbError> {
        let (seq, stamp_unix, boundary_source) =
            match manifest::load_pointer(self.store.as_ref(), &self.cfg).await? {
                Some(lp) => {
                    (Some(lp.pointer.seq), lp.last_modified_unix, lp.pointer.boundary_source)
                }
                None => match self.store.head(&self.cfg.manifest_key()).await {
                    Ok(meta) => {
                        let stamps = GenerationStamps::from_meta(&meta.meta);
                        (
                            stamps.as_ref().map(|s| s.generation),
                            meta.last_modified_unix,
                            stamps.and_then(|s| s.boundary_source),
                        )
                    }
                    Err(StoreError::NotFound(_)) => (None, None, None),
                    Err(e) => return Err(e.into()),
                },
            };
        let ib = inbox::load(self.store.as_ref(), &self.cfg).await?;
        let cell = self.store.epoch_read(&self.cfg.epoch_key()).await?;
        Ok(Status {
            seq,
            epoch: cell.as_ref().map(|c| c.epoch),
            holder_id: cell.as_ref().map(|c| c.holder_id.clone()),
            holder_released: cell.as_ref().map(|c| c.released),
            now_unix: now_unix(),
            last_cited_seq: seq,
            manifest_stamp_unix: stamp_unix,
            boundary_source,
            boundary_request: ib.doc.boundary_request.clone(),
            sync_request: ib.doc.sync_request.clone(),
        })
    }

    // ── delete and rename (docs/plans/flint-lean-delete-rename-design.md) ──
    //
    // A caller outside the pod cannot touch the tree, and must never
    // delete an object itself (§9: a cited object deleted from outside
    // wedges every checkout with "manifest cites it but it is gone").
    // So a removal COMMITS as a save does (P2): one CAS that stops citing
    // the path (the tombstone names what it retired) and deletes nothing;
    // the object goes to the retire reap once nothing cites it. Each
    // syncer's next consume unlinks the file where its tree is clean, and
    // an agent's unpublished edit on the path publishes over the delete
    // with a record. This crate exposes no function that deletes a cited
    // object.

    /// What the workspace knows about a path: the HANDLE and etag it is
    /// cited at, and the CRC of those bytes. Under P2 every UI edit
    /// commits, so the citation is the whole story. `None` = not a file here.
    pub(crate) fn lookup(&self, m: Option<&LoadedManifest>, path: &str) -> Option<Tracked> {
        m.and_then(|l| l.manifest.entries.get(path))
            .map(|e| Tracked { key: e.key.clone(), etag: e.etag.clone(), crc64_b64: Some(e.crc64_b64.clone()) })
    }

    /// The current manifest: the cached one while the pointer has not moved
    /// (one small GET), else a load, cached by the etag it was loaded at. A
    /// legacy single-object workspace has no pointer to key on and loads.
    pub(crate) async fn view(&self) -> Result<Option<Arc<LoadedManifest>>, VerbError> {
        let Some(p) = manifest::load_pointer(self.store.as_ref(), &self.cfg).await? else {
            return Ok(manifest::load(self.store.as_ref(), &self.cfg).await?.map(Arc::new));
        };
        if let Some(hit) = self.manifests.0.lock().unwrap().as_ref().filter(|c| c.etag == p.etag) {
            return Ok(Some(hit.clone()));
        }
        let loaded = manifest::load(self.store.as_ref(), &self.cfg).await?.map(Arc::new);
        if let Some(l) = &loaded {
            *self.manifests.0.lock().unwrap() = Some(l.clone());
        }
        Ok(loaded)
    }

    /// Delete a file: it COMMITS (P2) — one manifest CAS stops citing it,
    /// and the document's tombstone names what was deleted. `if_match` is
    /// judged against the citation (`FileChanged` names the current tag),
    /// again at every CAS attempt; `None` skips the check. The object goes
    /// to the sweep once nothing cites it. Never waits on the writers.
    pub async fn remove_file(
        &self,
        path: &str,
        author: Option<&str>,
        if_match: Option<&str>,
    ) -> Result<(), VerbError> {
        self.remove_files(&[(path, if_match)], author).await
    }

    /// `remove_file` for many paths in ONE commit — a folder delete is one
    /// manifest generation. Every path is judged before anything changes:
    /// one unknown path or failed precondition deletes none of them.
    pub async fn remove_files(
        &self,
        paths: &[(&str, Option<&str>)],
        _author: Option<&str>,
    ) -> Result<(), VerbError> {
        self.writable()?;
        for (path, _) in paths {
            if !path_ok(path) {
                return Err(VerbError::BadPath(path.to_string()));
            }
        }
        if paths.is_empty() {
            return Ok(());
        }
        let flush = flint_lean::ui_flush(now_unix());
        self.commit_edit(&flush, |current, doc| {
            if current.is_some_and(|l| l.manifest.sole_writer) {
                return Err(VerbError::ReadOnly);
            }
            for (path, if_match) in paths {
                let Some(cited) = doc.entries.get(*path) else {
                    return Err(VerbError::NoSuchFile(path.to_string()));
                };
                if let Some(tag) = if_match {
                    let t = normalize_etag(tag);
                    if t != "*" && t != normalize_etag(&cited.etag) {
                        return Err(VerbError::FileChanged { current: Some(cited.etag.clone()) });
                    }
                }
            }
            for (path, _) in paths {
                let gone = doc.entries.remove(*path).expect("judged above");
                doc.tombstones.insert(path.to_string(), manifest::Tombstone { etag: gone.etag, seq: doc.seq });
            }
            Ok(())
        })
        .await
    }

    /// Rename or move a file. Returns the destination's entity-tag. It
    /// COMMITS (P2): one manifest CAS moves the citation — a manifest
    /// reader sees the old name or the new, never both and never neither.
    pub async fn rename_file(
        &self,
        from: &str,
        to: &str,
        author: Option<&str>,
    ) -> Result<String, VerbError> {
        let mut etags = self.rename_files(&[(from, to)], author).await?;
        Ok(etags.pop().expect("one pair, one etag"))
    }

    /// `rename_file` for many pairs in ONE commit — a folder move.
    ///
    /// A rename is a CITATION MOVE (design 2026-09-19, R6): the
    /// destination's entry names the SOURCE's handle, the source is no
    /// longer cited and its tombstone names what moved, and no bytes move.
    /// Refused whole, before anything changes: `NoSuchFile` for a source
    /// that is not cited, `DestinationExists` for a destination that is,
    /// `BadPath` for either. Never waits on the writers (G1).
    pub async fn rename_files(
        &self,
        pairs: &[(&str, &str)],
        _author: Option<&str>,
    ) -> Result<Vec<String>, VerbError> {
        self.writable()?;
        let mut seen_to = std::collections::BTreeSet::new();
        for (from, to) in pairs {
            if !path_ok(from) {
                return Err(VerbError::BadPath(from.to_string()));
            }
            if !path_ok(to) || from == to || !seen_to.insert(to.to_string()) {
                return Err(VerbError::BadPath(to.to_string()));
            }
        }
        let flush = flint_lean::ui_flush(now_unix());
        let moved: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(vec![]);
        self.commit_edit(&flush, |current, doc| {
            if current.is_some_and(|l| l.manifest.sole_writer) {
                return Err(VerbError::ReadOnly);
            }
            for (from, to) in pairs {
                if !doc.entries.contains_key(*from) {
                    return Err(VerbError::NoSuchFile(from.to_string()));
                }
                if let Some(there) = doc.entries.get(*to) {
                    return Err(VerbError::DestinationExists { path: to.to_string(), current: Some(there.etag.clone()) });
                }
            }
            let mut etags = vec![];
            for (from, to) in pairs {
                let entry = doc.entries.remove(*from).expect("judged above");
                doc.tombstones.insert(from.to_string(), manifest::Tombstone { etag: entry.etag.clone(), seq: doc.seq });
                doc.tombstones.remove(*to);
                etags.push(entry.etag.clone());
                doc.entries.insert(to.to_string(), entry);
            }
            *moved.lock().unwrap() = etags;
            Ok(())
        })
        .await?;
        Ok(moved.into_inner().unwrap())
    }

    /// Ask the workspace to publish (§2.5).
    ///
    /// Nothing is published here and no epoch is held: a field is set,
    /// and the syncer performs the barrier under its own lease,
    /// min-interval and budget. That is what keeps a leaked credential
    /// from turning into an unbounded publish loop, and what keeps this
    /// verb honest about what it can promise — the answer says the
    /// request was RECORDED, never that a boundary happened.
    pub async fn request_boundary(&self, requestor: Option<&str>) -> Result<Accepted, VerbError> {
        self.request_verb(RequestedVerb::Boundary, requestor).await
    }

    /// Ask the workspace to pull. CARRIED, never performed (D14): the
    /// syncer copies it into `.flint/remote.seq` and stops. `sync`
    /// deletes local files for remotely-deleted paths, so performing
    /// it on a remote's say-so would upgrade a leaked credential from
    /// "publish, plus hand over these N named objects" to "rewrite and
    /// delete across a running agent's tree, at my timing".
    pub async fn request_sync(&self, requestor: Option<&str>) -> Result<Accepted, VerbError> {
        self.request_verb(RequestedVerb::Sync, requestor).await
    }

    async fn request_verb(
        &self,
        verb: RequestedVerb,
        requestor: Option<&str>,
    ) -> Result<Accepted, VerbError> {
        self.writable()?;
        let requestor = requestor.unwrap_or("gateway");
        let req = inbox::gateway_request(self.store.as_ref(), &self.cfg, verb, requestor).await?;
        Ok(Accepted {
            status: "recorded".into(),
            verb: match verb {
                RequestedVerb::Boundary => "boundary",
                RequestedVerb::Sync => "sync",
            }
            .into(),
            requested_unix: req.requested_unix,
            requestor: req.requestor,
            note: match verb {
                RequestedVerb::Boundary => {
                    "the syncer honors this as a publish sentinel at its next tick, \
                     subject to the same min-interval and hourly budget; the ack lands \
                     in .flint/publish.ack"
                }
                RequestedVerb::Sync => {
                    "CARRIED, not executed: the syncer moves .flint/remote.seq and the \
                     agent decides whether to sync"
                }
            }
            .into(),
        })
    }

    /// Wait until the manifest CITES `etag` at `path` — the moment a
    /// fresh checkout would see the write. Opt-in: `put_file` has
    /// already made the write durable, tracked, and visible to every
    /// reader that goes through this crate or through `sync`; this is
    /// for a caller that also wants to know when the coherent view
    /// caught up, typically after a `request_boundary`.
    ///
    /// Polls the POINTER (a few hundred bytes) every `poll` and reads
    /// the manifest only when its seq moved. Returns the citing seq, or
    /// `CitationPending` (HTTP 202) when `timeout` passes: the write is
    /// durable and tracked either way, and a syncer that starts later
    /// cites it. There is no early refusal for "no syncer is running" —
    /// the bucket cannot tell a dead syncer from an idle one, and the
    /// caller's timeout is the bound it asked for. A
    /// manifest that cites the path at a DIFFERENT etag means the write
    /// was superseded before or after citation: `Superseded`.
    pub async fn wait_cited(
        &self,
        path: &str,
        etag: &str,
        timeout: std::time::Duration,
        poll: std::time::Duration,
    ) -> Result<u64, VerbError> {
        if !path_ok(path) {
            return Err(VerbError::BadPath(path.to_string()));
        }
        let pending = |reason: &str| VerbError::CitationPending {
            path: path.to_string(),
            etag: etag.to_string(),
            reason: reason.to_string(),
        };
        let want = normalize_etag(etag);
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last_seq: Option<u64> = None;
        loop {
            // The pointer first; the entries only when the seq moved.
            let seq = manifest::load_pointer(self.store.as_ref(), &self.cfg)
                .await?
                .map(|lp| lp.pointer.seq);
            if seq != last_seq || last_seq.is_none() {
                last_seq = seq;
                if let Some(l) = manifest::load(self.store.as_ref(), &self.cfg).await? {
                    if let Some(e) = l.manifest.entries.get(path) {
                        if normalize_etag(&e.etag) == want {
                            return Ok(l.manifest.seq);
                        }
                        // Cited, but not our bytes: a later write won.
                        {
                            return Err(VerbError::Superseded {
                                path: path.to_string(),
                                cited_etag: e.etag.clone(),
                            });
                        }
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(pending("the syncer did not cite it within the wait"));
            }
            tokio::time::sleep(poll.min(deadline.saturating_duration_since(tokio::time::Instant::now()))).await;
        }
    }

    // ── syncer-facing (epoch-validated PER REQUEST) ──────────────────
    //
    // P5's teeth: a write whose claimed epoch is not the cell's CURRENT
    // epoch is rejected, closing the deposed-straggler door the model's
    // LeanNoEpochCheck mutation proves rotation alone leaves open. A
    // backend serving a UI has no use for these; they are here because
    // they are the gateway's, and the HTTP layer is built on them.

    /// The claimed epoch must be the cell's CURRENT epoch. A deposed
    /// writer's stale epoch — or a claim over an empty cell — is refused.
    async fn require_current_epoch(&self, claimed: u64) -> Result<(), VerbError> {
        match self.store.epoch_read(&self.cfg.epoch_key()).await? {
            Some(state) if state.epoch == claimed => Ok(()),
            Some(state) => Err(VerbError::StaleEpoch {
                cell_epoch: state.epoch,
                holder_id: state.holder_id,
                claimed,
            }),
            None => Err(VerbError::NoHolder),
        }
    }

    /// CAS the manifest. Returns the new handle's etag; `CasMiss`
    /// carries the etag the manifest has now.
    ///
    /// The wire carries an ETag, not a LAYOUT. A workspace
    /// mid-migration still has its legacy object and no pointer, and
    /// the two take different preconditions, so this reads which one
    /// the workspace is on rather than guessing — a HITL CAS must
    /// present exactly what a local writer would. The previous chunk
    /// list is not carried, so a chunked HITL CAS re-sends every chunk:
    /// §6 chose exactly this trade, because the path is rare and
    /// correctness beats throughput on it. Nor is the document it replaces,
    /// so this path logs no retirements (M1): what it retires falls to the
    /// sweeps' write-age rule, the shape before the retire age.
    pub async fn cas_manifest(
        &self,
        manifest: &LeanManifest,
        expected_etag: Option<&str>,
        epoch: u64,
        flush_uuid: &str,
    ) -> Result<String, VerbError> {
        self.writable()?;
        self.require_current_epoch(epoch).await?;
        let legacy =
            matches!(manifest::load_pointer(self.store.as_ref(), &self.cfg).await, Ok(None));
        let handle = expected_etag.map(|e| manifest::ManifestHandle {
            etag: e.to_string(),
            legacy,
            prev_chunks: Vec::new(),
            prev_tombstones: None,
        });
        match manifest::cas_write(
            self.store.as_ref(),
            &self.cfg,
            manifest,
            handle.as_ref(),
            epoch,
            flush_uuid,
        )
        .await
        {
            Ok(meta) => Ok(meta.etag),
            Err(LeanError::Store(StoreError::PreconditionFailed(_)))
            | Err(LeanError::Store(StoreError::Conflict(_))) => {
                let current = manifest::load(self.store.as_ref(), &self.cfg)
                    .await
                    .ok()
                    .flatten()
                    .map(|l| l.etag);
                Err(VerbError::CasMiss { current })
            }
            Err(e) => Err(e.into()),
        }
    }
}
