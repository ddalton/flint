//! The lean gateway verbs (plan §2.2 / Phase 3): the CONTROL plane.
//!
//! Deliberately NOT `lite_gateway`: that module is the hub fleet's
//! door (FlintShare resolution, derived per-share tokens, reverse
//! proxy to hub pods). Lean's gateway talks to the BUCKET — the same
//! CAS cells the sidecar uses — and has no hub, no CR resolution, no
//! token minting. It shares only the crate, the image, and the warp
//! stack. Coupling the two would hand every future hub-side change
//! (strict mode included) a blast radius into lean; see the operator
//! note in `docs/plans/flint-lean-plan.md` §2.4.
//!
//! Verbs (all under `/lean/v1/{workspace}`; bearer-authenticated):
//!
//! UI/HITL-facing:
//! - `PUT  /files/{path}`  — the HITL write: object PUT first, inbox
//!   entry second, NEVER a manifest edit. Refused 409+Retry-After
//!   while a live barrier window is open (every replica reads the
//!   window from the CELL — the statelessness contract).
//! - `GET  /files/{path}`  — read via the manifest citation, falling
//!   back to an uncited-but-tracked inbox entry.
//! - `GET  /snapshot`      — {manifest, manifest_etag, inbox}: the
//!   sync verb's one-stop read.
//! - `GET  /status`        — seq/window/inbox depth/epoch cell: the
//!   RPO observability surface.
//!
//! Sidecar-facing (epoch-validated PER REQUEST — P5's teeth: a write
//! whose claimed epoch is not the cell's CURRENT epoch is rejected,
//! closing the deposed-straggler door the model's LeanNoEpochCheck
//! mutation proves rotation alone leaves open):
//! - `POST /window/open`   {epoch, deadline_unix}
//! - `POST /window/clear`  {epoch, queued: [entry]}
//! - `POST /inbox/drop`    {epoch, consumed: [entry]}
//! - `POST /manifest`      {manifest, expected_etag?, epoch, flush_uuid}
//!
//! v1 deliberate limits (recorded, not hidden): HITL writes are
//! whole-object ≤ the configured cap (multipart via the gateway is
//! deferred); one shared bearer (per-workspace tokens arrive with the
//! SigV4/TokenReview deferral); HITL deletes are not a verb yet.

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use warp::http::StatusCode;
use warp::{Filter, Reply};

use flint_store::{
    crc64_nvme, GenerationStamps, ObjectStore, PutCondition, StoreError,
};

use super::inbox::{self, InboxEntry};
use super::manifest::{self, LeanManifest};
use super::{now_unix, LeanConfig, LeanError};

pub struct GatewayCore {
    pub store: Arc<dyn ObjectStore>,
    /// workspace id -> subtree prefix (the tenancy map; project-granular
    /// per §9 Q6). Unknown ids are 404, never a guessed prefix.
    pub workspaces: BTreeMap<String, String>,
    /// The inbound bearer. The binary refuses to start without one.
    pub token: String,
    /// Whole-object ceiling for HITL PUTs.
    pub max_put_bytes: u64,
}

