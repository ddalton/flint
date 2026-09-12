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
//! gateway made — object first and inbox entry second, never a manifest
//! edit from a HITL write, the barrier window read from the cell on
//! every write, epoch-validated syncer verbs — is made here, because
//! this is where it was always made.

use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use flint_store::{crc64_nvme, GenerationStamps, ObjectStore, PutCondition, StoreError};

use flint_lean::inbox::{self, InboxDoc, InboxEntry, RequestedVerb, VerbRequest, Window};
use flint_lean::manifest::{self, LeanManifest};
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
    window_wait: Option<std::time::Duration>,
}

/// What a HITL write brings besides its bytes. The preconditions are
/// HTTP-shaped on purpose — a backend forwarding a browser's `If-Match`
/// hands the header value over verbatim and gets the gateway's exact
/// judgement of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PutFile {
    /// Recorded on the inbox entry (the audit surface). `None` ⇒ `ui`.
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

/// The sync verb's one-stop read: the cited manifest and the inbox.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub manifest: LeanManifest,
    /// `None` ⇒ nothing has been published yet.
    pub manifest_etag: Option<String>,
    pub inbox: InboxDoc,
}

/// The RPO observability surface: seq, window, inbox depth, the epoch
/// cell, and the standing verb requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub seq: Option<u64>,
    pub window: Option<Window>,
    pub inbox_depth: usize,
    pub epoch: Option<u64>,
    pub holder_id: Option<String>,
    pub holder_released: Option<bool>,
    pub now_unix: u64,
    /// The last CITED manifest seq — under gated mode this is the
    /// coherent view, not the newest bytes in the bucket.
    pub last_cited_seq: Option<u64>,
    pub manifest_stamp_unix: Option<u64>,
    /// Which coherent point installed it: `sentinel`, `quiescence`,
    /// `forced-lag-cap`, `drain`, `recovered`… A reader that cares
    /// whether the view it is about to take was DECLARED coherent or
    /// forced by a cap can tell, from the bucket.
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
    /// 409 `barrier-window-open` (+ `Retry-After`): a publish barrier
    /// is in flight. Nothing was written; retry after the window.
    #[error("{message}")]
    WindowOpen { retry_after_secs: u64, message: String },
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
    /// 410 `dangling-citation`: the manifest cites a version the
    /// backstop has reaped (D8). Never falls back to the current
    /// object, which is precisely the uncited bytes gating withholds.
    #[error("the manifest cites {path} version {version_id} but that version is gone; run `flint-sync recover-staged` to re-cite forward")]
    DanglingCitation { path: String, version_id: String },
    /// 410 `foreign-write`: a sole-writer workspace whose object no
    /// longer carries the cited etag — something other than its
    /// publisher wrote it.
    #[error("the manifest cites {path} at an etag the object no longer carries, and this workspace is published by a sole writer — something other than its publisher wrote that object")]
    ForeignWrite { path: String },
    /// 410 `uncited-bytes`: under `pinned_reads`, an entry the citation
    /// could not make version-addressable whose object has moved.
    #[error("the manifest cites {path} at an etag the object no longer carries and names no version to resolve instead; run `flint-sync recover-staged` to re-cite forward")]
    UncitedBytes { path: String },
    /// 413 `payload-too-large`: the body is over the whole-object cap.
    /// The HTTP gateway refuses this at the body filter; the library
    /// refuses it here so an embedder cannot exceed the cap by
    /// forgetting to check.
    #[error("body is {size} bytes; the whole-object cap is {max}")]
    TooLarge { size: u64, max: u64 },
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
    /// 403 `fenced`: the window verb was refused by the cell.
    #[error("{0}")]
    Fenced(String),
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
    /// 409 `superseded`: the manifest cites `path` at another etag and
    /// the awaited write is no longer in the inbox — a later write won.
    #[error("{path} is cited at {cited_etag}, not at the awaited etag; a later write superseded it")]
    Superseded { path: String, cited_etag: String },
    /// 500 `encode`: a document this crate builds failed to serialise.
    #[error("{0}")]
    Encode(String),
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
            VerbError::StaleEpoch { .. } | VerbError::NoHolder | VerbError::Fenced(_) => 403,
            VerbError::NoSuchFile(_) | VerbError::NoDraft(_) => 404,
            VerbError::WindowOpen { .. }
            | VerbError::ConcurrentWrite
            | VerbError::Moved
            | VerbError::DraftStale { .. }
            | VerbError::DraftMoved(_)
            | VerbError::CasMiss { .. } => 409,
            VerbError::DanglingCitation { .. }
            | VerbError::ForeignWrite { .. }
            | VerbError::UncitedBytes { .. } => 410,
            VerbError::FileChanged { .. } => 412,
            VerbError::TooLarge { .. } => 413,
            VerbError::PreconditionRequired => 428,
            VerbError::CitationPending { .. } => 202,
            VerbError::Superseded { .. } => 409,
            VerbError::Encode(_) => 500,
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
            VerbError::WindowOpen { .. } => "barrier-window-open",
            VerbError::ConcurrentWrite => "concurrent-write",
            VerbError::Moved => "moved",
            VerbError::NoSuchFile(_) => "no-such-file",
            VerbError::DanglingCitation { .. } => "dangling-citation",
            VerbError::ForeignWrite { .. } => "foreign-write",
            VerbError::UncitedBytes { .. } => "uncited-bytes",
            VerbError::TooLarge { .. } => "payload-too-large",
            VerbError::NoDraft(_) => "no-draft",
            VerbError::DraftStale { .. } => "draft-stale",
            VerbError::DraftMoved(_) => "draft-moved",
            VerbError::StaleEpoch { .. } => "stale-epoch",
            VerbError::NoHolder => "no-holder",
            VerbError::Fenced(_) => "fenced",
            VerbError::CasMiss { .. } => "cas-miss",
            VerbError::CitationPending { .. } => "citation-pending",
            VerbError::Superseded { .. } => "superseded",
            VerbError::Encode(_) => "encode",
            VerbError::Store(_) => "store",
        }
    }

    /// The pacing hint a 409 carries: the wire's `Retry-After`. Every
    /// conflict says 2 s (callers poll; a default beats a stampede);
    /// an open window says how long the window has left.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            VerbError::WindowOpen { retry_after_secs, .. } => Some(*retry_after_secs),
            e if e.status() == 409 => Some(2),
            _ => None,
        }
    }

    /// The entity-tag a refusal names: what the caller should have sent
    /// (`file-changed`, on the wire's `etag` header) or what is there
    /// now (`draft-stale`, on `x-flint-current-etag`).
    pub fn current_etag(&self) -> Option<&str> {
        match self {
            VerbError::FileChanged { current } | VerbError::DraftStale { current, .. } => {
                current.as_deref()
            }
            _ => None,
        }
    }

    /// True when the caller is expected to re-read and retry the same
    /// request (a conflict), false when the request itself is wrong.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            VerbError::WindowOpen { .. }
                | VerbError::ConcurrentWrite
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
pub fn path_ok(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.split('/').any(|seg| {
            seg.is_empty() || seg == "." || seg == ".." || seg == flint_lean::STATE_DIR
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
            window_wait: None,
        }
    }

    /// How long a HITL write (`put_file`, `promote_draft`) may wait for
    /// an open barrier window to close before it is refused
    /// `WindowOpen`. `None` (the default, and the HTTP gateway's
    /// behaviour) refuses at once with the `Retry-After` the caller
    /// would have used; a UI that would rather not see the 409 at all
    /// sets a bound here, and the wait happens BEFORE anything is
    /// written, so it changes nothing about what the write does.
    pub fn with_window_wait(mut self, wait: Option<std::time::Duration>) -> Self {
        self.window_wait = wait;
        self
    }

    /// Whole-object ceiling for HITL writes and draft saves (default
    /// 64 MiB, the gateway's `FLINT_LEAN_GW_MAX_PUT_MB`). Multipart
    /// through this door is deferred, so a larger body is refused.
    pub fn with_max_put_bytes(mut self, max: u64) -> Self {
        self.max_put_bytes = max;
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

    /// The window check every HITL write makes: read the CELL (never a
    /// replica's memory — the statelessness contract) and refuse while
    /// a barrier window is open, saying how long it has left.
    pub(crate) async fn admit_hitl(&self) -> Result<(), VerbError> {
        let deadline = self.window_wait.map(|w| tokio::time::Instant::now() + w);
        loop {
            let loaded = inbox::load(self.store.as_ref(), &self.cfg).await?;
            if inbox::admits_hitl(&loaded.doc) {
                return Ok(());
            }
            let retry_after_secs = loaded
                .doc
                .window
                .as_ref()
                .map(|w| w.deadline_unix.saturating_sub(now_unix()).max(1))
                .unwrap_or(2);
            match deadline {
                Some(d) if tokio::time::Instant::now() < d => {
                    // A window is short (one barrier's manifest CAS);
                    // re-read the cell often enough to notice it close.
                    let left = d.saturating_duration_since(tokio::time::Instant::now());
                    tokio::time::sleep(std::time::Duration::from_millis(250).min(left)).await;
                }
                _ => {
                    return Err(VerbError::WindowOpen {
                        retry_after_secs,
                        message: "a publish barrier is in flight; retry after the window".into(),
                    })
                }
            }
        }
    }

    /// Append the inbox entry that makes a landed object a TRACKED
    /// write. A window that opened between the admission check and
    /// this append refuses: the object landed (an unreferenced orphan)
    /// but the write is NOT acked — the caller retries and the retry
    /// re-PUTs over it.
    pub(crate) async fn track(&self, entry: InboxEntry) -> Result<(), VerbError> {
        match inbox::gateway_append(self.store.as_ref(), &self.cfg, entry).await {
            Ok(()) => Ok(()),
            Err(LeanError::State(message)) => {
                Err(VerbError::WindowOpen { retry_after_secs: 2, message })
            }
            Err(e) => Err(e.into()),
        }
    }

    // ── HITL / UI-facing ─────────────────────────────────────────────

    /// The HITL write: object PUT first, inbox entry second, NEVER a
    /// manifest edit. Returns the entity-tag the write produced.
    ///
    /// Refused `WindowOpen` while a live barrier window is open;
    /// `PreconditionRequired` for an overwrite that names nothing;
    /// `FileChanged` when what it names is not what is there.
    pub async fn put_file(
        &self,
        path: &str,
        body: Bytes,
        opts: &PutFile,
    ) -> Result<String, VerbError> {
        if !path_ok(path) {
            return Err(VerbError::BadPath(path.to_string()));
        }
        self.check_size(&body)?;
        self.admit_hitl().await?;

        // Object FIRST (fresh read → conditional PUT), inbox entry second.
        let key = self.cfg.file_key(path);
        let (current, prev_gen) = match self.store.head(&key).await {
            Ok(meta) => {
                let g = GenerationStamps::from_meta(&meta.meta).map(|s| s.generation).unwrap_or(0);
                (Some(meta.etag), g)
            }
            Err(StoreError::NotFound(_)) => (None, 0),
            Err(e) => return Err(e.into()),
        };

        // The CALLER's precondition, judged against what is there now.
        // The conditional PUT below still carries the freshly-read
        // etag, which is what closes the window between this HEAD and
        // that PUT — but it is no longer the ONLY guard, and that is
        // the point of this check: a stale caller is told 412 here
        // rather than winning.
        let cond = match judge_preconditions(
            current.as_deref(),
            opts.if_match.as_deref(),
            opts.if_none_match.as_deref(),
        )? {
            Some(etag) => PutCondition::IfMatch(etag),
            None => PutCondition::IfNoneMatchAny,
        };
        let crc = crc64_nvme(&body);
        let author = opts.author.clone().unwrap_or_else(|| "ui".into());
        let stamps = GenerationStamps {
            generation: prev_gen + 1,
            epoch: 0, // a HITL write carries no lease epoch — it is the second writer
            flush_uuid: format!("gateway-{}", uuid::Uuid::new_v4()),
            boundary_source: None,
            posix: None,
        };
        let meta = match self.store.put_whole(&key, body, &cond, &stamps, crc).await {
            Ok(m) => m,
            // NOT `FileChanged`: the caller's precondition held when it
            // was judged, and the object moved inside the HEAD-to-PUT
            // window. Retrying the same request can succeed, which is
            // the opposite of the advice a 412 carries.
            Err(StoreError::PreconditionFailed(_)) => return Err(VerbError::ConcurrentWrite),
            Err(e) => return Err(e.into()),
        };
        let entry = InboxEntry {
            path: path.to_string(),
            etag: meta.etag.clone(),
            author,
            added_unix: now_unix(),
            crc64_b64: Some(flint_store::crc64_to_b64(crc)),
        };
        self.track(entry).await?;
        Ok(meta.etag)
    }

    /// Read via the manifest citation, falling back to an uncited but
    /// tracked inbox entry (a HITL write no barrier has re-cited yet).
    ///
    /// Under `pinned_reads` the citation names a VERSION, and that is
    /// what a coherent read resolves — the same rule `checkout`
    /// follows, and for the same reason. Reading by etag alone breaks
    /// exactly when gating is doing its job: the upload lane makes the
    /// cited version noncurrent, so an If-Match GET against the current
    /// object fails its precondition and the human read path goes dark
    /// for the whole withholding window. Gated mode withholds
    /// VISIBILITY of new bytes; it never withholds the cited ones.
    pub async fn get_file(&self, path: &str) -> Result<Blob, VerbError> {
        if !path_ok(path) {
            return Err(VerbError::BadPath(path.to_string()));
        }
        let key = self.cfg.file_key(path);
        let (cited, pinned, sole_writer) =
            match manifest::load(self.store.as_ref(), &self.cfg).await? {
                Some(l) => (
                    l.manifest.entries.get(path).map(|e| (e.etag.clone(), e.version_id.clone())),
                    l.manifest.pinned_reads,
                    l.manifest.sole_writer,
                ),
                None => (None, false, false),
            };
        let pinned_version = match (pinned, cited.as_ref()) {
            (true, Some((_, Some(vid)))) => Some(vid.clone()),
            _ => None,
        };
        if let Some(vid) = pinned_version {
            return match self.store.get_version(&key, &vid).await {
                Ok((meta, body)) => Ok(Blob { etag: meta.etag, body }),
                // The dangling-citation endgame (D8): the backstop
                // reaped a cited noncurrent version. Say so — never
                // fall back to the current object, which is precisely
                // the uncited, possibly-mid-logical-change bytes gating
                // withholds.
                Err(StoreError::NotFound(_)) => Err(VerbError::DanglingCitation {
                    path: path.to_string(),
                    version_id: vid,
                }),
                Err(e) => Err(e.into()),
            };
        }
        let tracked = if let Some((etag, _)) = cited {
            Some(etag)
        } else {
            let l = inbox::load(self.store.as_ref(), &self.cfg).await?;
            l.doc.entries.iter().rev().find(|e| e.path == path).map(|e| e.etag.clone())
        };
        let Some(etag) = tracked else {
            return Err(VerbError::NoSuchFile(path.to_string()));
        };
        match self.store.get_whole(&key, Some(&etag)).await {
            Ok((meta, body)) => Ok(Blob { etag: meta.etag, body }),
            // A sole-writer workspace (forge's export) never has a
            // second legitimate writer, so "retry" is wrong: the cited
            // etag is not coming back on its own.
            Err(StoreError::PreconditionFailed(_)) if sole_writer => {
                Err(VerbError::ForeignWrite { path: path.to_string() })
            }
            // Under `pinned_reads` this is the mixed-manifest cell: an
            // entry the citation could not make version-addressable,
            // whose object has since moved. Retrying cannot fix it and
            // adopting the current version is exactly the uncited bytes
            // gating withholds. Say which it is, so a UI does not retry
            // forever.
            Err(StoreError::PreconditionFailed(_)) if pinned => {
                Err(VerbError::UncitedBytes { path: path.to_string() })
            }
            Err(StoreError::PreconditionFailed(_)) => Err(VerbError::Moved),
            Err(StoreError::NotFound(_)) => Err(VerbError::NoSuchFile(path.to_string())),
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

    /// Seq, window, inbox depth, the epoch cell, standing requests.
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
            window: ib.doc.window.clone(),
            inbox_depth: ib.doc.entries.len(),
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
    /// the manifest only when its seq moved. Returns the citing seq.
    /// Refuses at once with `CitationPending` when no syncer holds the
    /// lease — nothing is there to cite, and waiting would only run the
    /// clock down — and with the same error when `timeout` passes. A
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
                        // Cited, but not our bytes. Either a later write
                        // won, or ours was dropped as superseded before
                        // consume — the inbox says which, and it is not
                        // this verb's to guess.
                        let ours_pending = inbox::load(self.store.as_ref(), &self.cfg)
                            .await?
                            .doc
                            .entries
                            .iter()
                            .any(|i| i.path == path && normalize_etag(&i.etag) == want);
                        if !ours_pending {
                            return Err(VerbError::Superseded {
                                path: path.to_string(),
                                cited_etag: e.etag.clone(),
                            });
                        }
                    }
                }
            }
            // Nobody to cite: say so now rather than at the deadline.
            match self.store.epoch_read(&self.cfg.epoch_key()).await? {
                Some(cell) if !cell.released => {}
                _ => return Err(pending("no syncer holds this workspace's lease")),
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

    /// Mark the barrier window open (`{epoch, deadline_unix}`).
    pub async fn open_window(&self, epoch: u64, deadline_unix: u64) -> Result<(), VerbError> {
        self.require_current_epoch(epoch).await?;
        match inbox::open_window(self.store.as_ref(), &self.cfg, epoch, deadline_unix).await {
            Ok(_) => Ok(()),
            Err(LeanError::Fenced(m)) => Err(VerbError::Fenced(m)),
            Err(e) => Err(e.into()),
        }
    }

    /// Clear the window, re-queueing the entries the barrier did not
    /// consume.
    pub async fn clear_window(&self, epoch: u64, queued: &[InboxEntry]) -> Result<(), VerbError> {
        self.require_current_epoch(epoch).await?;
        match inbox::clear_window(self.store.as_ref(), &self.cfg, epoch, queued).await {
            Ok(()) => Ok(()),
            Err(LeanError::Fenced(m)) => Err(VerbError::Fenced(m)),
            Err(e) => Err(e.into()),
        }
    }

    /// Drop consumed entries from the inbox.
    pub async fn drop_inbox(&self, epoch: u64, consumed: &[InboxEntry]) -> Result<(), VerbError> {
        self.require_current_epoch(epoch).await?;
        inbox::drop_entries(self.store.as_ref(), &self.cfg, epoch, consumed).await?;
        Ok(())
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
    /// correctness beats throughput on it.
    pub async fn cas_manifest(
        &self,
        manifest: &LeanManifest,
        expected_etag: Option<&str>,
        epoch: u64,
        flush_uuid: &str,
    ) -> Result<String, VerbError> {
        self.require_current_epoch(epoch).await?;
        let legacy =
            matches!(manifest::load_pointer(self.store.as_ref(), &self.cfg).await, Ok(None));
        let handle = expected_etag.map(|e| manifest::ManifestHandle {
            etag: e.to_string(),
            legacy,
            prev_chunks: Vec::new(),
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
