//! The lean gateway verbs (plan §2.2 / Phase 3): the CONTROL plane,
//! over HTTP.
//!
//! This module is the transport ONLY. Every verb is a method on
//! `workspace::Workspace` (files, snapshot, status, verb requests, the
//! syncer-facing verbs) or on it via `drafts.rs`; a handler here
//! resolves the workspace id, calls one method, and maps the result to
//! the wire — `reply_err` is the ONE place a `VerbError` becomes a
//! status, an `error` code and headers. The `flint-lean-gateway`
//! binary is `main` around `routes`; a process that would rather not
//! run it calls the same methods through this crate's library.
//!
//! Deliberately NOT `lite_gateway`: that module is the hub fleet's
//! door (FlintShare resolution, derived per-share tokens, reverse
//! proxy to hub pods). Lean's gateway talks to the BUCKET — the same
//! CAS cells the syncer uses — and has no hub, no CR resolution, no
//! token minting. It shares only the image and the warp stack.
//! Coupling the two would hand every future hub-side change (strict
//! mode included) a blast radius into lean; see the operator note in
//! `docs/plans/flint-lean-plan.md` §2.4.
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
//! - `DELETE /files/{path}` — record a DECLARED removal (delete/rename
//!   design): the syncer performs it at its next barrier. `If-Match`
//!   optional. 204 as soon as the intent is durable.
//! - `POST /rename` {from, to} — server-side copy, then one CAS with
//!   the destination entry and the source removal; `{etag}`.
//! - `DELETE /removals/{path}` — withdraw a recorded removal.
//! - `PUT  /drafts/{user}/{path}` — save a DURABLE UNPUBLISHED edit
//!   (`drafts.rs`). Unlike `PUT /files`, nothing about this is live:
//!   the bytes sit under the reserved namespace where no scan, no
//!   checkout, no manifest and no sweep can see them.
//! - `GET  /drafts/{user}` — the resume view, each row flagged `stale`
//!   when the file has moved since the draft was taken.
//! - `GET  /drafts/{user}/{path}` — the saved bytes.
//! - `POST /drafts/{user}/{path}` — PROMOTE: publish the draft,
//!   conditioned on the base it recorded. POST is the verb and there is
//!   no `/promote` suffix — see the router note.
//! - `DELETE /drafts/{user}/{path}` — discard.
//! - `GET  /snapshot`      — {manifest, manifest_etag, inbox}: the
//!   sync verb's one-stop read.
//! - `GET  /status`        — seq/window/inbox depth/epoch cell: the
//!   RPO observability surface.
//! - `POST /boundary`, `POST /sync-request` — §2.5's door: a boundary
//!   is PERFORMED by the syncer, a sync is CARRIED to the agent as
//!   advisory news (D14). Both answer "recorded", never "done".
//!
//! Syncer-facing (epoch-validated PER REQUEST — P5's teeth: a write
//! whose claimed epoch is not the cell's CURRENT epoch is rejected,
//! closing the deposed-straggler door the model's LeanNoEpochCheck
//! mutation proves rotation alone leaves open):
//! - `POST /window/open`   {epoch, deadline_unix}
//! - `POST /window/clear`  {epoch, queued: [entry]}
//! - `POST /inbox/drop`    {epoch, consumed: [entry]}
//! - `POST /manifest`      {manifest, expected_etag?, epoch, flush_uuid}
//!
//! NOT a verb, deliberately: there is no gateway-triggered `rescope`.
//! A rescope UNLINKS local files by scope, so honouring one on a
//! remote's say-so would upgrade what a leaked bearer can do to
//! "delete across a running agent's tree, under a scope I choose" —
//! D14's argument against performing a remote `sync`, with more force.
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

use flint_store::ObjectStore;

use crate::drafts::DraftRow;
use flint_lean::inbox::InboxEntry;
use flint_lean::manifest::LeanManifest;
use crate::workspace::{PutFile, VerbError, Workspace};

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
    /// The workspace behind an id, or `None` for an id the tenancy map
    /// does not name.
    pub fn workspace(&self, ws: &str) -> Option<Workspace> {
        self.workspaces
            .get(ws)
            .map(|p| Workspace::new(self.store.clone(), p).with_max_put_bytes(self.max_put_bytes))
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    message: String,
}

