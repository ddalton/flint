//! The HTTP surface, and the hop to the hub.
//!
//! ## Shape
//!
//! ```text
//! project service ──Bearer(gateway token)──▶ /v1/projects/{id}/files…
//!                                              │
//!                                              │ 1. authenticate  (before anything else)
//!                                              │ 2. validate the project id
//!                                              │ 3. find the share  (reflector, no API call)
//!                                              │ 4. decide          (resolve::decide)
//!                                              │ 5. wake if parked  (PATCH the annotation)
//!                                              ▼
//!                                       Bearer(derived) ──▶ hub /files…
//! ```
//!
//! ## Order is the security property
//!
//! Authentication runs in front of the project lookup, not beside it.
//! The hub shipped the mirror-image bug and it was fixed in `257dccb`:
//! a phase gate ahead of auth let a stranger read the hub's lifecycle
//! state without a credential. Here the leak would be worse — an
//! unauthenticated caller could enumerate which project ids exist by
//! telling 404 from 503. Behind auth, a caller with no token learns
//! only that the gateway is running.
//!
//! ## What is deliberately not here
//!
//! - **No `/status` route, of any shape.** Not proxied, not
//!   summarised. See `route.rs`.
//! - **No hub polling while waiting for a wake.** The gateway waits on
//!   the CR, because a file-API call counts as activity on the share
//!   and a gateway that polled the hub would pin awake every share it
//!   ever touched. The wait loop makes zero upstream requests, and a
//!   test asserts it.
//! - **No caching of bytes.** The hub answers `ETag`/`If-None-Match`
//!   already, and those headers cross this proxy intact; a second cache
//!   here would need its own invalidation story for a filesystem three
//!   other writers can change.

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes};
use futures::TryStreamExt;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::reflector::Store;
use kube::Client;
use serde::Serialize;
use warp::http::StatusCode;
use warp::{Filter, Rejection, Reply};

use crate::lite_operator::crd::FlintShare;
use crate::lite_operator::idle::ANN_REQUESTED_AT;
use crate::pnfs::mds::fileapi::token::TokenSource;

use super::derive::Minter;
use super::resolve::{self, Decision, Lookup, Refusal, ShareView};
use super::route::{self, Verb, RESPONSE_HEADERS};

/// Everything the gateway was started with.
#[derive(Debug, Clone)]
pub struct Config {
    /// Namespace to resolve shares in. `None` = every namespace the
    /// ServiceAccount can list, and then an ambiguous project id is
    /// refused rather than tie-broken (see [`resolve::find`]).
    pub namespace: Option<String>,
    /// Prepended to a project id to get the share name. `fs-` is the
    /// convention `docs/flint-lite-operator.md` documents.
    pub share_name_prefix: String,
    /// How long a request waits for a parked share to come back before
    /// answering 503. Zero = arm the wake and answer immediately.
    pub wake_wait: Duration,
    /// Refuse every mutating verb. A browse UI needs none of them, and
    /// this is the difference between a compromise that reads every
    /// project and one that rewrites them.
    pub read_only: bool,
    /// Largest upload accepted, before it is streamed on.
    pub max_upload_bytes: u64,
    /// How long to wait for a hub to answer with RESPONSE HEADERS. The
    /// body streams untimed after that — a GiB download is a legitimate
    /// request that must not be cut off by a request-scoped deadline.
    pub upstream_timeout: Duration,
}

/// The running gateway's shared state.
pub struct Gateway {
    pub client: Client,
    pub store: Store<FlintShare>,
    pub cfg: Config,
    pub minter: Minter,
    /// The credential the CALLER must present. Same live-re-read
    /// machinery the hub uses, so rotating the gateway's token is a
    /// Secret edit and not a restart.
    pub inbound: Arc<TokenSource>,
    pub http: reqwest::Client,
    /// Set once the share reflector has completed its initial list.
    ///
    /// Emptiness is NOT the same question: a cluster with no shares yet
    /// is a legitimate steady state, and treating it as "not ready"
    /// would keep the gateway out of service forever. Before the first
    /// list every project would 404 — read by a caller as "this project
    /// does not exist" rather than "ask again in a second" — so this
    /// gates traffic at the probe instead of being rediscovered on
    /// every request.
    pub ready: Arc<AtomicBool>,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    reason: String,
}

fn json_err(status: StatusCode, reason: &str, msg: &str, retry_after: Option<u64>) -> warp::reply::Response {
    let mut res = warp::reply::json(&ErrorBody {
        error: msg.to_string(),
        reason: reason.to_string(),
    })
    .into_response();
    *res.status_mut() = status;
    if let Some(secs) = retry_after {
        if let Ok(v) = warp::http::HeaderValue::from_str(&secs.to_string()) {
            res.headers_mut().insert("retry-after", v);
        }
    }
    res
}

fn from_refusal(r: &Refusal) -> warp::reply::Response {
    json_err(
        StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        r.reason,
        &r.message,
        r.retry_after,
    )
}

#[derive(Debug)]
struct Unauthorized;
impl warp::reject::Reject for Unauthorized {}

/// Bearer gate for the CALLER.
///
/// A near-copy of the hub's, deliberately: same `TokenSource`, same
/// per-request `current()` read so a rotation lands on the next
/// request, same constant-time compare. The one difference is that
/// there is always a token here — the gateway refuses to start without
/// one, because unlike the hub it has no "do not mount the routes"
/// posture to fall back to.
fn auth(source: Arc<TokenSource>) -> impl Filter<Extract = ((),), Error = Rejection> + Clone {
    warp::header::optional::<String>("authorization").and_then(move |given: Option<String>| {
        let source = source.clone();
        async move {
            let expected = source.current();
            let ok = given
                .as_deref()
                .and_then(|h| h.strip_prefix("Bearer "))
                .is_some_and(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()));
            if ok {
                Ok(())
            } else {
                Err(warp::reject::custom(Unauthorized))
            }
        }
    })
}

/// No early return on the first differing byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn recover(err: Rejection) -> Result<warp::reply::Response, Infallible> {
    if err.find::<Unauthorized>().is_some() {
        return Ok(json_err(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "a valid Bearer token is required",
            None,
        ));
    }
    if err.is_not_found() {
        return Ok(json_err(
            StatusCode::NOT_FOUND,
            "NoSuchRoute",
            "no such route — this gateway proxies /v1/projects/{id}/files* and nothing else",
            None,
        ));
    }
    if let Some(e) = err.find::<warp::filters::body::BodyDeserializeError>() {
        return Ok(json_err(StatusCode::BAD_REQUEST, "BadBody", &e.to_string(), None));
    }
    if err.find::<warp::reject::PayloadTooLarge>().is_some() {
        return Ok(json_err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "TooLarge",
            "upload exceeds this gateway's limit",
            None,
        ));
    }
    if err.find::<warp::reject::MethodNotAllowed>().is_some() {
        return Ok(json_err(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "that method is not proxied for this path",
            None,
        ));
    }
    tracing::warn!("unhandled rejection: {err:?}");
    Ok(json_err(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Internal",
        "internal error",
        None,
    ))
}

/// The body a verb carries upstream.
enum Payload {
    None,
    /// Buffered — small and JSON, so a 401 retry can replay it.
    Buffered(Bytes),
    /// Streamed, and therefore NOT replayable. See [`send_upstream`].
    Stream(reqwest::Body),
}