impl GatewayCore {
    fn cfg(&self, ws: &str) -> Option<LeanConfig> {
        // The gateway never touches a local tree; the root is unused.
        self.workspaces.get(ws).map(|p| LeanConfig::new(p, "/nonexistent"))
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    message: String,
}

fn err_reply(status: StatusCode, error: &str, message: String) -> warp::reply::Response {
    let mut res = warp::reply::with_status(
        warp::reply::json(&ErrorBody { error: error.into(), message }),
        status,
    )
    .into_response();
    if status == StatusCode::CONFLICT {
        // Callers poll; a default pacing hint beats a stampede.
        res.headers_mut().insert("retry-after", warp::http::HeaderValue::from_static("2"));
    }
    res
}

fn ok_json<T: Serialize>(v: &T) -> warp::reply::Response {
    warp::reply::json(v).into_response()
}

/// Constant-time-ish bearer compare (length + full fold, no early exit).
fn token_ok(expected: &str, header: Option<&str>) -> bool {
    let Some(h) = header else { return false };
    let Some(given) = h.strip_prefix("Bearer ") else { return false };
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Workspace-relative path hygiene: no traversal, no absolute, no
/// reserved namespaces, no empty segments.
fn path_ok(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.split('/').any(|seg| {
            seg.is_empty() || seg == "." || seg == ".." || seg == super::STATE_DIR
        })
        && !path.starts_with(".flint/")
        && path != ".flint"
}

/// Per-request epoch validation: the claimed epoch must be the cell's
/// CURRENT epoch. A deposed writer's stale epoch — or a claim over an
/// empty cell — is refused.
async fn require_current_epoch(
    core: &GatewayCore,
    cfg: &LeanConfig,
    claimed: u64,
) -> Result<(), warp::reply::Response> {
    match core.store.epoch_read(&cfg.epoch_key()).await {
        Ok(Some(state)) if state.epoch == claimed => Ok(()),
        Ok(Some(state)) => Err(err_reply(
            StatusCode::FORBIDDEN,
            "stale-epoch",
            format!("cell is at epoch {} (holder {}), request claims {}", state.epoch, state.holder_id, claimed),
        )),
        Ok(None) => Err(err_reply(
            StatusCode::FORBIDDEN,
            "no-holder",
            "no lease cell exists for this workspace".into(),
        )),
        Err(e) => Err(err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string())),
    }
}

// ── request bodies ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct WindowOpenReq {
    epoch: u64,
    deadline_unix: u64,
}

#[derive(Deserialize)]
struct WindowClearReq {
    epoch: u64,
    #[serde(default)]
    queued: Vec<InboxEntry>,
}

#[derive(Deserialize)]
struct InboxDropReq {
    epoch: u64,
    consumed: Vec<InboxEntry>,
}

#[derive(Deserialize)]
struct ManifestCasReq {
    manifest: LeanManifest,
    expected_etag: Option<String>,
    epoch: u64,
    flush_uuid: String,
}

#[derive(Serialize)]
struct EtagResp {
    etag: String,
}

// ── the router ───────────────────────────────────────────────────────

pub fn routes(
    core: Arc<GatewayCore>,
) -> warp::filters::BoxedFilter<(warp::reply::Response,)> {
    let with_core = {
        let core = core.clone();
        warp::any().map(move || core.clone())
    };

    // Auth wrapper: every /lean route demands the bearer.
    let authed = {
        let core = core.clone();
        warp::header::optional::<String>("authorization").and_then(move |h: Option<String>| {
            let core = core.clone();
            async move {
                if token_ok(&core.token, h.as_deref()) {
                    Ok(())
                } else {
                    Err(warp::reject::custom(Unauthorized))
                }
            }
        })
    };

    let files_put = warp::put()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "files" / ..))
        .and(warp::path::tail())
        .and(warp::header::optional::<String>("x-flint-author"))
        .and(warp::body::content_length_limit(core.max_put_bytes))
        .and(warp::body::bytes())
        .then(|_auth, core: Arc<GatewayCore>, ws: String, tail: warp::path::Tail, author, body| {
            handle_files_put(core, ws, tail.as_str().to_string(), author, body)
        });

    let files_get = warp::get()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "files" / ..))
        .and(warp::path::tail())
        .then(|_auth, core: Arc<GatewayCore>, ws: String, tail: warp::path::Tail| {
            handle_files_get(core, ws, tail.as_str().to_string())
        });

    let snapshot = warp::get()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "snapshot"))
        .then(handle_snapshot_authed);

    let status = warp::get()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "status"))
        .then(handle_status_authed);

    let window_open = warp::post()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "window" / "open"))
        .and(warp::body::json::<WindowOpenReq>())
        .then(handle_window_open);

    let window_clear = warp::post()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "window" / "clear"))
        .and(warp::body::json::<WindowClearReq>())
        .then(handle_window_clear);

    let inbox_drop = warp::post()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "inbox" / "drop"))
        .and(warp::body::json::<InboxDropReq>())
        .then(handle_inbox_drop);

    // §2.5's gateway door. Two verbs, deliberately asymmetric: a
    // boundary is PERFORMED by the sidecar, a sync is CARRIED to the
    // agent as advisory news (D14).
    let boundary_req = warp::post()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "boundary"))
        .and(warp::header::optional::<String>("x-flint-author"))
        .then(handle_boundary_request);

    let sync_req = warp::post()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "sync-request"))
        .and(warp::header::optional::<String>("x-flint-author"))
        .then(handle_sync_request);

    let manifest_cas = warp::post()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "manifest"))
        .and(warp::body::json::<ManifestCasReq>())
        .then(handle_manifest_cas);

    let healthz = warp::get()
        .and(warp::path!("healthz"))
        .map(|| warp::reply::with_status("ok", StatusCode::OK).into_response());

    files_put
        .or(files_get).unify()
        .or(snapshot).unify()
        .or(status).unify()
        .or(window_open).unify()
        .or(window_clear).unify()
        .or(inbox_drop).unify()
        .or(boundary_req).unify()
        .or(sync_req).unify()
        .or(manifest_cas).unify()
        .or(healthz).unify()
        .recover(recover_auth)
        .unify()
        .boxed()
}