pub(crate) fn err_reply(status: StatusCode, error: &str, message: String) -> warp::reply::Response {
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

pub(crate) fn ok_json<T: Serialize>(v: &T) -> warp::reply::Response {
    warp::reply::json(v).into_response()
}

fn set_header(res: &mut warp::reply::Response, name: &'static str, value: &str) {
    if let Ok(v) = warp::http::HeaderValue::from_str(value) {
        res.headers_mut().insert(name, v);
    }
}

/// The ONE mapping from a verb's refusal to the wire: the status and
/// `error` code the error names for itself, plus the headers a caller
/// acts on — `Retry-After` on a conflict, `etag` on a `file-changed`
/// (a UI retries from the header, a human reads the message),
/// `x-flint-current-etag` on a `draft-stale`.
pub(crate) fn reply_err(e: VerbError) -> warp::reply::Response {
    let status = StatusCode::from_u16(e.status()).expect("every VerbError status is a valid code");
    let mut res = err_reply(status, e.code(), e.to_string());
    if let Some(secs) = e.retry_after_secs() {
        set_header(&mut res, "retry-after", &secs.to_string());
    }
    match &e {
        VerbError::FileChanged { current: Some(c) } => set_header(&mut res, "etag", c),
        VerbError::DraftStale { current: Some(c), .. } => {
            set_header(&mut res, "x-flint-current-etag", c)
        }
        _ => {}
    }
    res
}

fn unknown_workspace(ws: String) -> warp::reply::Response {
    err_reply(StatusCode::NOT_FOUND, "unknown-workspace", ws)
}

fn body_reply(etag: &str, body: Bytes) -> warp::reply::Response {
    let mut res = warp::reply::Response::new(body.into());
    set_header(&mut res, "etag", etag);
    res
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

#[derive(Deserialize)]
struct RenameReq {
    from: String,
    to: String,
}

#[derive(Serialize)]
pub(crate) struct EtagResp {
    pub(crate) etag: String,
}

#[derive(Serialize)]
struct DraftList {
    drafts: Vec<DraftRow>,
}

// ── the router ───────────────────────────────────────────────────────

/// Every route is `.boxed()` as it is built, and that is a COMPILE-TIME
/// requirement, not tidiness. warp composes filters in the type system:
/// each `.or()` wraps the pair in `Or<A, B>` and each `.then()` adds an
/// opaque future, so an unboxed chain's type grows combinatorially in
/// the number of routes. At 8 routes this crate's lib compiled in
/// minutes; adding the five draft routes took ONE rustc invocation on
/// `lib.rs` past 31 minutes at 100% CPU — all of it in type checking,
/// with no error and no end in sight. Boxing erases the type at each
/// step, so the chain composes `BoxedFilter` with `BoxedFilter` and the
/// cost is linear. Add a route WITH its `.boxed()`.
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
        .and(warp::header::optional::<String>("if-match"))
        .and(warp::header::optional::<String>("if-none-match"))
        .and(warp::body::content_length_limit(core.max_put_bytes))
        .and(warp::body::bytes())
        .then(
            |_auth,
             core: Arc<GatewayCore>,
             ws: String,
             tail: warp::path::Tail,
             author,
             if_match,
             if_none_match,
             body| {
                handle_files_put(
                    core,
                    ws,
                    tail.as_str().to_string(),
                    author,
                    if_match,
                    if_none_match,
                    body,
                )
            },
        ).boxed();

    let files_get = warp::get()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "files" / ..))
        .and(warp::path::tail())
        .then(|_auth, core: Arc<GatewayCore>, ws: String, tail: warp::path::Tail| {
            handle_files_get(core, ws, tail.as_str().to_string())
        }).boxed();

    let files_delete = warp::delete()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "files" / ..))
        .and(warp::path::tail())
        .and(warp::header::optional::<String>("x-flint-author"))
        .and(warp::header::optional::<String>("if-match"))
        .then(
            |_auth, core: Arc<GatewayCore>, ws: String, tail: warp::path::Tail, author, if_match| {
                handle_files_delete(core, ws, tail.as_str().to_string(), author, if_match)
            },
        ).boxed();

    let rename = warp::post()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "rename"))
        .and(warp::header::optional::<String>("x-flint-author"))
        .and(warp::body::json::<RenameReq>())
        .then(handle_rename).boxed();

    let removal_withdraw = warp::delete()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "removals" / ..))
        .and(warp::path::tail())
        .then(|_auth, core: Arc<GatewayCore>, ws: String, tail: warp::path::Tail| {
            handle_removal_withdraw(core, ws, tail.as_str().to_string())
        }).boxed();

    // ── drafts (`drafts.rs`) ─────────────────────────────────────────
    //
    // Method-keyed, with the workspace path in the tail and NO verb
    // suffix. A `POST .../drafts/{u}/{path}/promote` would be genuinely
    // ambiguous — a file legally named `promote` makes
    // `notes/promote` both "promote the draft of notes" and "the draft
    // of notes/promote" — and there is no disambiguation that does not
    // reserve a filename. POST-is-promote reserves nothing.
    let draft_list = warp::get()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "drafts" / String))
        .then(handle_draft_list).boxed();

    let draft_put = warp::put()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "drafts" / String / ..))
        .and(warp::path::tail())
        .and(warp::header::optional::<String>("x-flint-author"))
        .and(warp::header::optional::<String>("x-flint-base-etag"))
        .and(warp::body::content_length_limit(core.max_put_bytes))
        .and(warp::body::bytes())
        .then(
            |_auth,
             core: Arc<GatewayCore>,
             ws: String,
             user: String,
             tail: warp::path::Tail,
             author,
             base_etag,
             body| {
                handle_draft_put(core, ws, user, tail.as_str().to_string(), author, base_etag, body)
            },
        ).boxed();

    let draft_get = warp::get()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "drafts" / String / ..))
        .and(warp::path::tail())
        .then(
            |_auth, core: Arc<GatewayCore>, ws: String, user: String, tail: warp::path::Tail| {
                handle_draft_get(core, ws, user, tail.as_str().to_string())
            },
        ).boxed();

    let draft_delete = warp::delete()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "drafts" / String / ..))
        .and(warp::path::tail())
        .then(
            |_auth, core: Arc<GatewayCore>, ws: String, user: String, tail: warp::path::Tail| {
                handle_draft_delete(core, ws, user, tail.as_str().to_string())
            },
        ).boxed();

    let draft_promote = warp::post()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "drafts" / String / ..))
        .and(warp::path::tail())
        .and(warp::header::optional::<String>("x-flint-author"))
        .then(
            |_auth,
             core: Arc<GatewayCore>,
             ws: String,
             user: String,
             tail: warp::path::Tail,
             author| {
                handle_draft_promote(core, ws, user, tail.as_str().to_string(), author)
            },
        ).boxed();

    let snapshot = warp::get()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "snapshot"))
        .then(handle_snapshot).boxed();

    let status = warp::get()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "status"))
        .then(handle_status).boxed();

    let window_open = warp::post()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "window" / "open"))
        .and(warp::body::json::<WindowOpenReq>())
        .then(handle_window_open).boxed();

    let window_clear = warp::post()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "window" / "clear"))
        .and(warp::body::json::<WindowClearReq>())
        .then(handle_window_clear).boxed();

    let inbox_drop = warp::post()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "inbox" / "drop"))
        .and(warp::body::json::<InboxDropReq>())
        .then(handle_inbox_drop).boxed();

    // §2.5's gateway door. Two verbs, deliberately asymmetric: a
    // boundary is PERFORMED by the syncer, a sync is CARRIED to the
    // agent as advisory news (D14).
    let boundary_req = warp::post()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "boundary"))
        .and(warp::header::optional::<String>("x-flint-author"))
        .then(handle_boundary_request).boxed();

    let sync_req = warp::post()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "sync-request"))
        .and(warp::header::optional::<String>("x-flint-author"))
        .then(handle_sync_request).boxed();

    let manifest_cas = warp::post()
        .and(authed.clone())
        .and(with_core.clone())
        .and(warp::path!("lean" / "v1" / String / "manifest"))
        .and(warp::body::json::<ManifestCasReq>())
        .then(handle_manifest_cas).boxed();

    let healthz = warp::get()
        .and(warp::path!("healthz"))
        .map(|| warp::reply::with_status("ok", StatusCode::OK).into_response())
        .boxed();

    files_put
        .or(files_get).unify()
        .or(files_delete).unify()
        .or(rename).unify()
        .or(removal_withdraw).unify()
        // The exact-match list route goes BEFORE the tail routes: a
        // tail filter matches `/drafts/{u}` with an EMPTY tail, which
        // `path_ok` then refuses as a bad path instead of listing.
        .or(draft_list).unify()
        .or(draft_put).unify()
        .or(draft_get).unify()
        .or(draft_delete).unify()
        .or(draft_promote).unify()
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