/// The whole route table.
pub fn routes(
    gw: Arc<Gateway>,
) -> impl Filter<Extract = (warp::reply::Response,), Error = Infallible> + Clone {
    // Unauthenticated and deliberately incurious: it answers for the
    // GATEWAY, names no share and reveals no fleet state. A probe must
    // not need a credential, and a probe must not be a disclosure.
    let healthz = warp::path!("healthz")
        .and(warp::get())
        .map(|| warp::reply::json(&serde_json::json!({"status": "ok"})).into_response());

    let readyz = {
        let gw = gw.clone();
        warp::path!("readyz").and(warp::get()).map(move || {
            if !gw.ready.load(Ordering::Relaxed) {
                json_err(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "CacheCold",
                    "the share cache has not listed yet",
                    Some(2),
                )
            } else {
                warp::reply::json(&serde_json::json!({"status": "ok"})).into_response()
            }
        })
    };

    let a = auth(gw.inbound.clone());

    let list = {
        let gw = gw.clone();
        warp::path!("v1" / "projects" / String / "files")
            .and(warp::get())
            .and(a.clone())
            .and(query())
            .then(move |p: String, _, q: Vec<(String, String)>| {
                let gw = gw.clone();
                async move { serve(gw, p, Verb::List, q, &[], Payload::None).await }
            })
    };

    let download = {
        let gw = gw.clone();
        warp::path!("v1" / "projects" / String / "files" / "content")
            .and(warp::get())
            .and(a.clone())
            .and(query())
            .and(warp::header::headers_cloned())
            .then(move |p: String, _, q: Vec<(String, String)>, h: warp::http::HeaderMap| {
                let gw = gw.clone();
                async move {
                    let fwd = pick_headers(&h, Verb::Download);
                    serve(gw, p, Verb::Download, q, &fwd, Payload::None).await
                }
            })
    };

    let upload = {
        let gw = gw.clone();
        let limit = gw.cfg.max_upload_bytes;
        warp::path!("v1" / "projects" / String / "files" / "content")
            .and(warp::put())
            .and(a.clone())
            .and(query())
            .and(warp::header::headers_cloned())
            .and(warp::body::content_length_limit(limit))
            .and(warp::body::stream())
            // `body` is deliberately un-annotated: warp's body stream is
            // an opaque type, and naming it would pin an implementation
            // detail of warp into this file.
            .then(move |p: String, _, q: Vec<(String, String)>, h: warp::http::HeaderMap, body| {
                let gw = gw.clone();
                async move {
                    let fwd = pick_headers(&h, Verb::Upload);
                    let body = stream_body(body);
                    serve(gw, p, Verb::Upload, q, &fwd, Payload::Stream(body)).await
                }
            })
    };

    let delete = {
        let gw = gw.clone();
        warp::path!("v1" / "projects" / String / "files" / "content")
            .and(warp::delete())
            .and(a.clone())
            .and(query())
            .and(warp::header::headers_cloned())
            .then(move |p: String, _, q: Vec<(String, String)>, h: warp::http::HeaderMap| {
                let gw = gw.clone();
                async move {
                    let fwd = pick_headers(&h, Verb::Delete);
                    serve(gw, p, Verb::Delete, q, &fwd, Payload::None).await
                }
            })
    };

    let folder = {
        let gw = gw.clone();
        warp::path!("v1" / "projects" / String / "files" / "folder")
            .and(warp::post())
            .and(a.clone())
            .and(warp::header::headers_cloned())
            .and(warp::body::content_length_limit(64 * 1024))
            .and(warp::body::bytes())
            .then(move |p: String, _, h: warp::http::HeaderMap, b: Bytes| {
                let gw = gw.clone();
                async move {
                    let fwd = pick_headers(&h, Verb::Folder);
                    serve(gw, p, Verb::Folder, vec![], &fwd, Payload::Buffered(b)).await
                }
            })
    };

    let mv = {
        let gw = gw.clone();
        warp::path!("v1" / "projects" / String / "files" / "move")
            .and(warp::post())
            .and(a)
            .and(warp::header::headers_cloned())
            .and(warp::body::content_length_limit(64 * 1024))
            .and(warp::body::bytes())
            .then(move |p: String, _, h: warp::http::HeaderMap, b: Bytes| {
                let gw = gw.clone();
                async move {
                    let fwd = pick_headers(&h, Verb::Move);
                    serve(gw, p, Verb::Move, vec![], &fwd, Payload::Buffered(b)).await
                }
            })
    };

    healthz
        .or(readyz)
        .unify()
        .or(list)
        .unify()
        .or(download)
        .unify()
        .or(upload)
        .unify()
        .or(delete)
        .unify()
        .or(folder)
        .unify()
        .or(mv)
        .unify()
        .recover(recover)
        .unify()
        .map(|r: warp::reply::Response| r)
}

/// Query pairs, order-preserving and duplicate-preserving.
///
/// A `HashMap` would silently drop a repeated key, which is a quiet way
/// to change what a caller asked for.
fn query() -> impl Filter<Extract = (Vec<(String, String)>,), Error = Infallible> + Clone {
    warp::query::raw()
        .map(|raw: String| serde_urlencoded::from_str::<Vec<(String, String)>>(&raw).unwrap_or_default())
        .or(warp::any().map(Vec::new))
        .unify()
}

/// Request headers this verb forwards, in the hub's spelling.
fn pick_headers(h: &warp::http::HeaderMap, verb: Verb) -> Vec<(String, String)> {
    verb.request_headers()
        .iter()
        .filter_map(|name| {
            let v = h.get(*name)?;
            Some(((*name).to_string(), v.to_str().ok()?.to_string()))
        })
        .collect()
}

/// One request, end to end.
async fn serve(
    gw: Arc<Gateway>,
    project: String,
    verb: Verb,
    q: Vec<(String, String)>,
    fwd_headers: &[(String, String)],
    payload: Payload,
) -> warp::reply::Response {
    if gw.cfg.read_only && verb.is_mutation() {
        return json_err(
            StatusCode::FORBIDDEN,
            "ReadOnly",
            "this gateway is deployed read-only; it proxies no mutating file operations",
            None,
        );
    }
    if let Err(bad) = resolve::validate_project_id(&project) {
        return json_err(StatusCode::BAD_REQUEST, "BadProjectId", bad.message(), None);
    }

    let mut view = match look_up(&gw, &project) {
        Ok(v) => v,
        Err(res) => return *res,
    };

    match resolve::decide(&view) {
        Decision::Dial(_) => {}
        Decision::Refuse(r) => return from_refusal(&r),
        Decision::Wake => {
            if let Err(res) = arm_wake(&gw, &view).await {
                return *res;
            }
            match wait_for_ready(&gw, &project).await {
                Ok(v) => view = v,
                Err(res) => return *res,
            }
        }
        Decision::Wait => match wait_for_ready(&gw, &project).await {
            Ok(v) => view = v,
            Err(res) => return *res,
        },
    }

    let endpoint = match resolve::decide(&view) {
        Decision::Dial(ep) => ep,
        Decision::Refuse(r) => return from_refusal(&r),
        // A share that went back to parked between the wait and here.
        _ => {
            return json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "NotServing",
                "the share is not serving",
                Some(5),
            )
        }
    };

    // The hub's OWN phase, when it was observed. Only ever a downgrade
    // of an otherwise-Ready share — see `resolve::hub_phase_blocks`.
    if let Some(r) = resolve::hub_phase_blocks(&view) {
        return from_refusal(&r);
    }

    let token = match gw.minter.token_for(view.binding()) {
        Ok(t) => t,
        Err(_) => {
            return json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "NoTokenBinding",
                &format!(
                    "share {}/{} has no bucket, so this gateway cannot derive its file-API \
                     token; give the gateway a shared token or the share a bucket",
                    view.namespace, view.name
                ),
                None,
            );
        }
    };

    let qs = route::filter_query(verb, &q);
    let url = route::upstream_url(&endpoint, verb, &qs);
    send_upstream(&gw, &view, verb, &url, fwd_headers, payload, &token).await
}

/// Find the project's share in the reflector, as a [`ShareView`].
fn look_up(gw: &Gateway, project: &str) -> Result<ShareView, Box<warp::reply::Response>> {
    let fleet = gw.store.state();
    match resolve::find(
        &fleet,
        &gw.cfg.share_name_prefix,
        project,
        gw.cfg.namespace.as_deref(),
    ) {
        Lookup::Found(s) => Ok(ShareView::of(&s)),
        Lookup::NotFound => Err(Box::new(json_err(
            StatusCode::NOT_FOUND,
            "NoSuchProject",
            "no share is registered for that project",
            None,
        ))),
        Lookup::Ambiguous(who) => {
            // Loud on the operator's side, vague on the caller's: the
            // caller cannot fix this and does not need the namespaces.
            tracing::error!(
                project = %project,
                candidates = ?who,
                "two or more shares claim one project id — refusing to guess"
            );
            Err(Box::new(json_err(
                StatusCode::CONFLICT,
                "AmbiguousProject",
                "more than one share claims that project id; refusing to guess between them",
                None,
            )))
        }
    }
}