#[derive(Debug)]
struct Unauthorized;
impl warp::reject::Reject for Unauthorized {}

async fn recover_auth(
    r: warp::Rejection,
) -> Result<warp::reply::Response, warp::Rejection> {
    if r.find::<Unauthorized>().is_some() {
        Ok(err_reply(StatusCode::UNAUTHORIZED, "unauthorized", "missing or wrong bearer".into()))
    } else {
        Err(r)
    }
}

// ── handlers ─────────────────────────────────────────────────────────

async fn handle_files_put(
    core: Arc<GatewayCore>,
    ws: String,
    path: String,
    author: Option<String>,
    body: Bytes,
) -> warp::reply::Response {
    let Some(cfg) = core.cfg(&ws) else {
        return err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws);
    };
    if !path_ok(&path) {
        return err_reply(StatusCode::BAD_REQUEST, "bad-path", path);
    }

    // The window check: every stateless replica reads the CELL.
    let loaded = match inbox::load(core.store.as_ref(), &cfg).await {
        Ok(l) => l,
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };
    if !inbox::admits_hitl(&loaded.doc) {
        let retry = loaded
            .doc
            .window
            .as_ref()
            .map(|w| w.deadline_unix.saturating_sub(now_unix()).max(1))
            .unwrap_or(2);
        let mut res = err_reply(
            StatusCode::CONFLICT,
            "barrier-window-open",
            "a publish barrier is in flight; retry after the window".into(),
        );
        if let Ok(v) = warp::http::HeaderValue::from_str(&retry.to_string()) {
            res.headers_mut().insert("retry-after", v);
        }
        return res;
    }

    // Object FIRST (fresh read → conditional PUT), inbox entry second.
    let key = cfg.file_key(&path);
    let (cond, prev_gen) = match core.store.head(&key).await {
        Ok(meta) => {
            let g = GenerationStamps::from_meta(&meta.meta).map(|s| s.generation).unwrap_or(0);
            (PutCondition::IfMatch(meta.etag), g)
        }
        Err(StoreError::NotFound(_)) => (PutCondition::IfNoneMatchAny, 0),
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };
    let crc = crc64_nvme(&body);
    let author = author.unwrap_or_else(|| "ui".into());
    let stamps = GenerationStamps {
        generation: prev_gen + 1,
        epoch: 0, // a gateway write carries no lease epoch — it is the second writer
        flush_uuid: format!("gateway-{}", uuid::Uuid::new_v4()),
        boundary_source: None,
        posix: None,
    };
    let meta = match core.store.put_whole(&key, body, &cond, &stamps, crc).await {
        Ok(m) => m,
        Err(StoreError::PreconditionFailed(_)) => {
            return err_reply(
                StatusCode::CONFLICT,
                "concurrent-write",
                "the object changed under this write; re-read and retry".into(),
            );
        }
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };
    let entry = InboxEntry {
        path: path.clone(),
        etag: meta.etag.clone(),
        author,
        added_unix: now_unix(),
    };
    match inbox::gateway_append(core.store.as_ref(), &cfg, entry).await {
        Ok(()) => ok_json(&EtagResp { etag: meta.etag }),
        // A window opened between our check and the append: the object
        // landed (an unreferenced orphan) but the write is NOT acked —
        // the caller retries and the retry re-PUTs over it.
        Err(LeanError::State(m)) => err_reply(StatusCode::CONFLICT, "barrier-window-open", m),
        Err(e) => err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    }
}