// ── handlers: resolve the workspace, call the verb, map the result ───

async fn handle_files_put(
    core: Arc<GatewayCore>,
    ws: String,
    path: String,
    author: Option<String>,
    if_match: Option<String>,
    if_none_match: Option<String>,
    body: Bytes,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.put_file(&path, body, &PutFile { author, if_match, if_none_match }).await {
        Ok(etag) => ok_json(&EtagResp { etag }),
        Err(e) => reply_err(e),
    }
}

async fn handle_files_get(
    core: Arc<GatewayCore>,
    ws: String,
    path: String,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.get_file(&path).await {
        Ok(blob) => body_reply(&blob.etag, blob.body),
        Err(e) => reply_err(e),
    }
}

async fn handle_files_delete(
    core: Arc<GatewayCore>,
    ws: String,
    path: String,
    author: Option<String>,
    if_match: Option<String>,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.remove_file(&path, author.as_deref(), if_match.as_deref()).await {
        Ok(()) => warp::reply::with_status("", StatusCode::NO_CONTENT).into_response(),
        Err(e) => reply_err(e),
    }
}

async fn handle_rename(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
    author: Option<String>,
    req: RenameReq,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.rename_file(&req.from, &req.to, author.as_deref()).await {
        Ok(etag) => ok_json(&EtagResp { etag }),
        Err(e) => reply_err(e),
    }
}