/// Ask for the share to come back.
///
/// A merge patch on one annotation, NOT server-side apply: SSA would
/// make the gateway a field owner of the annotation and start a
/// tug-of-war with whichever front door also writes it. Merge patch
/// leaves ownership alone and is what the operator's own contract asks
/// for — it reads this annotation and never writes it.
async fn arm_wake(gw: &Gateway, view: &ShareView) -> Result<(), Box<warp::reply::Response>> {
    let api: Api<FlintShare> = Api::namespaced(gw.client.clone(), &view.namespace);
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let patch = serde_json::json!({
        "metadata": { "annotations": { ANN_REQUESTED_AT: now } }
    });
    match api
        .patch(&view.name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        Ok(_) => {
            tracing::info!(
                share = %format!("{}/{}", view.namespace, view.name),
                "wake requested"
            );
            Ok(())
        }
        Err(e) => {
            tracing::warn!(
                share = %format!("{}/{}", view.namespace, view.name),
                "wake request failed: {e}"
            );
            Err(Box::new(json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "WakeFailed",
                "could not request a wake for this share",
                Some(10),
            )))
        }
    }
}

/// Wait for the CR to say the share is serving.
///
/// **Watches the CR, never the hub.** A file-API request counts as
/// activity on the share, so polling the hub to learn whether it is up
/// would keep every share this gateway ever touched permanently awake —
/// defeating the idle ladder the whole fleet's economics rest on. The
/// reflector is already open, so this costs nothing but the sleep.
///
/// A bounded wait, and the bound is a UX decision rather than a
/// correctness one: an `IdleSuspended` share is back in ~20-30s, while
/// a `Hibernated` one is a full DR import and can be minutes. Rather
/// than hold a request open for either, the wake is armed — which
/// persists — and a caller that times out gets a 503 with a
/// `Retry-After`, having already made the share come back.
async fn wait_for_ready(gw: &Gateway, project: &str) -> Result<ShareView, Box<warp::reply::Response>> {
    let deadline = tokio::time::Instant::now() + gw.cfg.wake_wait;
    loop {
        let view = look_up(gw, project)?;
        match resolve::decide(&view) {
            Decision::Dial(_) => return Ok(view),
            Decision::Refuse(r) => return Err(Box::new(from_refusal(&r))),
            Decision::Wake | Decision::Wait => {}
        }
        if tokio::time::Instant::now() >= deadline {
            let phase = view
                .phase
                .as_ref()
                .map(|p| format!("{p:?}"))
                .unwrap_or_else(|| "unreported".into());
            return Err(Box::new(json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "Waking",
                &format!(
                    "the share is {phase}; a wake has been requested and is in progress. \
                     Retry — it does not need to be requested again."
                ),
                Some(10),
            )));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Make the upstream call and stream the answer back.
///
/// ## The 401 retry, and where it stops
///
/// `docs/plans/file-api-fleet-auth.md` §6: during a rotation a hub may
/// still hold the previous token, so a 401 is retried once at
/// `version - 1`. Safe because a 401 means the hub rejected the request
/// before doing anything — there is nothing to make idempotent.
///
/// It does NOT apply to an upload, and that is a real asymmetry rather
/// than an oversight: the request body is a stream that has already
/// been consumed by the first attempt, so replaying it would mean
/// buffering the whole upload (up to `maxUploadBytes`) in the gateway
/// on the chance of a rotation. The upload gets the 401 with a message
/// saying so.
async fn send_upstream(
    gw: &Gateway,
    view: &ShareView,
    verb: Verb,
    url: &str,
    fwd: &[(String, String)],
    payload: Payload,
    token: &str,
) -> warp::reply::Response {
    let replayable = match &payload {
        Payload::Stream(_) => None,
        Payload::None => Some(None),
        Payload::Buffered(b) => Some(Some(b.clone())),
    };

    let res = match dispatch(gw, verb, url, fwd, payload, token, view.hydrate_wait_secs).await {
        Ok(r) => r,
        Err(e) => return upstream_error(view, &e),
    };

    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        let Some(replay) = replayable else {
            return json_err(
                StatusCode::BAD_GATEWAY,
                "HubRejectedCredential",
                "the hub rejected this gateway's credential. If a token rotation is in \
                 flight, retry the upload — a streamed body cannot be replayed here.",
                Some(5),
            );
        };
        let Some(prev) = gw.minter.previous_token_for(view.binding()) else {
            return json_err(
                StatusCode::BAD_GATEWAY,
                "HubRejectedCredential",
                "the hub rejected this gateway's credential and there is no previous \
                 token version to fall back to",
                None,
            );
        };
        tracing::warn!(
            share = %format!("{}/{}", view.namespace, view.name),
            "hub rejected the current token — retrying once at the previous version"
        );
        let payload = match replay {
            Some(b) => Payload::Buffered(b),
            None => Payload::None,
        };
        match dispatch(gw, verb, url, fwd, payload, &prev, view.hydrate_wait_secs).await {
            Ok(r) => {
                if r.status() != reqwest::StatusCode::UNAUTHORIZED {
                    tracing::warn!(
                        share = %format!("{}/{}", view.namespace, view.name),
                        "this hub is running a STALE file-API token — it needs its Secret \
                         re-read or the share bounced"
                    );
                }
                return relay(r);
            }
            Err(e) => return upstream_error(view, &e),
        }
    }

    relay(res)
}

async fn dispatch(
    gw: &Gateway,
    verb: Verb,
    url: &str,
    fwd: &[(String, String)],
    payload: Payload,
    token: &str,
    hydrate_wait: Option<u64>,
) -> Result<reqwest::Response, String> {
    let mut req = gw.http.request(verb.method(), url).bearer_auth(token);
    for (k, v) in fwd {
        req = req.header(k.as_str(), v.as_str());
    }
    req = match payload {
        Payload::None => req,
        Payload::Buffered(b) => req.body(b),
        Payload::Stream(s) => req.body(s),
    };
    // The timeout covers the RESPONSE HEADERS only. Putting it on the
    // whole exchange would cap a download at `upstreamTimeout`, so a
    // large file would fail at a deadline chosen for a control-plane
    // hop. The body streams untimed after this resolves.
    let deadline = header_deadline(gw.cfg.upstream_timeout, verb, hydrate_wait);
    match tokio::time::timeout(deadline, req.send()).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => Err(scrub(&e.to_string())),
        Err(_) => Err(format!("no response headers within {deadline:?}")),
    }
}

/// How long to wait for a hub's response headers.
///
/// **This must outlive the hub's own hydrate wait, or a cold read fails
/// as the wrong error.** A download of an evicted file makes the hub
/// pull it back from S3, and `monitoring.fileApi.hydrateWaitSecs`
/// (default 30) is how long it holds the request before giving up and
/// answering 503 with a `Retry-After` — a normal, retryable outcome
/// that a browse UI handles. The gateway's own default deadline is also
/// 30s, so the two race: whichever fires first decides what the caller
/// sees, and if it is the gateway the caller gets a 502 with no
/// Retry-After and no way to tell a hydrating file from a broken hub.
///
/// So a download waits the hub's own budget plus a margin for the RTT
/// and the hub's 503 write. Every other verb keeps the configured
/// deadline: nothing else hydrates.
fn header_deadline(configured: Duration, verb: Verb, hydrate_wait: Option<u64>) -> Duration {
    if verb != Verb::Download {
        return configured;
    }
    // `None` = the share does not set one, so the hub uses its own
    // default of 30s. Assuming the default rather than the configured
    // value is the safe direction: assuming zero would reintroduce the
    // race for every share that never touched the knob, which is most.
    let hub = hydrate_wait.unwrap_or(30);
    configured.max(Duration::from_secs(hub) + Duration::from_secs(10))
}

/// A hub that could not be reached at all.
///
/// The endpoint IS named: it is an in-cluster DNS name the operator
/// derived, not a secret, and without it "bad gateway" is unactionable
/// for whoever has to fix the share.
fn upstream_error(view: &ShareView, why: &str) -> warp::reply::Response {
    tracing::warn!(
        share = %format!("{}/{}", view.namespace, view.name),
        "upstream call failed: {why}"
    );
    json_err(
        StatusCode::BAD_GATEWAY,
        "HubUnreachable",
        &format!(
            "could not reach the hub for share {}/{}: {why}",
            view.namespace, view.name
        ),
        Some(5),
    )
}