async fn handle_files_get(
    core: Arc<GatewayCore>,
    ws: String,
    path: String,
) -> warp::reply::Response {
    let Some(cfg) = core.cfg(&ws) else {
        return err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws);
    };
    if !path_ok(&path) {
        return err_reply(StatusCode::BAD_REQUEST, "bad-path", path);
    }
    let key = cfg.file_key(&path);

    // The manifest citation is the coherent view; an uncited inbox
    // entry (a HITL write no barrier has re-cited yet) is the fallback.
    //
    // Under `pinned_reads` the citation names a VERSION, and that is
    // what a coherent read resolves — the same rule `checkout` follows,
    // and for the same reason. Reading by etag alone breaks exactly
    // when gating is doing its job: the upload lane makes the cited
    // version noncurrent, so an If-Match GET against the current object
    // fails its precondition and the human read path goes dark for the
    // whole withholding window. Gated mode withholds VISIBILITY of new
    // bytes; it never withholds the cited ones.
    let (cited, pinned) = match manifest::load(core.store.as_ref(), &cfg).await {
        Ok(Some(l)) => (
            l.manifest.entries.get(&path).map(|e| (e.etag.clone(), e.version_id.clone())),
            l.manifest.pinned_reads,
        ),
        Ok(None) => (None, false),
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };
    let pinned_version = match (pinned, cited.as_ref()) {
        (true, Some((_, Some(vid)))) => Some(vid.clone()),
        _ => None,
    };
    if let Some(vid) = pinned_version {
        return match core.store.get_version(&key, &vid).await {
            Ok((meta, body)) => {
                let mut res = warp::reply::Response::new(body.into());
                res.headers_mut()
                    .insert("etag", warp::http::HeaderValue::from_str(&meta.etag).unwrap());
                res
            }
            // The dangling-citation endgame (D8): the backstop reaped a
            // cited noncurrent version. Say so — never fall back to the
            // current object, which is precisely the uncited,
            // possibly-mid-logical-change bytes gating withholds.
            Err(StoreError::NotFound(_)) => err_reply(
                StatusCode::GONE,
                "dangling-citation",
                format!(
                    "the manifest cites {path} version {vid} but that version is gone; \
                     run `flint-sync recover-staged` to re-cite forward"
                ),
            ),
            Err(e) => err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
        };
    }
    let tracked = if let Some((etag, _)) = cited {
        Some(etag)
    } else {
        match inbox::load(core.store.as_ref(), &cfg).await {
            Ok(l) => l.doc.entries.iter().rev().find(|e| e.path == path).map(|e| e.etag.clone()),
            Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
        }
    };
    let Some(etag) = tracked else {
        return err_reply(StatusCode::NOT_FOUND, "no-such-file", path);
    };
    match core.store.get_whole(&key, Some(&etag)).await {
        Ok((meta, body)) => {
            let mut res = warp::reply::Response::new(body.into());
            res.headers_mut()
                .insert("etag", warp::http::HeaderValue::from_str(&meta.etag).unwrap());
            res
        }
        // Under `pinned_reads` this is the mixed-manifest cell: an entry
        // the citation could not make version-addressable, whose object
        // has since moved. Retrying cannot fix it — the cited etag will
        // never come back — and adopting the current version is exactly
        // the uncited, possibly mid-logical-change bytes gating
        // withholds. Say which it is, so a UI does not retry forever.
        Err(StoreError::PreconditionFailed(_)) if pinned => err_reply(
            StatusCode::GONE,
            "uncited-bytes",
            format!(
                "the manifest cites {path} at an etag the object no longer carries and names \
                 no version to resolve instead; run `flint-sync recover-staged` to re-cite forward"
            ),
        ),
        Err(StoreError::PreconditionFailed(_)) => err_reply(
            StatusCode::CONFLICT,
            "moved",
            "the object moved past the tracked version; retry".into(),
        ),
        Err(StoreError::NotFound(_)) => err_reply(StatusCode::NOT_FOUND, "no-such-file", path),
        Err(e) => err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    }
}

async fn handle_snapshot_authed(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
) -> warp::reply::Response {
    let Some(cfg) = core.cfg(&ws) else {
        return err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws);
    };
    let m = match manifest::load(core.store.as_ref(), &cfg).await {
        Ok(m) => m,
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };
    let ib = match inbox::load(core.store.as_ref(), &cfg).await {
        Ok(l) => l,
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };
    #[derive(Serialize)]
    struct Snapshot {
        manifest: LeanManifest,
        manifest_etag: Option<String>,
        inbox: super::inbox::InboxDoc,
    }
    let (manifest, manifest_etag) = match m {
        Some(l) => (l.manifest, Some(l.etag)),
        None => (Default::default(), None),
    };
    ok_json(&Snapshot { manifest, manifest_etag, inbox: ib.doc })
}