async fn handle_removal_withdraw(
    core: Arc<GatewayCore>,
    ws: String,
    path: String,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.withdraw_removal(&path).await {
        Ok(()) => warp::reply::with_status("", StatusCode::NO_CONTENT).into_response(),
        Err(e) => reply_err(e),
    }
}

async fn handle_draft_put(
    core: Arc<GatewayCore>,
    ws: String,
    user: String,
    path: String,
    author: Option<String>,
    base_etag: Option<String>,
    body: Bytes,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.put_draft(&user, &path, body, author.as_deref(), base_etag.as_deref()).await {
        Ok(etag) => ok_json(&EtagResp { etag }),
        Err(e) => reply_err(e),
    }
}

async fn handle_draft_get(
    core: Arc<GatewayCore>,
    ws: String,
    user: String,
    path: String,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.get_draft(&user, &path).await {
        Ok(d) => {
            let mut res = body_reply(&d.etag, d.body);
            if let Some(b) = d.base_etag.as_deref() {
                set_header(&mut res, "x-flint-base-etag", b);
            }
            // An incomplete draft is readable, but the caller is told,
            // because promote will refuse it.
            if d.incomplete {
                set_header(&mut res, "x-flint-draft-incomplete", "1");
            }
            res
        }
        Err(e) => reply_err(e),
    }
}

async fn handle_draft_list(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
    user: String,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.list_drafts(&user).await {
        Ok(drafts) => ok_json(&DraftList { drafts }),
        Err(e) => reply_err(e),
    }
}

async fn handle_draft_delete(
    core: Arc<GatewayCore>,
    ws: String,
    user: String,
    path: String,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.delete_draft(&user, &path).await {
        Ok(()) => warp::reply::with_status("", StatusCode::NO_CONTENT).into_response(),
        Err(e) => reply_err(e),
    }
}

async fn handle_draft_promote(
    core: Arc<GatewayCore>,
    ws: String,
    user: String,
    path: String,
    author: Option<String>,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.promote_draft(&user, &path, author.as_deref()).await {
        Ok(etag) => ok_json(&EtagResp { etag }),
        Err(e) => reply_err(e),
    }
}

async fn handle_snapshot(_auth: (), core: Arc<GatewayCore>, ws: String) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.snapshot().await {
        Ok(s) => ok_json(&s),
        Err(e) => reply_err(e),
    }
}

async fn handle_status(_auth: (), core: Arc<GatewayCore>, ws: String) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.status().await {
        Ok(s) => ok_json(&s),
        Err(e) => reply_err(e),
    }
}

/// `POST /lean/v1/{ws}/boundary` — ask the workspace to publish. The
/// response says the request was RECORDED, never that a boundary
/// happened (`Workspace::request_boundary`).
async fn handle_boundary_request(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
    author: Option<String>,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.request_boundary(author.as_deref()).await {
        Ok(a) => ok_json(&a),
        Err(e) => reply_err(e),
    }
}

/// `POST /lean/v1/{ws}/sync-request` — ask the workspace to pull.
/// CARRIED, never performed (D14; `Workspace::request_sync`).
async fn handle_sync_request(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
    author: Option<String>,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.request_sync(author.as_deref()).await {
        Ok(a) => ok_json(&a),
        Err(e) => reply_err(e),
    }
}

async fn handle_window_open(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
    req: WindowOpenReq,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.open_window(req.epoch, req.deadline_unix).await {
        Ok(()) => ok_json(&serde_json::json!({"open": true})),
        Err(e) => reply_err(e),
    }
}

async fn handle_window_clear(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
    req: WindowClearReq,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.clear_window(req.epoch, &req.queued).await {
        Ok(()) => ok_json(&serde_json::json!({"cleared": true})),
        Err(e) => reply_err(e),
    }
}