/// Belt and braces: never let a credential reach a response body.
///
/// `reqwest` does not put headers in its error strings today, and the
/// URL it does include carries no credential because the token rides an
/// `Authorization` header. This exists so that stays true if either
/// changes — the cost is a `contains` on an error path.
fn scrub(msg: &str) -> String {
    if msg.to_ascii_lowercase().contains("bearer") || msg.to_ascii_lowercase().contains("authorization") {
        return "upstream error (details suppressed)".to_string();
    }
    msg.to_string()
}

/// Copy status and allowlisted headers, then stream the body.
fn relay(res: reqwest::Response) -> warp::reply::Response {
    let status = StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut headers = warp::http::HeaderMap::new();
    for name in RESPONSE_HEADERS {
        if let Some(v) = res.headers().get(*name) {
            if let (Ok(n), Ok(v)) = (
                warp::http::header::HeaderName::from_bytes(name.as_bytes()),
                warp::http::HeaderValue::from_bytes(v.as_bytes()),
            ) {
                headers.insert(n, v);
            }
        }
    }
    // Streamed, not buffered. A hub download can be gigabytes and the
    // gateway must not hold one in memory per concurrent reader.
    let body = warp::hyper::Body::wrap_stream(res.bytes_stream());
    let mut out = warp::reply::Response::new(body);
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

/// The HTTP client for the hop to the hub.
///
/// No overall timeout, on purpose — see [`dispatch`]. `connect_timeout`
/// bounds the case that actually hangs: a headless Service name that
/// resolves to a pod IP which no longer answers.
pub fn upstream_client(connect_timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
}

/// Adapt warp's request-body stream into a reqwest streaming body.
///
/// The point of the whole function is that no `collect()` appears in
/// it: an upload is bounded by `maxUploadBytes` (5 GiB by default on
/// the hub side), and buffering one per concurrent writer is how a
/// gateway with a 256Mi limit gets OOMKilled by two users saving a
/// notebook at once.
fn stream_body<S, B>(body: S) -> reqwest::Body
where
    S: futures::Stream<Item = Result<B, warp::Error>> + Send + 'static,
    B: Buf,
{
    reqwest::Body::wrap_stream(
        body.map_ok(|mut b| b.copy_to_bytes(b.remaining()))
            .map_err(|e| std::io::Error::other(e.to_string())),
    )
}

#[cfg(test)]
mod tests {
    //! End-to-end, over real sockets.
    //!
    //! The pure decision logic is tested in `resolve` and the URL
    //! construction in `route`; neither of those exercises the thing
    //! that actually faces the network. These tests stand up a FAKE HUB
    //! on a real port, point a share's `status.apiEndpoint` at it, and
    //! drive requests through the assembled warp route table — so what
    //! is asserted is what a caller would observe, including the parts
    //! (filter ordering, header copying, method routing) that no unit
    //! test of a helper can see.
    //!
    //! Every test here that asserts an ABSENCE — the hub was not
    //! reached, a header did not arrive — is paired with a positive
    //! control in the same test, because "nothing happened" is also
    //! what a broken rig produces.

    use super::*;
    use crate::lite_operator::crd::FlintShare;
    use kube::runtime::{reflector, watcher};
    use std::sync::Mutex;
    use warp::http::HeaderMap;

    /// What the fake hub saw.
    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        path: String,
        query: String,
        auth: Option<String>,
        if_match: Option<String>,
        range: Option<String>,
        body: Vec<u8>,
    }

    #[derive(Clone, Default)]
    struct HubLog(Arc<Mutex<Vec<Seen>>>);

    impl HubLog {
        fn all(&self) -> Vec<Seen> {
            self.0.lock().unwrap().clone()
        }
        fn paths(&self) -> Vec<String> {
            self.all().into_iter().map(|s| s.path).collect()
        }
    }

    /// A stand-in hub that answers ANY path — including `/status`.
    ///
    /// Answering everything is the point: a hub that 404'd on `/status`
    /// would make the traversal tests pass without the gateway doing
    /// anything, which is exactly the vacuous shape this repo keeps
    /// finding. Here, if a request for `/status` ever arrives it is
    /// logged and answered 200 with a marker body, and the test fails
    /// on the log.
    async fn fake_hub(log: HubLog, name: &'static str) -> String {
        let route = warp::any()
            .and(warp::method())
            .and(warp::path::full())
            .and(
                warp::filters::query::raw()
                    .or(warp::any().map(String::new))
                    .unify(),
            )
            .and(warp::header::headers_cloned())
            .and(warp::body::bytes())
            .map(
                move |m: warp::http::Method,
                      p: warp::path::FullPath,
                      q: String,
                      h: HeaderMap,
                      b: Bytes| {
                    let hdr = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(String::from);
                    log.0.lock().unwrap().push(Seen {
                        method: m.to_string(),
                        path: p.as_str().to_string(),
                        query: q,
                        auth: hdr("authorization"),
                        if_match: hdr("if-match"),
                        range: hdr("range"),
                        body: b.to_vec(),
                    });
                    // Every body NAMES THE HUB THAT SERVED IT. That is
                    // what makes a cross-routed request visible in the
                    // response a caller sees, rather than only in a log
                    // the test happens to check.
                    let body = if p.as_str() == "/status" {
                        // The marker. If this ever reaches a caller the
                        // gateway has failed at its whole job.
                        format!(r#"{{"phase":"Serving","hub":"{name}","THIS-IS-THE-STATUS-DOCUMENT":true}}"#)
                    } else {
                        format!(r#"{{"hub":"{name}","entries":[]}}"#)
                    };
                    let mut res = warp::reply::Response::new(body.into());
                    res.headers_mut().insert("etag", "\"abc123\"".parse().unwrap());
                    res.headers_mut().insert("content-type", "application/json".parse().unwrap());
                    // Something the allowlist does NOT name. A proxy
                    // that copies headers wholesale leaks it.
                    res.headers_mut().insert("x-hub-internal", "leaked".parse().unwrap());
                    res.headers_mut().insert("set-cookie", "hubsession=1".parse().unwrap());
                    res
                },
            );
        let (addr, srv) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(srv);
        format!("http://{addr}")
    }

    fn share_json(name: &str, project: &str, phase: &str, endpoint: Option<&str>) -> FlintShare {
        let mut status = serde_json::json!({ "phase": phase });
        if let Some(ep) = endpoint {
            status["apiEndpoint"] = serde_json::json!(ep);
            status["conditions"] = serde_json::json!([{
                "type": "ApiEndpointPublished", "status": "True", "reason": "InCluster",
                "lastTransitionTime": "2026-08-21T00:00:00Z"
            }]);
        }
        serde_json::from_value(serde_json::json!({
            "apiVersion": "flint.io/v1alpha1", "kind": "FlintShare",
            "metadata": {
                "name": name, "namespace": "workspaces",
                "labels": {"flint.io/project-id": project},
            },
            "spec": {
                "bucket": "tenant-bucket",
                "keyPrefix": format!("{project}/"),
                "persistence": {"size": "20Gi"}
            },
            "status": status
        }))
        .expect("test share")
    }

    fn store_of(shares: Vec<FlintShare>) -> reflector::Store<FlintShare> {
        let (store, mut writer) = reflector::store::<FlintShare>();
        writer.apply_watcher_event(&watcher::Event::Init);
        for s in shares {
            writer.apply_watcher_event(&watcher::Event::InitApply(s));
        }
        writer.apply_watcher_event(&watcher::Event::InitDone);
        store
    }

    const INBOUND: &str = "the-gateway-token";

    struct Rig {
        gw: Arc<Gateway>,
        log: HubLog,
    }

    async fn rig(shares: Vec<FlintShare>, read_only: bool, wake_wait_secs: u64) -> Rig {
        let log = HubLog::default();
        let hub = fake_hub(log.clone(), "hub-a").await;
        // Re-point every share that carries the placeholder at the real
        // ephemeral port.
        let shares = shares
            .into_iter()
            .map(|mut s| {
                if let Some(st) = s.status.as_mut() {
                    if st.api_endpoint.as_deref() == Some("HUB") {
                        st.api_endpoint = Some(hub.clone());
                    }
                }
                s
            })
            .collect();
        // Two rustls providers are in this crate's tree, so the
        // process default has to be chosen before ANY client is built —
        // `Client::try_from` panicked outright without it, which is the
        // regression v1.28.0 shipped a fix for.
        crate::install_crypto_provider();
        // Never dialled in these tests; a Ready share needs no API
        // server, and the parked-share test asserts the hub is not
        // touched even when the wake PATCH cannot be delivered.
        let client = kube::Client::try_from(kube::Config::new(
            "http://127.0.0.1:1".parse().expect("uri"),
        ))
        .expect("client");
        let gw = Arc::new(Gateway {
            client,
            store: store_of(shares),
            cfg: Config {
                namespace: None,
                share_name_prefix: "fs-".into(),
                wake_wait: Duration::from_secs(wake_wait_secs),
                read_only,
                max_upload_bytes: 1024 * 1024,
                upstream_timeout: Duration::from_secs(5),
            },
            minter: Minter::Shared("the-hub-token".into()),
            inbound: TokenSource::fixed(INBOUND),
            http: upstream_client(Duration::from_secs(2)).expect("http"),
            ready: Arc::new(AtomicBool::new(true)),
        });
        Rig { gw, log }
    }

    async fn ready_rig() -> Rig {
        rig(vec![share_json("fs-proj-a", "proj-a", "Ready", Some("HUB"))], false, 0).await
    }

    fn req() -> warp::test::RequestBuilder {
        warp::test::request().header("authorization", format!("Bearer {INBOUND}"))
    }

    /// THE HEADLINE.
    ///
    /// The hub serves an UNAUTHENTICATED `/status` on the same listener
    /// as the file API — tier recovery point, epoch holder, NFS client
    /// list, lifecycle phase. This gateway faces further out than the
    /// hub ever does, so a single path that reaches `/status` publishes
    /// all of it.
    ///
    /// The fake hub answers every path, so nothing here passes because
    /// the target was missing.
    #[tokio::test]
    async fn no_request_shape_reaches_the_hubs_status_document() {
        let r = ready_rig().await;
        let attempts = [
            "/status",
            "/v1/projects/proj-a/status",
            "/v1/projects/proj-a/files/../status",
            "/v1/projects/proj-a/files/%2e%2e/status",
            "/v1/projects/proj-a/../status",
            "/v1/projects/proj-a%2F..%2Fstatus/files",
            "/v1/projects/proj-a/files/content/../../status",
            "/v1/projects/proj-a/files?path=../status",
            "/v1/projects/proj-a/files/content?path=/../../status",
            "/v1/projects/../status/files",
            "/v1//projects/proj-a/status",
            "/v1/projects/proj-a/files/status",
        ];
        for path in attempts {
            let res = req().method("GET").path(path).reply(&routes(r.gw.clone())).await;
            let body = String::from_utf8_lossy(res.body());
            assert!(
                !body.contains("THIS-IS-THE-STATUS-DOCUMENT"),
                "{path} returned the hub's status document"
            );
        }
        // Some of those attempts DO reach the hub — the two that put
        // the traversal in `path=` are well-formed requests for a file
        // called `../status`, and the hub's own `FsPath::parse` is what
        // refuses them. What matters is that every request that got
        // through landed on one of the six literal file paths.
        let legal: Vec<&str> = super::super::route::ALL
            .iter()
            .map(|v| v.upstream_path())
            .collect();
        for p in r.log.paths() {
            assert!(
                legal.contains(&p.as_str()),
                "a request reached the hub at {p}, which is not one of {legal:?}"
            );
        }

        // POSITIVE CONTROL, same rig, same hub: the legitimate route
        // does arrive and is served. Without this the assertions above
        // hold just as well against a gateway that can reach nothing at
        // all.
        let before = r.log.all().len();
        let ok = req()
            .method("GET")
            .path("/v1/projects/proj-a/files?path=/")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(ok.status(), 200, "the control request must reach the hub");
        assert_eq!(r.log.all()[before].path, "/files");
        assert!(
            String::from_utf8_lossy(ok.body()).contains(r#""hub":"hub-a""#),
            "the control answer did not come from the hub"
        );
    }

    /// Auth runs in FRONT of the project lookup. The hub shipped the
    /// mirror-image bug (a phase gate ahead of auth, fixed in
    /// `257dccb`); here it would let a stranger enumerate which project
    /// ids exist by telling 404 from 503.
    #[tokio::test]
    async fn an_unauthenticated_caller_cannot_tell_a_real_project_from_a_missing_one() {
        let r = ready_rig().await;
        for path in [
            "/v1/projects/proj-a/files?path=/",     // exists
            "/v1/projects/nope/files?path=/",       // does not
            "/v1/projects/BAD_ID/files?path=/",     // would be a 400
        ] {
            for builder in [
                warp::test::request(),
                warp::test::request().header("authorization", "Bearer wrong"),
                warp::test::request().header("authorization", INBOUND), // no "Bearer "
            ] {
                let res = builder.method("GET").path(path).reply(&routes(r.gw.clone())).await;
                assert_eq!(res.status(), 401, "{path} leaked a distinguishable status");
            }
        }
        assert!(r.log.all().is_empty(), "an unauthenticated call reached a hub");

        // CONTROL: with the token the three DO answer differently, so
        // the sameness above is auth and not a uniformly broken rig.
        let mut got = Vec::new();
        for path in [
            "/v1/projects/proj-a/files?path=/",
            "/v1/projects/nope/files?path=/",
            "/v1/projects/BAD_ID/files?path=/",
        ] {
            got.push(
                req().method("GET").path(path).reply(&routes(r.gw.clone())).await.status().as_u16(),
            );
        }
        assert_eq!(got, vec![200, 404, 400]);
    }

    /// The caller's credential authenticates it to the GATEWAY. A hub
    /// that received it would hold the key to every other project.
    #[tokio::test]
    async fn the_hub_sees_the_gateways_credential_and_never_the_callers() {
        let r = ready_rig().await;
        let res = req()
            .method("GET")
            .path("/v1/projects/proj-a/files?path=/")
            .header("cookie", "session=abc")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 200);
        let seen = r.log.all();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].auth.as_deref(), Some("Bearer the-hub-token"));
        assert!(
            !seen[0].auth.as_deref().unwrap().contains(INBOUND),
            "the caller's token was forwarded upstream"
        );
    }

    /// v1.30.0's conditional writes are an end-to-end protocol and this
    /// proxy sits in the middle of it. A dropped `If-Match` downgrades
    /// every conditional write in the fleet to a blind overwrite, and
    /// both ends still answer 200 — so the lost update is invisible
    /// from either side. A dropped `ETag` on the way back means the
    /// caller has nothing to send next time.
    #[tokio::test]
    async fn the_conditional_write_and_range_headers_cross_in_both_directions() {
        let r = ready_rig().await;
        let res = req()
            .method("PUT")
            .path("/v1/projects/proj-a/files/content?path=/a.txt")
            .header("if-match", "\"v1\"")
            .header("content-type", "application/octet-stream")
            .body("hello")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 200);
        let seen = r.log.all();
        assert_eq!(seen[0].method, "PUT");
        assert_eq!(seen[0].path, "/files/content");
        assert_eq!(seen[0].query, "path=%2Fa.txt");
        assert_eq!(seen[0].if_match.as_deref(), Some("\"v1\""));
        assert_eq!(seen[0].body, b"hello", "the body must survive the stream hop");
        assert_eq!(res.headers().get("etag").unwrap(), "\"abc123\"");

        let res = req()
            .method("GET")
            .path("/v1/projects/proj-a/files/content?path=/a.txt")
            .header("range", "bytes=0-9")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 200);
        assert_eq!(r.log.all()[1].range.as_deref(), Some("bytes=0-9"));
    }

    /// Response headers are an allowlist, not a copy-minus-hop-by-hop.
    /// The two directions are not symmetric: forgetting to strip
    /// something the hub grows later leaks it, while forgetting to add
    /// something breaks a feature loudly.
    #[tokio::test]
    async fn only_allowlisted_response_headers_reach_the_caller() {
        let r = ready_rig().await;
        let res = req()
            .method("GET")
            .path("/v1/projects/proj-a/files?path=/")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 200);
        // Present, because they are named.
        assert!(res.headers().contains_key("etag"));
        assert!(res.headers().contains_key("content-type"));
        // Absent, because they are not — and the fake hub really did
        // send both, which the assertions above prove it can.
        assert!(!res.headers().contains_key("x-hub-internal"), "leaked an internal header");
        assert!(!res.headers().contains_key("set-cookie"), "leaked a Set-Cookie from a hub");
    }

    #[tokio::test]
    async fn a_read_only_gateway_refuses_mutations_before_it_dials_anything() {
        let r = rig(
            vec![share_json("fs-proj-a", "proj-a", "Ready", Some("HUB"))],
            true,
            0,
        )
        .await;
        let cases = [
            ("PUT", "/v1/projects/proj-a/files/content?path=/a.txt"),
            ("DELETE", "/v1/projects/proj-a/files/content?path=/a.txt"),
            ("POST", "/v1/projects/proj-a/files/folder"),
            ("POST", "/v1/projects/proj-a/files/move"),
        ];
        for (m, p) in cases {
            let res = req()
                .method(m)
                .path(p)
                .header("content-type", "application/json")
                .body("{}")
                .reply(&routes(r.gw.clone()))
                .await;
            assert_eq!(res.status(), 403, "{m} {p}");
        }
        assert!(r.log.all().is_empty(), "a refused mutation still reached a hub");

        // CONTROL: reads still work on the same rig.
        let ok = req()
            .method("GET")
            .path("/v1/projects/proj-a/files?path=/")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(ok.status(), 200);
    }

    /// A parked share must not be dialled, and — the part that matters
    /// for the fleet's economics — the WAIT must not touch the hub
    /// either. A file-API call counts as activity, so a gateway that
    /// polled hubs while waiting would pin awake every share it ever
    /// touched and quietly disable the idle ladder.
    ///
    /// The wake PATCH fails here (there is no API server at
    /// 127.0.0.1:1), which is the point: it proves the hub was not
    /// dialled even on the path where waking could not be arranged.
    #[tokio::test]
    async fn a_parked_share_is_never_dialled_and_waiting_never_touches_the_hub() {
        let r = rig(
            vec![share_json("fs-proj-a", "proj-a", "IdleSuspended", Some("HUB"))],
            false,
            1,
        )
        .await;
        let res = req()
            .method("GET")
            .path("/v1/projects/proj-a/files?path=/")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 503, "a parked share must not be served");
        assert!(r.log.all().is_empty(), "the hub was dialled for a parked share");
        assert!(res.headers().contains_key("retry-after"), "a 503 must say when to come back");
    }

    /// `Suspended` is an ADMIN decision the CRD says a wake request does
    /// not override. A gateway that armed the annotation anyway would be
    /// quietly reversing an operator.
    #[tokio::test]
    async fn an_admin_suspended_share_is_refused_rather_than_woken() {
        let r = rig(
            vec![share_json("fs-proj-a", "proj-a", "Suspended", Some("HUB"))],
            false,
            5,
        )
        .await;
        let started = std::time::Instant::now();
        let res = req()
            .method("GET")
            .path("/v1/projects/proj-a/files?path=/")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 409);
        assert!(r.log.all().is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "it waited out the wake budget for a share that will never wake"
        );
    }

    #[tokio::test]
    async fn the_probes_answer_without_a_credential_and_name_no_share() {
        let r = ready_rig().await;
        for p in ["/healthz", "/readyz"] {
            let res = warp::test::request().method("GET").path(p).reply(&routes(r.gw.clone())).await;
            assert_eq!(res.status(), 200, "{p}");
            let body = String::from_utf8_lossy(res.body()).to_string();
            assert!(!body.contains("proj-a"), "{p} named a share: {body}");
            assert!(!body.contains(INBOUND), "{p} leaked the gateway token");
        }
    }

    /// Before the reflector has listed, every project would 404 — which
    /// a caller reads as "this project does not exist" rather than "ask
    /// me again in a second". The readiness probe has to gate traffic
    /// rather than the condition being rediscovered per request.
    #[tokio::test]
    async fn readyz_fails_while_the_share_cache_is_still_cold() {
        let r = ready_rig().await;
        r.gw.ready.store(false, Ordering::Relaxed);
        let res = warp::test::request().method("GET").path("/readyz").reply(&routes(r.gw.clone())).await;
        assert_eq!(res.status(), 503);
        assert_eq!(res.headers().get("retry-after").unwrap(), "2");
        // And /healthz stays up, so a liveness probe does not restart a
        // pod that is merely still listing.
        let live = warp::test::request().method("GET").path("/healthz").reply(&routes(r.gw.clone())).await;
        assert_eq!(live.status(), 200);
    }

    /// An oversized upload must be refused by the GATEWAY rather than
    /// streamed into a hub that will refuse it after receiving it.
    #[tokio::test]
    async fn an_oversized_upload_is_refused_before_it_reaches_a_hub() {
        let r = ready_rig().await;
        let big = vec![b'x'; 2 * 1024 * 1024]; // limit is 1 MiB
        let res = req()
            .method("PUT")
            .path("/v1/projects/proj-a/files/content?path=/big.bin")
            .body(big)
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 413);
        assert!(r.log.all().is_empty(), "the oversized body was forwarded");
    }

    /// Two shares claiming one project id is a 409, never a guess: the
    /// alternative serves one tenant's files to a caller asking for the
    /// other's, and the reflector's order is not stable across relists.
    #[tokio::test]
    async fn an_ambiguous_project_id_is_refused_without_dialling_either_hub() {
        let mut a = share_json("fs-proj-a", "proj-a", "Ready", Some("HUB"));
        a.metadata.namespace = Some("tenant-a".into());
        let mut b = share_json("fs-proj-a", "proj-a", "Ready", Some("HUB"));
        b.metadata.namespace = Some("tenant-b".into());
        let r = rig(vec![a, b], false, 0).await;
        let res = req()
            .method("GET")
            .path("/v1/projects/proj-a/files?path=/")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 409);
        assert!(r.log.all().is_empty(), "it picked one of two tenants");
        let body = String::from_utf8_lossy(res.body()).to_string();
        assert!(!body.contains("tenant-a") && !body.contains("tenant-b"),
            "the caller cannot fix this and does not need the namespaces: {body}");
    }

    /// Unknown query parameters are dropped rather than forwarded.
    #[tokio::test]
    async fn only_the_declared_query_parameters_reach_the_hub() {
        let r = ready_rig().await;
        let res = req()
            .method("GET")
            .path("/v1/projects/proj-a/files?path=/x&limit=5&token=steal&redirect=http://evil")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 200);
        assert_eq!(r.log.all()[0].query, "path=%2Fx&limit=5");
    }

    /// A cold read hydrates from S3, and the hub holds the request
    /// while it does. Both defaults are 30s, so without this the
    /// gateway's deadline fires FIRST and a routine hydration is
    /// reported as a 502 with no `Retry-After` — indistinguishable from
    /// a dead hub, and not retryable by a UI that reads the difference.
    #[test]
    fn a_downloads_deadline_always_outlives_the_hubs_hydrate_wait() {
        let cfg = Duration::from_secs(30);

        // The default collision: the hub may hold for 30s.
        assert!(
            header_deadline(cfg, Verb::Download, None) > Duration::from_secs(30),
            "the gateway would time out at the same instant the hub answers 503"
        );
        // A share that raised its hydrate budget drags the deadline with it.
        assert_eq!(
            header_deadline(cfg, Verb::Download, Some(120)),
            Duration::from_secs(130)
        );
        // A share that LOWERED it keeps the configured deadline, which
        // is already generous.
        assert_eq!(header_deadline(cfg, Verb::Download, Some(5)), cfg);

        // Nothing else hydrates, so nothing else is extended — a hung
        // PUT must not hold a connection for a hydrate budget that has
        // no bearing on it.
        for v in super::super::route::ALL.iter().filter(|v| **v != Verb::Download) {
            assert_eq!(header_deadline(cfg, *v, Some(600)), cfg, "{v:?}");
        }
    }

    // ───────────────────────────────────────────────────────────────
    // TWO HUBS, AND A CLIENT THAT IS ACTUALLY OUTSIDE
    //
    // Everything above drives the filter in-process with
    // `warp::test`, which is the right tool for asserting the route
    // table but skips the gateway's own socket. It also runs against
    // ONE hub, and a proxy with one backend cannot exhibit the single
    // most damaging bug a multi-tenant proxy has: serving project A's
    // request from project B's hub. Nothing about the code above would
    // change if `look_up` ignored the project id entirely.
    //
    // So this section stands up TWO independent hubs on two ports,
    // binds the gateway on a third, and drives it with `reqwest` — a
    // real client, over a real socket, exactly as the project service
    // will. Each hub names itself in every response body, so a
    // misroute is visible in what the CALLER receives and not only in
    // a log the test remembers to check.
    // ───────────────────────────────────────────────────────────────

    const ROOT_KEY: &[u8] = b"a-root-key-of-at-least-32-bytes-long!!";

    struct TwoHubs {
        base: String,
        a: HubLog,
        b: HubLog,
    }

    /// Two hubs, two shares, one gateway, all on real ports.
    async fn two_hub_rig(read_only: bool) -> TwoHubs {
        crate::install_crypto_provider();
        let (la, lb) = (HubLog::default(), HubLog::default());
        let (ha, hb) = (
            fake_hub(la.clone(), "hub-a").await,
            fake_hub(lb.clone(), "hub-b").await,
        );
        assert_ne!(ha, hb, "the two hubs must be distinct servers");

        let mut sa = share_json("fs-proj-a", "proj-a", "Ready", Some(&ha));
        let mut sb = share_json("fs-proj-b", "proj-b", "Ready", Some(&hb));
        // `share_json` derives keyPrefix from the project id, so the two
        // shares already have DIFFERENT bindings — which is what makes
        // their derived tokens differ.
        sa.metadata.namespace = Some("tenant-a".into());
        sb.metadata.namespace = Some("tenant-b".into());

        let client = kube::Client::try_from(kube::Config::new(
            "http://127.0.0.1:1".parse().expect("uri"),
        ))
        .expect("client");
        let gw = Arc::new(Gateway {
            client,
            store: store_of(vec![sa, sb]),
            cfg: Config {
                namespace: None,
                share_name_prefix: "fs-".into(),
                wake_wait: Duration::from_secs(0),
                read_only,
                max_upload_bytes: 1024 * 1024,
                upstream_timeout: Duration::from_secs(5),
            },
            minter: Minter::Derived(ROOT_KEY.to_vec()),
            inbound: TokenSource::fixed(INBOUND),
            http: upstream_client(Duration::from_secs(2)).expect("http"),
            ready: Arc::new(AtomicBool::new(true)),
        });

        let (addr, srv) = warp::serve(routes(gw)).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(srv);
        TwoHubs { base: format!("http://{addr}"), a: la, b: lb }
    }

    fn outside() -> reqwest::Client {
        reqwest::Client::builder().build().expect("external client")
    }

    fn expected_token(project: &str) -> String {
        super::super::derive::derive(
            ROOT_KEY,
            &super::super::derive::Binding {
                endpoint: "",
                bucket: "tenant-bucket",
                key_prefix: &format!("{project}/"),
                version: 1,
            },
        )
    }

    /// Every project's traffic reaches ITS OWN hub, over a real socket,
    /// from a real HTTP client — and never the other tenant's hub.
    ///
    /// This is the property a single-backend test cannot express. The
    /// two hubs are separate servers on separate ports whose responses
    /// name them, so a misroute fails on the body the caller got, not
    /// just on a log.
    #[tokio::test]
    async fn each_project_reaches_its_own_hub_and_never_the_other_tenants() {
        let r = two_hub_rig(false).await;
        let c = outside();

        for (project, want_hub) in [("proj-a", "hub-a"), ("proj-b", "hub-b")] {
            let res = c
                .get(format!("{}/v1/projects/{project}/files?path=/", r.base))
                .bearer_auth(INBOUND)
                .send()
                .await
                .expect("the gateway must answer an external client");
            assert_eq!(res.status(), 200, "{project}");
            let body = res.text().await.unwrap();
            assert!(
                body.contains(&format!(r#""hub":"{want_hub}""#)),
                "{project} was served by the wrong hub: {body}"
            );
        }

        // One request each, and each landed on exactly one hub.
        assert_eq!(r.a.paths(), vec!["/files"], "hub-a");
        assert_eq!(r.b.paths(), vec!["/files"], "hub-b");
    }

    /// Each hub receives ITS OWN derived credential.
    ///
    /// The containment claim of the derived-token design rests on this:
    /// a compromised hub holds a token that opens only itself. If the
    /// gateway sent one value to both, that claim would be false and
    /// nothing in a one-hub test could tell.
    #[tokio::test]
    async fn each_hub_is_given_a_different_credential_bound_to_its_own_prefix() {
        let r = two_hub_rig(false).await;
        let c = outside();
        for project in ["proj-a", "proj-b"] {
            c.get(format!("{}/v1/projects/{project}/files?path=/", r.base))
                .bearer_auth(INBOUND)
                .send()
                .await
                .unwrap();
        }
        let ta = r.a.all()[0].auth.clone().expect("hub-a saw no credential");
        let tb = r.b.all()[0].auth.clone().expect("hub-b saw no credential");

        assert_eq!(ta, format!("Bearer {}", expected_token("proj-a")));
        assert_eq!(tb, format!("Bearer {}", expected_token("proj-b")));
        assert_ne!(ta, tb, "both hubs were given the same token");
        // And neither is the credential the CALLER presented.
        assert!(!ta.contains(INBOUND) && !tb.contains(INBOUND));
    }

    /// The whole file API, through the proxy, against both hubs.
    ///
    /// Not just the read path: a browse UI creates folders, renames and
    /// deletes. Each verb is checked for the method, the upstream path
    /// and the hub it landed on.
    #[tokio::test]
    async fn all_six_file_operations_work_externally_against_both_hubs() {
        let r = two_hub_rig(false).await;
        let c = outside();

        for (project, log, want_hub) in
            [("proj-a", &r.a, "hub-a"), ("proj-b", &r.b, "hub-b")]
        {
            let base = format!("{}/v1/projects/{project}", r.base);
            let calls: Vec<(reqwest::RequestBuilder, &str, &str)> = vec![
                (c.get(format!("{base}/files?path=/")), "GET", "/files"),
                (
                    c.get(format!("{base}/files/content?path=/a.txt")),
                    "GET",
                    "/files/content",
                ),
                (
                    c.put(format!("{base}/files/content?path=/a.txt")).body("bytes"),
                    "PUT",
                    "/files/content",
                ),
                (
                    c.delete(format!("{base}/files/content?path=/a.txt")),
                    "DELETE",
                    "/files/content",
                ),
                (
                    c.post(format!("{base}/files/folder"))
                        .header("content-type", "application/json")
                        .body(r#"{"path":"/new"}"#),
                    "POST",
                    "/files/folder",
                ),
                (
                    c.post(format!("{base}/files/move"))
                        .header("content-type", "application/json")
                        .body(r#"{"from":"/a.txt","to":"/b.txt"}"#),
                    "POST",
                    "/files/move",
                ),
            ];
            let expected: Vec<(String, String)> = calls
                .iter()
                .map(|(_, m, p)| ((*m).to_string(), (*p).to_string()))
                .collect();

            for (rb, method, path) in calls {
                let res = rb.bearer_auth(INBOUND).send().await.expect("gateway answered");
                assert_eq!(res.status(), 200, "{project} {method} {path}");
                let body = res.text().await.unwrap();
                assert!(
                    body.contains(&format!(r#""hub":"{want_hub}""#)),
                    "{project} {method} {path} was served by the wrong hub: {body}"
                );
            }

            let seen: Vec<(String, String)> =
                log.all().into_iter().map(|s| (s.method, s.path)).collect();
            assert_eq!(seen, expected, "{project}: wrong verbs or paths reached the hub");
        }

        // Six each, and not one crossed over.
        assert_eq!(r.a.all().len(), 6);
        assert_eq!(r.b.all().len(), 6);
    }

    /// Interleaved concurrent traffic to both projects stays routed.
    ///
    /// A per-request lookup is correct by construction; a cached-or
    /// shared "current endpoint" would not be, and would pass every
    /// sequential test above. This drives both projects at once and
    /// checks that each hub saw only its own prefix in `path=`.
    #[tokio::test]
    async fn concurrent_traffic_to_two_projects_never_crosses_over() {
        let r = two_hub_rig(false).await;
        let c = outside();
        let mut tasks = Vec::new();
        for i in 0..24 {
            let project = if i % 2 == 0 { "proj-a" } else { "proj-b" };
            let url = format!(
                "{}/v1/projects/{project}/files?path=/{project}-{i}",
                r.base
            );
            let c = c.clone();
            tasks.push(tokio::spawn(async move {
                let res = c.get(url).bearer_auth(INBOUND).send().await.unwrap();
                let status = res.status();
                (project, status, res.text().await.unwrap())
            }));
        }
        for t in tasks {
            let (project, status, body) = t.await.unwrap();
            assert_eq!(status, 200, "{project}");
            let want = if project == "proj-a" { "hub-a" } else { "hub-b" };
            assert!(body.contains(&format!(r#""hub":"{want}""#)), "{project}: {body}");
        }
        assert_eq!(r.a.all().len(), 12);
        assert_eq!(r.b.all().len(), 12);
        for (log, project) in [(&r.a, "proj-a"), (&r.b, "proj-b")] {
            for seen in log.all() {
                assert!(
                    seen.query.contains(project),
                    "{project}'s hub was asked for {}", seen.query
                );
            }
        }
    }

    /// Neither hub's `/status` is reachable through the gateway.
    ///
    /// **Note what a real client does to these URLs.** `reqwest` (like
    /// every RFC 3986 client) resolves `..` BEFORE the request leaves
    /// the process, so `/v1/projects/proj-a/../proj-b/files` is sent as
    /// `/v1/projects/proj-b/files` and is a perfectly ordinary request
    /// for proj-b. That is not the gateway doing anything, and a test
    /// that asserted "proj-a's request did not reach hub-b" here would
    /// be asserting a property of reqwest.
    ///
    /// The shapes that actually survive a client are the
    /// percent-encoded ones — `%2F..%2F` stays one path segment — and
    /// those land in the project id, where `validate_project_id`
    /// refuses them. Both are checked. The un-normalised form is
    /// checked separately, over a raw socket, by
    /// [`a_handwritten_request_cannot_traverse_out_of_the_files_routes`].
    #[tokio::test]
    async fn neither_hubs_status_is_reachable_from_an_external_client() {
        let r = two_hub_rig(false).await;
        let c = outside();
        let cases = [
            ("/status", 404),
            ("/v1/projects/proj-a/status", 404),
            ("/v1/projects/proj-a/files/status", 404),
            // One segment after decoding, and not a legal project id.
            ("/v1/projects/proj-a%2F..%2Fproj-b/files", 400),
            ("/v1/projects/..%2F..%2Fstatus/files", 400),
            // A legal id that simply has no share.
            ("/v1/projects/status/files", 404),
        ];
        for (path, want) in cases {
            let res = c
                .get(format!("{}{path}", r.base))
                .bearer_auth(INBOUND)
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), want, "{path}");
            let body = res.text().await.unwrap();
            assert!(
                !body.contains("THIS-IS-THE-STATUS-DOCUMENT"),
                "{path} returned a status document"
            );
        }
        for (log, who) in [(&r.a, "hub-a"), (&r.b, "hub-b")] {
            assert!(
                !log.paths().iter().any(|p| p.contains("status")),
                "{who} was asked for a status path: {:?}",
                log.paths()
            );
        }
    }

    /// The adversary is not a well-behaved HTTP library.
    ///
    /// Everything above goes through `reqwest`, which normalises `..`
    /// out of a path before sending it — so those tests can never put
    /// a raw traversal on the wire, and would keep passing against a
    /// gateway that resolved `..` itself. This writes the bytes to the
    /// socket by hand, which is what an attacker does.
    ///
    /// warp matches path SEGMENTS and never resolves `..`, so
    /// `/v1/projects/proj-a/../proj-b/files` is six segments and
    /// matches no route. That is the property being pinned: the
    /// gateway does not have traversal handling to get wrong.
    #[tokio::test]
    async fn a_handwritten_request_cannot_traverse_out_of_the_files_routes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let r = two_hub_rig(false).await;
        let host = r.base.trim_start_matches("http://").to_string();

        let raw = [
            "/v1/projects/proj-a/../proj-b/files?path=/",
            "/v1/projects/proj-a/../../status",
            "/v1/projects/proj-a/files/../status",
            "/v1/projects/proj-a/files/content/../../../status",
            "/../status",
            "//status",
            "/v1/projects/proj-a/files/..%2f..%2fstatus",
        ];
        for path in raw {
            let mut sock = tokio::net::TcpStream::connect(&host).await.expect("connect");
            let req = format!(
                "GET {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {INBOUND}\r\n\
                 Connection: close\r\n\r\n"
            );
            sock.write_all(req.as_bytes()).await.expect("write");
            let mut buf = Vec::new();
            sock.read_to_end(&mut buf).await.expect("read");
            let res = String::from_utf8_lossy(&buf);
            assert!(
                !res.contains("THIS-IS-THE-STATUS-DOCUMENT"),
                "raw {path} returned a status document:\n{res}"
            );
            assert!(
                res.starts_with("HTTP/1.1 400") || res.starts_with("HTTP/1.1 404"),
                "raw {path} was not refused outright:\n{res}"
            );
        }
        for (log, who) in [(&r.a, "hub-a"), (&r.b, "hub-b")] {
            assert!(log.all().is_empty(), "{who} was reached by a handwritten traversal: {:?}", log.paths());
        }

        // POSITIVE CONTROL over the same raw socket: a well-formed
        // handwritten request IS served, so the refusals above are the
        // routing and not a gateway that rejects everything typed by
        // hand.
        let mut sock = tokio::net::TcpStream::connect(&host).await.expect("connect");
        let req = format!(
            "GET /v1/projects/proj-b/files?path=/ HTTP/1.1\r\nHost: {host}\r\n\
             Authorization: Bearer {INBOUND}\r\nConnection: close\r\n\r\n"
        );
        sock.write_all(req.as_bytes()).await.expect("write");
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).await.expect("read");
        let res = String::from_utf8_lossy(&buf);
        assert!(res.starts_with("HTTP/1.1 200"), "control request refused:\n{res}");
        assert!(res.contains(r#""hub":"hub-b""#), "control did not reach hub-b:\n{res}");
        assert!(r.a.all().is_empty(), "the control request also touched hub-a");
    }

    /// A read-only gateway is read-only for BOTH tenants — the posture
    /// is a property of the deployment, not of a share.
    #[tokio::test]
    async fn read_only_applies_to_every_hub_behind_the_gateway() {
        let r = two_hub_rig(true).await;
        let c = outside();
        for project in ["proj-a", "proj-b"] {
            let res = c
                .put(format!("{}/v1/projects/{project}/files/content?path=/a.txt", r.base))
                .bearer_auth(INBOUND)
                .body("x")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 403, "{project}");
        }
        assert!(r.a.all().is_empty() && r.b.all().is_empty());

        // CONTROL: reads still reach both hubs.
        for (project, want) in [("proj-a", "hub-a"), ("proj-b", "hub-b")] {
            let res = c
                .get(format!("{}/v1/projects/{project}/files?path=/", r.base))
                .bearer_auth(INBOUND)
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 200);
            assert!(res.text().await.unwrap().contains(want));
        }
    }

    /// An unauthenticated external caller reaches neither hub, and
    /// cannot tell the two projects apart.
    #[tokio::test]
    async fn an_external_caller_without_a_token_reaches_neither_hub() {
        let r = two_hub_rig(false).await;
        let c = outside();
        for project in ["proj-a", "proj-b", "no-such-project"] {
            let res = c
                .get(format!("{}/v1/projects/{project}/files?path=/", r.base))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 401, "{project}");
        }
        assert!(r.a.all().is_empty() && r.b.all().is_empty());
    }
}