async fn handle_status_authed(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
) -> warp::reply::Response {
    let Some(cfg) = core.cfg(&ws) else {
        return err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws);
    };
    // ONE manifest request for all four manifest-derived fields, and it
    // is a HEAD: /status reports scalars — seq, the stamp, the source —
    // and NOT ONE ENTRY, so downloading and parsing a document that runs
    // to ~66 MiB at the 250k cap bought nothing. Every field it needs
    // rides the object stamps `cas_write_stamped` already writes
    // (`generation` IS the seq), and the request COUNT is unchanged at
    // three — U31 was about a HEAD *in addition to* the GET, not
    // instead of it.
    //
    // This is only sound because the stamp and the document agree by
    // construction; `cas_write_stamped` stamps the document's own
    // `boundary_source` precisely so a HEAD reader and a GET reader
    // cannot disagree.
    let (seq, stamp_unix, boundary_source) =
        match core.store.head(&cfg.manifest_key()).await {
            Ok(meta) => {
                let stamps = GenerationStamps::from_meta(&meta.meta);
                (
                    stamps.as_ref().map(|s| s.generation),
                    meta.last_modified_unix,
                    stamps.and_then(|s| s.boundary_source),
                )
            }
            Err(StoreError::NotFound(_)) => (None, None, None),
            Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
        };
    let ib = match inbox::load(core.store.as_ref(), &cfg).await {
        Ok(l) => l,
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };
    let cell = match core.store.epoch_read(&cfg.epoch_key()).await {
        Ok(c) => c,
        Err(e) => return err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    };
    #[derive(Serialize)]
    struct Status {
        seq: Option<u64>,
        window: Option<super::inbox::Window>,
        inbox_depth: usize,
        epoch: Option<u64>,
        holder_id: Option<String>,
        holder_released: Option<bool>,
        now_unix: u64,
        /// The last CITED manifest seq — under gated mode this is the
        /// coherent view, not the newest bytes in the bucket.
        last_cited_seq: Option<u64>,
        manifest_stamp_unix: Option<u64>,
        /// Which coherent point installed it: `sentinel`, `quiescence`,
        /// `forced-lag-cap`, `drain`, `recovered`… A reader that cares
        /// whether the view it is about to take was DECLARED coherent or
        /// forced by a cap can tell, from the bucket.
        boundary_source: Option<String>,
        /// Whether a boundary/sync request is standing (§2.5).
        boundary_request: Option<super::inbox::VerbRequest>,
        sync_request: Option<super::inbox::VerbRequest>,
    }
    ok_json(&Status {
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

/// `POST /lean/v1/{ws}/boundary` — ask the workspace to publish.
///
/// The gateway does not publish anything itself and holds no epoch: it
/// sets a field, and the sidecar performs the barrier under its own
/// lease, min-interval and budget. That is what keeps a leaked bearer
/// from turning into an unbounded publish loop, and what keeps this
/// endpoint honest about what it can promise — the response says the
/// request was RECORDED, never that a boundary happened.
async fn handle_boundary_request(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
    author: Option<String>,
) -> warp::reply::Response {
    handle_verb_request(core, ws, author, super::inbox::RequestedVerb::Boundary).await
}

/// `POST /lean/v1/{ws}/sync-request` — ask the workspace to pull.
///
/// CARRIED, never performed (D14): the sidecar copies it into
/// `.flint/remote.seq` and stops. `sync` deletes local files for
/// remotely-deleted paths, so performing it on a remote's say-so would
/// upgrade a leaked bearer from "publish, plus hand over these N named
/// objects" to "rewrite and delete across a running agent's tree, at my
/// timing, under a scope I choose".
async fn handle_sync_request(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
    author: Option<String>,
) -> warp::reply::Response {
    handle_verb_request(core, ws, author, super::inbox::RequestedVerb::Sync).await
}

async fn handle_verb_request(
    core: Arc<GatewayCore>,
    ws: String,
    author: Option<String>,
    verb: super::inbox::RequestedVerb,
) -> warp::reply::Response {
    let Some(cfg) = core.cfg(&ws) else {
        return err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws);
    };
    let requestor = author.unwrap_or_else(|| "gateway".into());
    match super::inbox::gateway_request(core.store.as_ref(), &cfg, verb, &requestor).await {
        Ok(req) => {
            #[derive(Serialize)]
            struct Accepted {
                /// "recorded", never "done": the sidecar decides when.
                status: &'static str,
                verb: &'static str,
                requested_unix: u64,
                requestor: String,
                note: &'static str,
            }
            ok_json(&Accepted {
                status: "recorded",
                verb: match verb {
                    super::inbox::RequestedVerb::Boundary => "boundary",
                    super::inbox::RequestedVerb::Sync => "sync",
                },
                requested_unix: req.requested_unix,
                requestor: req.requestor,
                note: match verb {
                    super::inbox::RequestedVerb::Boundary =>
                        "the sidecar honors this as a publish sentinel at its next tick, \
                         subject to the same min-interval and hourly budget; the ack lands \
                         in .flint/publish.ack",
                    super::inbox::RequestedVerb::Sync =>
                        "CARRIED, not executed: the sidecar moves .flint/remote.seq and the \
                         agent decides whether to sync",
                },
            })
        }
        Err(e) => err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    }
}

async fn handle_window_open(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
    req: WindowOpenReq,
) -> warp::reply::Response {
    let Some(cfg) = core.cfg(&ws) else {
        return err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws);
    };
    if let Err(res) = require_current_epoch(&core, &cfg, req.epoch).await {
        return res;
    }
    match inbox::open_window(core.store.as_ref(), &cfg, req.epoch, req.deadline_unix).await {
        Ok(_) => ok_json(&serde_json::json!({"open": true})),
        Err(LeanError::Fenced(m)) => err_reply(StatusCode::FORBIDDEN, "fenced", m),
        Err(e) => err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    }
}