async fn handle_inbox_drop(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
    req: InboxDropReq,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w.drop_inbox(req.epoch, &req.consumed).await {
        Ok(()) => ok_json(&serde_json::json!({"dropped": true})),
        Err(e) => reply_err(e),
    }
}

async fn handle_manifest_cas(
    _auth: (),
    core: Arc<GatewayCore>,
    ws: String,
    req: ManifestCasReq,
) -> warp::reply::Response {
    let Some(w) = core.workspace(&ws) else { return unknown_workspace(ws) };
    match w
        .cas_manifest(&req.manifest, req.expected_etag.as_deref(), req.epoch, &req.flush_uuid)
        .await
    {
        Ok(etag) => ok_json(&EtagResp { etag }),
        Err(e) => reply_err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flint_lean::LeanError;

    /// The wire a `VerbError` maps to, pinned as a table: a variant
    /// that changes its status or code changes what every frontend
    /// built against the gateway sees, and this is where that shows.
    #[tokio::test]
    #[allow(clippy::type_complexity)]
    async fn every_refusal_keeps_the_status_code_and_headers_it_shipped_with() {
        let store_err = || VerbError::Store(LeanError::State("x".into()));
        let cases: Vec<(VerbError, u16, &str, Option<u64>, Option<&str>)> = vec![
            (VerbError::BadPath("a/../b".into()), 400, "bad-path", None, None),
            (VerbError::BadUser("".into()), 400, "bad-user", None, None),
            (VerbError::BadPrecondition("x"), 400, "bad-precondition", None, None),
            (VerbError::PreconditionRequired, 428, "precondition-required", None, None),
            (VerbError::FileChanged { current: Some("\"e1\"".into()) }, 412, "file-changed", None, Some("\"e1\"")),
            (VerbError::FileChanged { current: None }, 412, "file-changed", None, None),
            (VerbError::WindowOpen { retry_after_secs: 17, message: "w".into() }, 409, "barrier-window-open", Some(17), None),
            (VerbError::ConcurrentWrite, 409, "concurrent-write", Some(2), None),
            (VerbError::Moved, 409, "moved", Some(2), None),
            (VerbError::NoSuchFile("p".into()), 404, "no-such-file", None, None),
            (VerbError::ForeignWrite { path: "p".into() }, 410, "foreign-write", None, None),
            (VerbError::TooLarge { size: 2, max: 1 }, 413, "payload-too-large", None, None),
            (VerbError::DestinationExists { path: "p".into(), current: Some("\"e3\"".into()) }, 409, "destination-exists", Some(2), None),
            (VerbError::NoRemoval("p".into()), 404, "no-removal", None, None),
            (VerbError::NoDraft("d".into()), 404, "no-draft", None, None),
            (VerbError::DraftStale { current: Some("\"e2\"".into()), message: "s".into() }, 409, "draft-stale", Some(2), Some("\"e2\"")),
            (VerbError::DraftMoved("p".into()), 409, "draft-moved", Some(2), None),
            (VerbError::StaleEpoch { cell_epoch: 3, holder_id: "h".into(), claimed: 2 }, 403, "stale-epoch", None, None),
            (VerbError::NoHolder, 403, "no-holder", None, None),
            (VerbError::Fenced("f".into()), 403, "fenced", None, None),
            (VerbError::CasMiss { current: None }, 409, "cas-miss", Some(2), None),
            (VerbError::CitationPending { path: "p".into(), etag: "e".into(), reason: "r".into() }, 202, "citation-pending", None, None),
            (VerbError::Superseded { path: "p".into(), cited_etag: "e".into() }, 409, "superseded", Some(2), None),
            (VerbError::Encode("e".into()), 500, "encode", None, None),
            (store_err(), 502, "store", None, None),
        ];
        for (e, status, code, retry, etag) in cases {
            let message = e.to_string();
            let is_file_changed = matches!(e, VerbError::FileChanged { .. });
            let res = reply_err(e);
            assert_eq!(res.status().as_u16(), status, "{code}");
            let retry_hdr = res.headers().get("retry-after").map(|v| v.to_str().unwrap().to_string());
            assert_eq!(retry_hdr, retry.map(|r| r.to_string()), "{code} retry-after");
            let hdr = if is_file_changed { "etag" } else { "x-flint-current-etag" };
            let etag_hdr = res.headers().get(hdr).map(|v| v.to_str().unwrap().to_string());
            assert_eq!(etag_hdr.as_deref(), etag, "{code} {hdr}");
            let body = warp::hyper::body::to_bytes(res.into_body()).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["error"], code);
            assert_eq!(v["message"], message);
        }
    }
}