async fn handle_window_clear(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
    req: WindowClearReq,
) -> warp::reply::Response {
    let Some(cfg) = core.cfg(&ws) else {
        return err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws);
    };
    if let Err(res) = require_current_epoch(&core, &cfg, req.epoch).await {
        return res;
    }
    match inbox::clear_window(core.store.as_ref(), &cfg, req.epoch, &req.queued).await {
        Ok(()) => ok_json(&serde_json::json!({"cleared": true})),
        Err(LeanError::Fenced(m)) => err_reply(StatusCode::FORBIDDEN, "fenced", m),
        Err(e) => err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    }
}

async fn handle_inbox_drop(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
    req: InboxDropReq,
) -> warp::reply::Response {
    let Some(cfg) = core.cfg(&ws) else {
        return err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws);
    };
    if let Err(res) = require_current_epoch(&core, &cfg, req.epoch).await {
        return res;
    }
    match inbox::drop_entries(core.store.as_ref(), &cfg, req.epoch, &req.consumed).await {
        Ok(()) => ok_json(&serde_json::json!({"dropped": true})),
        Err(e) => err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    }
}

async fn handle_manifest_cas(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
    req: ManifestCasReq,
) -> warp::reply::Response {
    let Some(cfg) = core.cfg(&ws) else {
        return err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws);
    };
    // P5: per-request epoch validation on the manifest path — the
    // straggler's CAS dies HERE even if rotation were absent.
    if let Err(res) = require_current_epoch(&core, &cfg, req.epoch).await {
        return res;
    }
    match manifest::cas_write(
        core.store.as_ref(),
        &cfg,
        &req.manifest,
        req.expected_etag.as_deref(),
        req.epoch,
        &req.flush_uuid,
    )
    .await
    {
        Ok(meta) => ok_json(&EtagResp { etag: meta.etag }),
        Err(LeanError::Store(StoreError::PreconditionFailed(_)))
        | Err(LeanError::Store(StoreError::Conflict(_))) => {
            let current = manifest::load(core.store.as_ref(), &cfg)
                .await
                .ok()
                .flatten()
                .map(|l| l.etag);
            err_reply(
                StatusCode::CONFLICT,
                "cas-miss",
                format!("current manifest etag: {current:?}"),
            )
        }
        Err(e) => err_reply(StatusCode::BAD_GATEWAY, "store", e.to_string()),
    }
}
