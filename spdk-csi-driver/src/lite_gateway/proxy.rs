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
use super::resolve::{self, Decision, Door, Lookup, Refusal, ShareView};
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

/// Which share a request addressed: a project, and optionally one of
/// its volumes.
type Target = (String, Option<String>);

/// The addressing prefix, in both shapes.
///
/// **The `/volumes/` branch is first, and the order is load-bearing.**
/// warp commits to a branch when it matches, so with the bare shape
/// first, `/v1/projects/p/volumes/data/files` would bind
/// `project = "p"` and leave `volumes/data/files` as an unmatched tail.
/// Volumes-first also handles a project literally named `volumes`
/// correctly, because the branch needs the LITERAL `volumes` in the
/// fourth position and a project id there fails it.
fn scope() -> impl Filter<Extract = (Target,), Error = Rejection> + Clone {
    warp::path!("v1" / "projects" / String / "volumes" / String / ..)
        .map(|p: String, v: String| (p, Some(v)))
        .or(warp::path!("v1" / "projects" / String / ..).map(|p: String| (p, None)))
        .unify()
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

    // A project's volumes. NOT under `scope()` — it is a statement
    // about the project rather than about one of its volumes, and
    // nesting it under the volume prefix would make the answer to
    // "which volumes are there" require already knowing one.
    let volumes = {
        let gw = gw.clone();
        warp::path!("v1" / "projects" / String / "volumes")
            .and(warp::get())
            .and(a.clone())
            .then(move |p: String, _| {
                let gw = gw.clone();
                async move { list_volumes(gw, p).await }
            })
    };

    // Wake / keepalive. A CONTROL operation: it reads the CR, stamps
    // one annotation and reads the CR again. It never dials a hub —
    // which is the point, because a file-API call counts as activity
    // and this endpoint exists precisely for callers that are NOT
    // making file calls.
    let wake = {
        let gw = gw.clone();
        scope()
            .and(warp::path!("wake"))
            .and(warp::post())
            .and(a.clone())
            .then(move |t: Target, _| {
                let gw = gw.clone();
                async move { wake_share(gw, t.0, t.1).await }
            })
    };

    let list = {
        let gw = gw.clone();
        scope()
            .and(warp::path!("files"))
            .and(warp::get())
            .and(a.clone())
            .and(query())
            .then(move |t: Target, _, q: Vec<(String, String)>| {
                let gw = gw.clone();
                async move { serve(gw, t.0, t.1, Verb::List, q, &[], Payload::None).await }
            })
    };

    let download = {
        let gw = gw.clone();
        scope()
            .and(warp::path!("files" / "content"))
            .and(warp::get())
            .and(a.clone())
            .and(query())
            .and(warp::header::headers_cloned())
            .then(move |t: Target, _, q: Vec<(String, String)>, h: warp::http::HeaderMap| {
                let gw = gw.clone();
                async move {
                    let fwd = pick_headers(&h, Verb::Download);
                    serve(gw, t.0, t.1, Verb::Download, q, &fwd, Payload::None).await
                }
            })
    };

    let upload = {
        let gw = gw.clone();
        let limit = gw.cfg.max_upload_bytes;
        scope()
            .and(warp::path!("files" / "content"))
            .and(warp::put())
            .and(a.clone())
            .and(query())
            .and(warp::header::headers_cloned())
            .and(warp::body::content_length_limit(limit))
            .and(warp::body::stream())
            // `body` is deliberately un-annotated: warp's body stream is
            // an opaque type, and naming it would pin an implementation
            // detail of warp into this file.
            .then(move |t: Target, _, q: Vec<(String, String)>, h: warp::http::HeaderMap, body| {
                let gw = gw.clone();
                async move {
                    let fwd = pick_headers(&h, Verb::Upload);
                    let body = stream_body(body);
                    serve(gw, t.0, t.1, Verb::Upload, q, &fwd, Payload::Stream(body)).await
                }
            })
    };

    let delete = {
        let gw = gw.clone();
        scope()
            .and(warp::path!("files" / "content"))
            .and(warp::delete())
            .and(a.clone())
            .and(query())
            .and(warp::header::headers_cloned())
            .then(move |t: Target, _, q: Vec<(String, String)>, h: warp::http::HeaderMap| {
                let gw = gw.clone();
                async move {
                    let fwd = pick_headers(&h, Verb::Delete);
                    serve(gw, t.0, t.1, Verb::Delete, q, &fwd, Payload::None).await
                }
            })
    };

    let folder = {
        let gw = gw.clone();
        scope()
            .and(warp::path!("files" / "folder"))
            .and(warp::post())
            .and(a.clone())
            // The query is parsed even though `Verb::Folder` forwards
            // none of it: `wake` is a GATEWAY control, and a documented
            // parameter that is silently ignored on two of six routes is
            // a footgun rather than a simplification.
            .and(query())
            .and(warp::header::headers_cloned())
            .and(warp::body::content_length_limit(64 * 1024))
            .and(warp::body::bytes())
            .then(move |t: Target, _, q: Vec<(String, String)>, h: warp::http::HeaderMap, b: Bytes| {
                let gw = gw.clone();
                async move {
                    let fwd = pick_headers(&h, Verb::Folder);
                    serve(gw, t.0, t.1, Verb::Folder, q, &fwd, Payload::Buffered(b)).await
                }
            })
    };

    let mv = {
        let gw = gw.clone();
        scope()
            .and(warp::path!("files" / "move"))
            .and(warp::post())
            .and(a)
            .and(query())
            .and(warp::header::headers_cloned())
            .and(warp::body::content_length_limit(64 * 1024))
            .and(warp::body::bytes())
            .then(move |t: Target, _, q: Vec<(String, String)>, h: warp::http::HeaderMap, b: Bytes| {
                let gw = gw.clone();
                async move {
                    let fwd = pick_headers(&h, Verb::Move);
                    serve(gw, t.0, t.1, Verb::Move, q, &fwd, Payload::Buffered(b)).await
                }
            })
    };

    healthz
        .or(readyz)
        .unify()
        .or(volumes)
        .unify()
        .or(wake)
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WakeReply {
    project: String,
    volume: String,
    phase: String,
    /// `host:2049` — what a consumer mounts. Absent until there is
    /// something to mount.
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<String>,
    /// **Compare it across wakes.** Stable across an ordinary restart;
    /// DIFFERENT after a hibernate, because that deletes the PVC — and
    /// then every stateid a client holds is invalid, so the correct
    /// response is a remount rather than resuming on the old handles.
    #[serde(skip_serializing_if = "Option::is_none")]
    server_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_endpoint: Option<String>,
    /// Whether this call stamped the wake annotation.
    requested: bool,
    /// Seconds of quiet after which the idle ladder may suspend this
    /// share. Absent = the ladder is off for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    suspend_after_secs: Option<u64>,
    /// How often to POST this endpoint again while holding a mount.
    /// Absent = never needed.
    #[serde(skip_serializing_if = "Option::is_none")]
    keepalive_secs: Option<u64>,
    /// Set when this share can be scaled to zero out from under a live
    /// mount. **A mounting consumer should read this and act on it** —
    /// see `resolve::mount_hazard`.
    #[serde(skip_serializing_if = "Option::is_none")]
    mount_warning: Option<String>,
}

/// `POST /v1/projects/{id}[/volumes/{v}]/wake` — bring a share up, and
/// keep it up.
///
/// **It is a keepalive as much as a wake, and that is deliberate.**
/// The idle ladder suspends only when TWO signals agree: the front
/// door's `chert.us/requested-at` is stale AND the hub's own activity
/// clock says idle. A consumer doing file I/O is held up by the second
/// signal for free. A consumer that is NOT doing I/O — an agent
/// computing in memory for twenty minutes with a mount held open, which
/// is exactly the case `idle::decide` calls out — has only the first,
/// and nothing else in the system will stamp it.
///
/// So this stamps on EVERY call, including when the share is already
/// Ready. That is one small write per call, against a share whose
/// alternative is being scaled to zero underneath a live `hard` mount.
///
/// It never dials the hub. Doing so would work, and would also make the
/// endpoint self-defeating for the file-proxy path: file-API calls
/// count as activity, so a "did it come up" probe through `/files`
/// would keep alive every share it ever checked.
async fn wake_share(
    gw: Arc<Gateway>,
    project: String,
    volume: Option<String>,
) -> warp::reply::Response {
    if let Err(bad) = resolve::validate_project_id(&project) {
        return json_err(StatusCode::BAD_REQUEST, "BadProjectId", bad.message(), None);
    }
    if let Some(v) = volume.as_deref() {
        if let Err(bad) = resolve::validate_project_id(v) {
            return json_err(StatusCode::BAD_REQUEST, "BadVolumeId", bad.message(), None);
        }
    }
    let view = match look_up(&gw, &project, volume.as_deref()) {
        Ok(v) => v,
        Err(res) => return *res,
    };

    // An admin suspend is refused BEFORE anything is stamped: the CRD
    // is explicit that a wake request does not override it, so stamping
    // would leave an annotation that means nothing and reads like a
    // pending wake to whoever looks next.
    if let Decision::Refuse(r) = resolve::decide(&view) {
        if r.reason == "AdminSuspended" || r.status == 410 || r.status == 409 {
            return from_refusal(&r);
        }
    }

    if let Err(res) = arm_wake(&gw, &view).await {
        return *res;
    }

    // Wait for it, on the CR.
    let view = match wait_for_ready(&gw, &project, volume.as_deref(), Door::Nfs).await {
        Ok(v) => v,
        Err(res) => return *res,
    };

    let body = WakeReply {
        volume: resolve::volume_id_of_view(&view),
        project,
        phase: view.phase.as_ref().map(|p| format!("{p:?}")).unwrap_or_default(),
        address: view.address.clone(),
        server_id: view.server_id.clone(),
        api_endpoint: view.api_endpoint.clone(),
        requested: true,
        suspend_after_secs: view.suspend_after_secs,
        keepalive_secs: resolve::keepalive_secs(&view),
        mount_warning: resolve::mount_hazard(&view),
    };
    // `Door::Nfs` already refused an absent address, so reaching here
    // with none is not possible — but saying 200 with no `address`
    // would send a caller to parse a field that is not there, and a
    // belt-and-braces check on the field a mount depends on is cheap.
    if body.address.is_none() {
        return json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "NoAddress",
            "the share is up but has published no NFS address yet",
            Some(5),
        );
    }
    warp::reply::json(&body).into_response()
}

#[derive(Serialize)]
struct VolumeListing {
    project: String,
    volumes: Vec<resolve::VolumeRow>,
}

/// `GET /v1/projects/{id}/volumes` — what a UI needs to render a
/// project that has more than one hub.
///
/// Answered entirely from the reflector, so listing a project costs no
/// API-server call and — importantly — **touches no hub**. Asking "what
/// volumes does this project have" must not wake anything, and it must
/// not count as activity against the idle ladder.
async fn list_volumes(gw: Arc<Gateway>, project: String) -> warp::reply::Response {
    if let Err(bad) = resolve::validate_project_id(&project) {
        return json_err(StatusCode::BAD_REQUEST, "BadProjectId", bad.message(), None);
    }
    let fleet = gw.store.state();
    let found = resolve::shares_of(
        &fleet,
        &gw.cfg.share_name_prefix,
        &project,
        gw.cfg.namespace.as_deref(),
    );
    if found.is_empty() {
        return json_err(
            StatusCode::NOT_FOUND,
            "NoSuchProject",
            "no share is registered for that project",
            None,
        );
    }
    let mut volumes: Vec<resolve::VolumeRow> =
        found.iter().map(|s| resolve::volume_row(s)).collect();
    // Stable order. The reflector's is not, and a UI that re-renders a
    // reordered list on every poll looks broken.
    volumes.sort_by(|a, b| a.volume.cmp(&b.volume));
    warp::reply::json(&VolumeListing { project, volumes }).into_response()
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
#[allow(clippy::too_many_arguments)]
async fn serve(
    gw: Arc<Gateway>,
    project: String,
    volume: Option<String>,
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
    let may_wake = match route::wake_allowed(&q) {
        Ok(v) => v,
        Err(why) => return json_err(StatusCode::BAD_REQUEST, "BadWakeParam", &why, None),
    };

    if let Some(v) = volume.as_deref() {
        // A volume id becomes part of a label match, never part of an
        // object name — but it is still caller input, so it is
        // validated on the same terms as the project id rather than
        // trusted because it is "only" a selector.
        if let Err(bad) = resolve::validate_project_id(v) {
            return json_err(StatusCode::BAD_REQUEST, "BadVolumeId", bad.message(), None);
        }
    }

    let mut view = match look_up(&gw, &project, volume.as_deref()) {
        Ok(v) => v,
        Err(res) => return *res,
    };

    match resolve::decide(&view) {
        Decision::Dial(_) => {}
        Decision::Refuse(r) => return from_refusal(&r),
        // `wake=false` refuses BOTH the arming and the wait. A crawl
        // over a fleet must not block on 300 shares coming up any more
        // than it should start 2700 that were not.
        Decision::Wake | Decision::Wait if !may_wake => {
            let phase = view
                .phase
                .as_ref()
                .map(|p| format!("{p:?}"))
                .unwrap_or_else(|| "unreported".into());
            return json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "Parked",
                &format!(
                    "this share is {phase} and you asked not to wake it (wake=false). \
                     It will not come back on its own; retry without wake=false, or POST \
                     .../wake."
                ),
                // Deliberately NO Retry-After: nothing is on a timer
                // here, and a Retry-After would tell a crawler to come
                // back and find exactly the same thing.
                None,
            );
        }
        Decision::Wake => {
            if let Err(res) = arm_wake(&gw, &view).await {
                return *res;
            }
            match wait_for_ready(&gw, &project, volume.as_deref(), Door::FileApi).await {
                Ok(v) => view = v,
                Err(res) => return *res,
            }
        }
        Decision::Wait => {
            match wait_for_ready(&gw, &project, volume.as_deref(), Door::FileApi).await {
                Ok(v) => view = v,
                Err(res) => return *res,
            }
        }
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

/// Find the addressed share in the reflector, as a [`ShareView`].
///
/// `volume` is `None` for the `/v1/projects/{id}/files*` shape, which
/// serves only when the project has exactly one volume.
fn look_up(
    gw: &Gateway,
    project: &str,
    volume: Option<&str>,
) -> Result<ShareView, Box<warp::reply::Response>> {
    let fleet = gw.store.state();
    match resolve::find(
        &fleet,
        &gw.cfg.share_name_prefix,
        project,
        volume,
        gw.cfg.namespace.as_deref(),
    ) {
        Lookup::Found(s) => Ok(ShareView::of(&s)),
        Lookup::NotFound => Err(Box::new(json_err(
            StatusCode::NOT_FOUND,
            match volume {
                Some(_) => "NoSuchVolume",
                None => "NoSuchProject",
            },
            match volume {
                Some(v) => format!("this project has no volume named {v:?}"),
                None => "no share is registered for that project".to_string(),
            }
            .as_str(),
            None,
        ))),
        // ACTIONABLE, and answered as such. A project with several
        // volumes is an ordinary configuration — nothing in the
        // operator forbids it — so a request that did not name one is
        // under-specified rather than wrong, and the caller gets the
        // list it needs to ask again.
        Lookup::NeedsVolume(vols) => Err(Box::new(volumes_reply(
            StatusCode::CONFLICT,
            "MultipleVolumes",
            &format!(
                "this project has {} volumes; address one as \
                 /v1/projects/{project}/volumes/<volume>/files…",
                vols.len()
            ),
            &vols,
        ))),
        Lookup::Ambiguous(who) => {
            // Loud on the operator's side, vague on the caller's: the
            // caller cannot fix this and does not need the namespaces.
            tracing::error!(
                project = %project,
                volume = ?volume,
                candidates = ?who,
                "two or more shares claim one (project, volume) — refusing to guess"
            );
            Err(Box::new(json_err(
                StatusCode::CONFLICT,
                "AmbiguousVolume",
                "more than one share claims that project and volume; refusing to guess \
                 between them",
                None,
            )))
        }
    }
}

#[derive(Serialize)]
struct VolumesBody {
    error: String,
    reason: String,
    volumes: Vec<String>,
}

/// A refusal that carries the choice the caller has to make.
fn volumes_reply(
    status: StatusCode,
    reason: &str,
    msg: &str,
    volumes: &[String],
) -> warp::reply::Response {
    let mut res = warp::reply::json(&VolumesBody {
        error: msg.to_string(),
        reason: reason.to_string(),
        volumes: volumes.to_vec(),
    })
    .into_response();
    *res.status_mut() = status;
    res
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
async fn wait_for_ready(
    gw: &Gateway,
    project: &str,
    volume: Option<&str>,
    door: Door,
) -> Result<ShareView, Box<warp::reply::Response>> {
    let deadline = tokio::time::Instant::now() + gw.cfg.wake_wait;
    loop {
        let view = look_up(gw, project, volume)?;
        match resolve::decide_for(&view, door) {
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
                &format!(
                    "the hub rejected this gateway's credential. {}. A streamed upload \
                     cannot be replayed, so no retry was attempted.",
                    credential_advice(gw, view)
                ),
                Some(5),
            );
        };
        let Some(prev) = gw.minter.previous_token_for(view.binding()) else {
            return json_err(
                StatusCode::BAD_GATEWAY,
                "HubRejectedCredential",
                &format!(
                    "the hub rejected this gateway's credential. {}",
                    credential_advice(gw, view)
                ),
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

/// Why a hub most likely rejected us, and the binding we used.
///
/// **The first version of this message blamed a token rotation, and
/// that was the wrong diagnosis in the common case.** A rotation is
/// rare and self-heals; a provisioning mismatch is neither, and it
/// produces exactly the same 401. The drill that first hit this had no
/// rotation anywhere — its provisioning step had omitted
/// `spec.endpoint` from the binding, which is a legal empty value that
/// nothing complains about and that no hub will ever accept.
///
/// So the binding is NAMED. None of it is secret — endpoint, bucket,
/// prefix and version are all readable from the CR by anyone who can
/// read the CR — and without it the operator has nothing to compare
/// against the Secret they wrote.
fn credential_advice(gw: &Gateway, view: &ShareView) -> String {
    match (&gw.minter, view.binding()) {
        (Minter::Shared(_), _) => "This gateway uses a single shared hub token; the share's \
             Secret holds a different value."
            .to_string(),
        (Minter::Derived(_), Ok(b)) => format!(
            "This gateway DERIVES tokens. Check that the share's token Secret was written \
             from the same root key and the same binding: endpoint={:?} bucket={:?} \
             keyPrefix={:?} version={}. `flint-hub-gateway --derive-for {}/{}` prints what \
             this gateway expects. Note that spec.endpoint is part of the binding and is \
             MUTABLE — changing it invalidates every token derived before the change.",
            b.endpoint, b.bucket, b.key_prefix, b.version, view.namespace, view.name
        ),
        (Minter::Derived(_), Err(_)) => "This gateway derives tokens, but this share has no \
             bucket to bind one to."
            .to_string(),
    }
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
        /// What framing the upstream request actually used. A body
        /// carrying BOTH a Content-Length and `chunked` is a malformed
        /// request that some servers accept and others treat as request
        /// smuggling, so the fix for the 411 has to produce one or the
        /// other — never both.
        content_length: Option<String>,
        transfer_encoding: Option<String>,
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
        let reply = move |m: warp::http::Method,
                          p: warp::path::FullPath,
                          q: String,
                          h: HeaderMap,
                          b: Bytes,
                          log: HubLog| {
            let hdr = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(String::from);
            let cold = q.contains("cold.bin");
            log.0.lock().unwrap().push(Seen {
                method: m.to_string(),
                path: p.as_str().to_string(),
                query: q,
                auth: hdr("authorization"),
                if_match: hdr("if-match"),
                range: hdr("range"),
                content_length: hdr("content-length"),
                transfer_encoding: hdr("transfer-encoding"),
                body: b.to_vec(),
            });
            // Every body NAMES THE HUB THAT SERVED IT. That is what
            // makes a cross-routed request visible in the response a
            // caller sees, rather than only in a log the test happens
            // to check.
            // A COLD FILE. The real hub answers a download of an
            // evicted file `503 {"error":"Delay","nfs_status":"Delay"}`
            // with a `Retry-After`, once `hydrateWaitSecs` is spent
            // (`fileapi::err_reply`). Nothing in this rig could produce
            // that shape before, so nothing tested whether the gateway
            // hands BOTH halves back — and the kind drill found the
            // header missing with the status and body intact.
            if cold {
                let mut res = warp::reply::Response::new(
                    format!(r#"{{"error":"Delay","nfs_status":"Delay","hub":"{name}"}}"#).into(),
                );
                *res.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
                res.headers_mut().insert("retry-after", "2".parse().unwrap());
                res.headers_mut().insert("content-type", "application/json".parse().unwrap());
                res.headers_mut().insert("x-hub-internal", "leaked".parse().unwrap());
                return res;
            }
            let body = if p.as_str() == "/status" {
                // The marker. If this ever reaches a caller the gateway
                // has failed at its whole job.
                format!(r#"{{"phase":"Serving","hub":"{name}","THIS-IS-THE-STATUS-DOCUMENT":true}}"#)
            } else {
                format!(r#"{{"hub":"{name}","entries":[]}}"#)
            };
            let mut res = warp::reply::Response::new(body.into());
            res.headers_mut().insert("etag", "\"abc123\"".parse().unwrap());
            res.headers_mut().insert("content-type", "application/json".parse().unwrap());
            // Something the allowlist does NOT name. A proxy that
            // copies headers wholesale leaks it.
            res.headers_mut().insert("x-hub-internal", "leaked".parse().unwrap());
            res.headers_mut().insert("set-cookie", "hubsession=1".parse().unwrap());
            res
        };

        // THE CONTENT-LENGTH CHECK IS EXPLICIT, and it has to be.
        //
        // The real hub guards its PUT with
        // `warp::body::content_length_limit`, which REFUSES a request
        // carrying no Content-Length — 411, before the handler runs.
        // This fake hub originally used a bare `bytes()` on every
        // method, strictly more permissive than the product, so the
        // gateway's streaming upload (`Body::wrap_stream` has unknown
        // length, so hyper frames it `Transfer-Encoding: chunked`)
        // sailed through every test here and 411'd against a real hub
        // on the very first upload of the kind drill.
        //
        // The obvious fix — a `content_length_limit` PUT branch
        // `.or()`ed with a permissive one — DOES NOT WORK, and the
        // reason is worth writing down: warp's `or` BACKTRACKS on
        // rejection, so a 411 from the first branch is swallowed and
        // the permissive branch answers anyway. The test went green
        // again while testing nothing. Checking the header in the
        // handler is the only version that cannot be backtracked
        // around.
        let hub = warp::method()
            .and(warp::path::full())
            .and(warp::filters::query::raw().or(warp::any().map(String::new)).unify())
            .and(warp::header::headers_cloned())
            .and(warp::body::bytes())
            .map(move |m: warp::http::Method,
                       p: warp::path::FullPath,
                       q: String,
                       h: HeaderMap,
                       b: Bytes| {
                if m == warp::http::Method::PUT && !h.contains_key("content-length") {
                    let mut res = warp::reply::Response::new(
                        "A content-length header is required".into(),
                    );
                    *res.status_mut() = StatusCode::LENGTH_REQUIRED;
                    return res;
                }
                reply(m, p, q, h, b, log.clone())
            });

        let (addr, srv) = warp::serve(hub).bind_ephemeral(([127, 0, 0, 1], 0));
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
            "apiVersion": "chert.us/v1alpha1", "kind": "FlintShare",
            "metadata": {
                "name": name, "namespace": "workspaces",
                "labels": {"chert.us/project-id": project},
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

    /// THE 411 THE KIND DRILL FOUND.
    ///
    /// The gateway streams an upload rather than buffering it, and a
    /// streamed body has no known length — so hyper frames it
    /// `Transfer-Encoding: chunked`. The hub's upload route guards
    /// itself with `warp::body::content_length_limit`, which REFUSES a
    /// request with no Content-Length: 411, before the handler runs.
    /// Every upload in the fleet failed, and no test here saw it,
    /// because this file's fake hub used a bare `bytes()` and was more
    /// permissive than the product.
    ///
    /// Both halves are asserted: the length IS forwarded, and the
    /// request is NOT also chunked. A body carrying both is malformed
    /// and is treated as request smuggling by some servers.
    #[tokio::test]
    async fn an_upload_reaches_the_hub_with_a_content_length_and_not_chunked() {
        let r = ready_rig().await;
        let res = req()
            .method("PUT")
            .path("/v1/projects/proj-a/files/content?path=/a.txt")
            .body("0123456789")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 200, "the hub refused the upload framing");
        let seen = &r.log.all()[0];
        assert_eq!(
            seen.content_length.as_deref(),
            Some("10"),
            "the caller's Content-Length was not forwarded — the hub answers 411"
        );
        assert_eq!(
            seen.transfer_encoding, None,
            "the request carried BOTH a length and chunked framing"
        );
        assert_eq!(seen.body, b"0123456789", "and the bytes still arrived intact");
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

    /// A COLD READ IS A RETRYABLE 503, AND BOTH HALVES HAVE TO ARRIVE.
    ///
    /// A download of an evicted file makes the hub pull it back from S3.
    /// Past `hydrateWaitSecs` it gives up and answers
    /// `503 {"error":"Delay"}` with a `Retry-After` — a normal outcome a
    /// browse UI handles by asking again. The status alone is not enough:
    /// without the header the caller cannot tell "coming, ask again"
    /// from "this hub is broken", and the only safe reading of a bare
    /// 503 is the second one.
    ///
    /// `retry-after` has been in `RESPONSE_HEADERS` since the first
    /// commit and `relay` copies the whole allowlist, so this test
    /// should never have been able to fail — which is exactly why it is
    /// worth having. It did not exist until the kind drill asked the
    /// question, and nothing else here can: every other fake-hub answer
    /// is a 200.
    #[tokio::test]
    async fn a_hubs_503_reaches_the_caller_with_its_retry_after() {
        let r = ready_rig().await;
        let res = req()
            .method("GET")
            .path("/v1/projects/proj-a/files/content?path=/cold.bin")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 503, "a hub's 503 must be relayed, not rewritten");
        assert_eq!(
            res.headers().get("retry-after").map(|v| v.to_str().unwrap()),
            Some("2"),
            "the hub's Retry-After was dropped — a caller cannot tell a hydrating file \
             from a broken hub"
        );
        // It is the HUB's 503 and not one the gateway made up. Those
        // two are indistinguishable by status alone, and they mean
        // opposite things about where the fault is.
        let body = String::from_utf8_lossy(res.body()).to_string();
        assert!(body.contains("hub-a"), "the body did not come from the hub: {body}");
        assert!(body.contains("Delay"), "the hub's error was rewritten: {body}");
        // The allowlist still applies on the error path.
        assert!(!res.headers().contains_key("x-hub-internal"), "leaked an internal header");

        // ANTI-VACUITY: the same gateway, the same hub, an ordinary
        // path. Without this, a gateway that answered 503 to everything
        // would pass the assertions above.
        let ok = req()
            .method("GET")
            .path("/v1/projects/proj-a/files?path=/")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(ok.status(), 200, "the rig cannot serve anything at all");
        assert!(
            ok.headers().get("retry-after").is_none(),
            "a 200 carried a Retry-After — the header is not coming from the hub"
        );
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

    /// THE FLEET-CRAWL GUARD.
    ///
    /// A project service listing every project is a different caller
    /// from a person clicking one. At the design fleet size — 3000
    /// shares, ~300 live — a crawl that woke what it touched would
    /// start 2700 hubs, and for the `Hibernated` ones that is a full DR
    /// import from S3 each: real billed egress, a thundering herd
    /// against an operator bounded to 32 concurrent reconciles, and
    /// then 2700 hubs sitting awake until the ladder walks them back
    /// down.
    ///
    /// `wake=false` refuses instead — and refuses the WAIT too, so a
    /// crawl does not block on shares that are already coming up.
    #[tokio::test]
    async fn wake_false_refuses_a_parked_share_instead_of_starting_it() {
        for phase in ["IdleSuspended", "Hibernated", "Pending", "Starting"] {
            let r = rig(
                vec![share_json("fs-proj-a", "proj-a", phase, Some("HUB"))],
                false,
                // A wake budget that would be VERY obvious if it were
                // waited out.
                30,
            )
            .await;
            let started = std::time::Instant::now();
            let res = req()
                .method("GET")
                .path("/v1/projects/proj-a/files?path=/&wake=false")
                .reply(&routes(r.gw.clone()))
                .await;
            assert_eq!(res.status(), 503, "{phase}");
            let body = String::from_utf8_lossy(res.body()).to_string();
            assert!(body.contains("Parked"), "{phase}: {body}");
            assert!(body.contains(phase), "{phase}: the caller needs the phase: {body}");
            assert!(
                res.headers().get("retry-after").is_none(),
                "{phase}: a Retry-After would send a crawler back to find the same thing"
            );
            assert!(r.log.all().is_empty(), "{phase}: it dialled a parked hub");
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{phase}: wake=false waited out the wake budget"
            );
        }
    }

    /// `wake=false` must mean the same thing on all six routes. A
    /// documented parameter honoured on four of them is worse than one
    /// that does not exist.
    #[tokio::test]
    async fn every_file_route_honours_wake_false() {
        let cases: Vec<(&str, &str, &str)> = vec![
            ("GET", "/v1/projects/proj-a/files?path=/&wake=false", ""),
            ("GET", "/v1/projects/proj-a/files/content?path=/a&wake=false", ""),
            ("PUT", "/v1/projects/proj-a/files/content?path=/a&wake=false", "x"),
            ("DELETE", "/v1/projects/proj-a/files/content?path=/a&wake=false", ""),
            ("POST", "/v1/projects/proj-a/files/folder?wake=false", "{}"),
            ("POST", "/v1/projects/proj-a/files/move?wake=false", "{}"),
        ];
        for (m, path, body) in cases {
            let r = rig(
                vec![share_json("fs-proj-a", "proj-a", "IdleSuspended", Some("HUB"))],
                false,
                30,
            )
            .await;
            let started = std::time::Instant::now();
            let res = req()
                .method(m)
                .path(path)
                .header("content-type", "application/json")
                .body(body)
                .reply(&routes(r.gw.clone()))
                .await;
            assert_eq!(res.status(), 503, "{m} {path}");
            assert!(
                String::from_utf8_lossy(res.body()).contains("Parked"),
                "{m} {path}: {}",
                String::from_utf8_lossy(res.body())
            );
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{m} {path} waited out the wake budget"
            );
            assert!(r.log.all().is_empty(), "{m} {path} dialled a parked hub");
        }
    }

    /// The default is unchanged, and it has to be: a person clicking a
    /// project expects it to open. Only the crawler opts out.
    #[tokio::test]
    async fn a_parked_share_is_still_woken_when_wake_is_not_disabled() {
        for q in ["", "&wake=true", "&wake=1"] {
            let r = rig(
                vec![share_json("fs-proj-a", "proj-a", "IdleSuspended", Some("HUB"))],
                false,
                1,
            )
            .await;
            let res = req()
                .method("GET")
                .path(&format!("/v1/projects/proj-a/files?path=/{q}"))
                .reply(&routes(r.gw.clone()))
                .await;
            // The wake PATCH fails in this rig (no API server), so this
            // is 503 either way — but the REASON is what distinguishes
            // "I tried to wake it" from "I refused to".
            assert_eq!(res.status(), 503, "{q:?}");
            let body = String::from_utf8_lossy(res.body()).to_string();
            assert!(
                !body.contains("Parked"),
                "{q:?} refused instead of attempting a wake: {body}"
            );
        }
    }

    /// A serving share is unaffected — `wake=false` is about parked
    /// shares, not about refusing traffic.
    #[tokio::test]
    async fn wake_false_still_serves_a_share_that_is_already_up() {
        let r = ready_rig().await;
        let res = req()
            .method("GET")
            .path("/v1/projects/proj-a/files?path=/&wake=false")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 200);
        // And the control parameter did not reach the hub.
        assert_eq!(r.log.all()[0].query, "path=%2F");
    }

    /// A typo must not read as "yes, wake" — that mistake's blast
    /// radius is every parked share in the fleet.
    #[tokio::test]
    async fn an_unreadable_wake_parameter_is_a_400_not_a_default() {
        let r = rig(
            vec![share_json("fs-proj-a", "proj-a", "IdleSuspended", Some("HUB"))],
            false,
            1,
        )
        .await;
        let res = req()
            .method("GET")
            .path("/v1/projects/proj-a/files?path=/&wake=fasle")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 400);
        assert!(String::from_utf8_lossy(res.body()).contains("BadWakeParam"));
        assert!(r.log.all().is_empty());
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

    // ───────────────────────────────────────────────────────────────
    // ONE PROJECT, SEVERAL HUBS
    //
    // Nothing in the operator forbids it. `conflict::overlaps` keys
    // fleet uniqueness on (endpoint, bucket, prefix-subtree) and NOTHING
    // reads `chert.us/project-id` at all — so N shares on N different
    // prefixes, all labelled with one project id, is a legal and
    // unremarkable configuration. (One HUB serving several volumes is a
    // different thing and is not implemented; the model here is one
    // volume, one hub, N of them per project.)
    //
    // The first cut of this gateway assumed project↔share was 1:1 and
    // answered 409 for the whole shape. These tests exist so that
    // assumption cannot come back.
    // ───────────────────────────────────────────────────────────────

    fn volume_share(
        name: &str,
        project: &str,
        volume: &str,
        endpoint: &str,
    ) -> FlintShare {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "chert.us/v1alpha1", "kind": "FlintShare",
            "metadata": {
                "name": name, "namespace": "workspaces",
                "labels": {
                    "chert.us/project-id": project,
                    "chert.us/volume-id": volume,
                },
            },
            "spec": {
                "bucket": "tenant-bucket",
                // DIFFERENT prefixes — which is exactly what makes two
                // shares of one project legal to the arbiter.
                "keyPrefix": format!("{project}/{volume}/"),
                "persistence": {"size": "20Gi"}
            },
            "status": {
                "phase": "Ready",
                "apiEndpoint": endpoint,
                "conditions": [{
                    "type": "ApiEndpointPublished", "status": "True", "reason": "InCluster",
                    "lastTransitionTime": "2026-08-21T00:00:00Z"
                }]
            }
        }))
        .expect("test share")
    }

    /// One project, two volumes, two hubs, on real ports.
    async fn one_project_two_volumes() -> TwoHubs {
        crate::install_crypto_provider();
        let (la, lb) = (HubLog::default(), HubLog::default());
        let (ha, hb) = (
            fake_hub(la.clone(), "hub-a").await,
            fake_hub(lb.clone(), "hub-b").await,
        );
        let shares = vec![
            volume_share("fs-proj-a-data", "proj-a", "data", &ha),
            volume_share("fs-proj-a-models", "proj-a", "models", &hb),
        ];
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
                wake_wait: Duration::from_secs(0),
                read_only: false,
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

    /// The headline for this shape: each volume of one project reaches
    /// its own hub, addressed by volume id, over a real socket.
    #[tokio::test]
    async fn one_project_with_two_volumes_reaches_a_different_hub_for_each() {
        let r = one_project_two_volumes().await;
        let c = outside();
        for (volume, want) in [("data", "hub-a"), ("models", "hub-b")] {
            let res = c
                .get(format!("{}/v1/projects/proj-a/volumes/{volume}/files?path=/", r.base))
                .bearer_auth(INBOUND)
                .send()
                .await
                .expect("the gateway must answer");
            assert_eq!(res.status(), 200, "{volume}");
            let body = res.text().await.unwrap();
            assert!(
                body.contains(&format!(r#""hub":"{want}""#)),
                "volume {volume} was served by the wrong hub: {body}"
            );
        }
        assert_eq!(r.a.paths(), vec!["/files"]);
        assert_eq!(r.b.paths(), vec!["/files"]);
    }

    /// Each volume's hub gets a credential bound to ITS OWN prefix.
    ///
    /// Two volumes of one project are two independent subtrees with two
    /// independent epochs; a shared credential would mean a compromise
    /// of one volume's hub opened the other.
    #[tokio::test]
    async fn each_volume_of_one_project_has_its_own_credential() {
        let r = one_project_two_volumes().await;
        let c = outside();
        for volume in ["data", "models"] {
            c.get(format!("{}/v1/projects/proj-a/volumes/{volume}/files?path=/", r.base))
                .bearer_auth(INBOUND)
                .send()
                .await
                .unwrap();
        }
        let ta = r.a.all()[0].auth.clone().unwrap();
        let tb = r.b.all()[0].auth.clone().unwrap();
        assert_ne!(ta, tb, "two volumes of one project shared a credential");
        // And each is the token for its own prefix, not merely different.
        let want = |vol: &str| {
            format!(
                "Bearer {}",
                super::super::derive::derive(
                    ROOT_KEY,
                    &super::super::derive::Binding {
                        endpoint: "",
                        bucket: "tenant-bucket",
                        key_prefix: &format!("proj-a/{vol}/"),
                        version: 1,
                    },
                )
            )
        };
        assert_eq!(ta, want("data"));
        assert_eq!(tb, want("models"));
    }

    /// The bare `/files` shape on a multi-volume project is
    /// UNDER-SPECIFIED, not wrong — so it names the choice rather than
    /// refusing opaquely, and it dials nothing while doing so.
    ///
    /// Picking one instead would serve `models/` to a caller that meant
    /// `data/`, and the reflector's order is not stable across a watch
    /// reconnect, so it would not even be consistently the same one.
    #[tokio::test]
    async fn a_multi_volume_project_names_the_choice_instead_of_guessing() {
        let r = one_project_two_volumes().await;
        let c = outside();
        let res = c
            .get(format!("{}/v1/projects/proj-a/files?path=/", r.base))
            .bearer_auth(INBOUND)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 409);
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body["reason"], "MultipleVolumes");
        let mut vols: Vec<String> = body["volumes"]
            .as_array()
            .expect("the caller needs the list to act on")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        vols.sort();
        assert_eq!(vols, vec!["data", "models"]);
        assert!(
            r.a.all().is_empty() && r.b.all().is_empty(),
            "it dialled a hub while deciding it could not choose"
        );

        // And the answer is ACTIONABLE: following it works.
        let ok = c
            .get(format!("{}/v1/projects/proj-a/volumes/data/files?path=/", r.base))
            .bearer_auth(INBOUND)
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
        assert!(ok.text().await.unwrap().contains(r#""hub":"hub-a""#));
    }

    /// A single-volume project keeps working with no volume in the
    /// path and no `chert.us/volume-id` label anywhere. This is the
    /// shape every existing caller uses.
    #[tokio::test]
    async fn a_single_volume_project_still_serves_the_bare_path() {
        let r = two_hub_rig(false).await;   // two PROJECTS, one volume each
        let c = outside();
        let res = c
            .get(format!("{}/v1/projects/proj-a/files?path=/", r.base))
            .bearer_auth(INBOUND)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(res.text().await.unwrap().contains(r#""hub":"hub-a""#));
    }

    /// Asking for a volume that does not exist must be a 404, never a
    /// fallback onto whichever volume the project does have. A
    /// fallback here is a caller reading the wrong subtree and being
    /// told everything is fine.
    #[tokio::test]
    async fn an_unknown_volume_is_404_and_never_falls_back_to_another() {
        let r = one_project_two_volumes().await;
        let c = outside();
        let res = c
            .get(format!("{}/v1/projects/proj-a/volumes/nope/files?path=/", r.base))
            .bearer_auth(INBOUND)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body["reason"], "NoSuchVolume");
        assert!(r.a.all().is_empty() && r.b.all().is_empty());
    }

    /// Listing a project's volumes must touch NO hub. "Which volumes
    /// are there" is a question about the CR, and answering it by
    /// dialling would wake parked volumes and count as activity against
    /// the idle ladder for the live ones.
    #[tokio::test]
    async fn listing_a_projects_volumes_touches_no_hub() {
        let r = one_project_two_volumes().await;
        let c = outside();
        let res = c
            .get(format!("{}/v1/projects/proj-a/volumes", r.base))
            .bearer_auth(INBOUND)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body["project"], "proj-a");
        let vols = body["volumes"].as_array().unwrap();
        assert_eq!(vols.len(), 2);
        // Sorted, so a UI does not re-render a shuffled list each poll.
        assert_eq!(vols[0]["volume"], "data");
        assert_eq!(vols[1]["volume"], "models");
        assert_eq!(vols[0]["keyPrefix"], "proj-a/data/");
        assert_eq!(vols[0]["serving"], true);
        assert!(
            r.a.all().is_empty() && r.b.all().is_empty(),
            "listing volumes dialled a hub"
        );

        // Unauthenticated callers cannot enumerate a project's volumes.
        let bare = c
            .get(format!("{}/v1/projects/proj-a/volumes", r.base))
            .send()
            .await
            .unwrap();
        assert_eq!(bare.status(), 401);
    }

    /// A project id that happens to be the literal `volumes` must still
    /// route — the `/volumes/` branch is matched first, so this is the
    /// case that would break if the branch ordering were reasoned about
    /// carelessly.
    #[tokio::test]
    async fn a_project_named_volumes_is_not_swallowed_by_the_volume_route() {
        crate::install_crypto_provider();
        let log = HubLog::default();
        let hub = fake_hub(log.clone(), "hub-v").await;
        let share = volume_share("fs-volumes", "volumes", "only", &hub);
        let client = kube::Client::try_from(kube::Config::new(
            "http://127.0.0.1:1".parse().unwrap(),
        ))
        .unwrap();
        let gw = Arc::new(Gateway {
            client,
            store: store_of(vec![share]),
            cfg: Config {
                namespace: None,
                share_name_prefix: "fs-".into(),
                wake_wait: Duration::from_secs(0),
                read_only: false,
                max_upload_bytes: 1024 * 1024,
                upstream_timeout: Duration::from_secs(5),
            },
            minter: Minter::Shared("t".into()),
            inbound: TokenSource::fixed(INBOUND),
            http: upstream_client(Duration::from_secs(2)).unwrap(),
            ready: Arc::new(AtomicBool::new(true)),
        });
        let (addr, srv) = warp::serve(routes(gw)).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(srv);
        let c = outside();
        let res = c
            .get(format!("http://{addr}/v1/projects/volumes/files?path=/"))
            .bearer_auth(INBOUND)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(res.text().await.unwrap().contains(r#""hub":"hub-v""#));
        assert_eq!(log.paths(), vec!["/files"]);
    }

    // ───────────────────────────────────────────────────────────────
    // THE WAKE ENDPOINT, DRIVEN THROUGH THE ROUTE TABLE
    //
    // Everything above tests `decide_for` — the DECISION. None of it
    // executes `wake_share`, which is the code that actually patches
    // the annotation and shapes the reply. So these stand up a fake API
    // SERVER as well as fake hubs, point a real `kube::Client` at it,
    // and assert on what it received.
    //
    // That matters for one property in particular: the patch must be a
    // MERGE patch touching one annotation. Server-side apply would make
    // the gateway a field owner and start a tug-of-war with whatever
    // front door also writes `chert.us/requested-at` — and nothing
    // short of watching the request body catches that.
    // ───────────────────────────────────────────────────────────────

    #[derive(Clone, Default)]
    struct ApiLog(Arc<Mutex<Vec<(String, String, String)>>>); // method, path, body

    impl ApiLog {
        fn patches(&self) -> Vec<(String, String, String)> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, _, _)| m == "PATCH")
                .cloned()
                .collect()
        }
    }

    /// A stand-in API server that accepts the wake patch.
    async fn fake_apiserver(log: ApiLog, share: FlintShare) -> String {
        let body = serde_json::to_string(&share).expect("share serialises");
        let route = warp::method()
            .and(warp::path::full())
            .and(warp::header::headers_cloned())
            .and(warp::body::bytes())
            .map(move |m: warp::http::Method,
                       p: warp::path::FullPath,
                       h: HeaderMap,
                       b: Bytes| {
                log.0.lock().unwrap().push((
                    m.to_string(),
                    // The content type IS the assertion for merge-vs-SSA,
                    // so it rides along with the path.
                    format!(
                        "{} [{}]",
                        p.as_str(),
                        h.get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                    ),
                    String::from_utf8_lossy(&b).to_string(),
                ));
                let mut res = warp::reply::Response::new(body.clone().into());
                res.headers_mut()
                    .insert("content-type", "application/json".parse().unwrap());
                res
            });
        let (addr, srv) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(srv);
        format!("http://{addr}")
    }

    struct WakeRig {
        gw: Arc<Gateway>,
        hub: HubLog,
        api: ApiLog,
    }

    async fn wake_rig(share: FlintShare) -> WakeRig {
        crate::install_crypto_provider();
        let hub = HubLog::default();
        let hub_url = fake_hub(hub.clone(), "hub-a").await;
        let mut share = share;
        if let Some(st) = share.status.as_mut() {
            if st.api_endpoint.as_deref() == Some("HUB") {
                st.api_endpoint = Some(hub_url);
            }
        }
        let api = ApiLog::default();
        let api_url = fake_apiserver(api.clone(), share.clone()).await;
        let client = kube::Client::try_from(kube::Config::new(
            api_url.parse().expect("uri"),
        ))
        .expect("client");
        let gw = Arc::new(Gateway {
            client,
            store: store_of(vec![share]),
            cfg: Config {
                namespace: None,
                share_name_prefix: "fs-".into(),
                wake_wait: Duration::from_secs(0),
                read_only: false,
                max_upload_bytes: 1024 * 1024,
                upstream_timeout: Duration::from_secs(5),
            },
            minter: Minter::Shared("hub-token".into()),
            inbound: TokenSource::fixed(INBOUND),
            http: upstream_client(Duration::from_secs(2)).expect("http"),
            ready: Arc::new(AtomicBool::new(true)),
        });
        WakeRig { gw, hub, api }
    }

    fn mountable(phase: &str, file_api: bool, idle: Option<(u64, Option<bool>)>) -> FlintShare {
        let mut status = serde_json::json!({
            "phase": phase,
            "address": "10.96.1.7:2049",
            "serverId": "srv-1",
        });
        if file_api {
            status["apiEndpoint"] = serde_json::json!("HUB");
            status["conditions"] = serde_json::json!([{
                "type": "ApiEndpointPublished", "status": "True", "reason": "InCluster",
                "lastTransitionTime": "2026-08-21T00:00:00Z"
            }]);
        }
        let mut spec = serde_json::json!({"persistence": {"size": "1Gi"}});
        if let Some((after, with_sessions)) = idle {
            let mut i = serde_json::json!({"suspendAfterSecs": after});
            if let Some(w) = with_sessions {
                i["suspendWithSessions"] = serde_json::json!(w);
            }
            spec["idle"] = i;
        }
        serde_json::from_value(serde_json::json!({
            "apiVersion": "chert.us/v1alpha1", "kind": "FlintShare",
            "metadata": {
                "name": "fs-proj-a", "namespace": "workspaces",
                "labels": {"chert.us/project-id": "proj-a"},
            },
            "spec": spec,
            "status": status
        }))
        .expect("share")
    }

    /// The happy path, and the shape of the patch.
    #[tokio::test]
    async fn wake_returns_a_mount_address_and_merge_patches_one_annotation() {
        let r = wake_rig(mountable("Ready", true, Some((600, None)))).await;
        let res = req()
            .method("POST")
            .path("/v1/projects/proj-a/wake")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 200, "{}", String::from_utf8_lossy(res.body()));
        let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        assert_eq!(body["address"], "10.96.1.7:2049");
        assert_eq!(body["serverId"], "srv-1");
        assert_eq!(body["phase"], "Ready");
        assert_eq!(body["requested"], true);
        // The agent is told how often to come back WITHOUT having to
        // read the share's spec itself.
        assert_eq!(body["keepaliveSecs"], 300, "half the 600s budget");
        assert_eq!(body["suspendAfterSecs"], 600);
        // …and warned, because this share suspends even with a lease.
        assert!(
            body["mountWarning"].as_str().unwrap().contains("suspendWithSessions"),
            "{body}"
        );

        // ONE patch, MERGE, one annotation, no spec.
        let patches = r.api.patches();
        assert_eq!(patches.len(), 1, "{patches:?}");
        let (_, path, patch_body) = &patches[0];
        assert!(
            path.contains("/apis/chert.us/v1alpha1/namespaces/workspaces/flintshares/fs-proj-a"),
            "{path}"
        );
        assert!(
            path.contains("application/merge-patch+json"),
            "the wake must be a MERGE patch — server-side apply would make the gateway a \
             field owner and fight the front door for the annotation: {path}"
        );
        let j: serde_json::Value = serde_json::from_str(patch_body).unwrap();
        assert!(j["metadata"]["annotations"]["chert.us/requested-at"].is_string(), "{j}");
        assert!(j["spec"].is_null(), "the wake patch must not touch spec: {j}");

        // A wake is a CONTROL operation: it must not dial the hub,
        // because a file-API call counts as activity and this endpoint
        // exists for callers that make none.
        assert!(r.hub.all().is_empty(), "the wake dialled the hub");
    }

    /// THE NFS-ONLY SHARE, end to end through the route table.
    ///
    /// `monitoring.fileApi` is off by default, so this is the ordinary
    /// case for a share that exists to be mounted. The first cut
    /// refused it with `FileApiDisabled`.
    #[tokio::test]
    async fn wake_works_for_a_share_with_no_file_api_at_all() {
        let r = wake_rig(mountable("Ready", false, None)).await;
        let res = req()
            .method("POST")
            .path("/v1/projects/proj-a/wake")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 200, "{}", String::from_utf8_lossy(res.body()));
        let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        assert_eq!(body["address"], "10.96.1.7:2049");
        assert!(body["apiEndpoint"].is_null(), "there is no file API on this share");
        // No ladder configured, so nothing to keep alive against.
        assert!(body["keepaliveSecs"].is_null());
        assert!(body["mountWarning"].is_null());
        assert_eq!(r.api.patches().len(), 1);
    }

    /// An admin suspend is refused BEFORE anything is stamped. A
    /// leftover `requested-at` that means nothing reads like a pending
    /// wake to whoever looks next.
    #[tokio::test]
    async fn wake_refuses_an_admin_suspend_without_stamping_anything() {
        let r = wake_rig(mountable("Suspended", true, None)).await;
        let res = req()
            .method("POST")
            .path("/v1/projects/proj-a/wake")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 409);
        assert!(String::from_utf8_lossy(res.body()).contains("AdminSuspended"));
        assert!(r.api.patches().is_empty(), "it stamped a share it then refused");
        assert!(r.hub.all().is_empty());
    }

    #[tokio::test]
    async fn wake_needs_a_credential_and_refuses_an_unknown_project() {
        let r = wake_rig(mountable("Ready", true, None)).await;
        let bare = warp::test::request()
            .method("POST")
            .path("/v1/projects/proj-a/wake")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(bare.status(), 401);
        assert!(r.api.patches().is_empty(), "an unauthenticated call reached the API server");

        let missing = req()
            .method("POST")
            .path("/v1/projects/nope/wake")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(missing.status(), 404);
        assert!(r.api.patches().is_empty());
    }

    /// It stamps EVERY time, including on a share that is already up.
    /// An endpoint that only stamped when parked would leave the quiet
    /// mount — the case it exists for — exactly as exposed.
    #[tokio::test]
    async fn wake_stamps_again_on_a_share_that_is_already_ready() {
        let r = wake_rig(mountable("Ready", true, Some((60, None)))).await;
        for _ in 0..3 {
            let res = req()
                .method("POST")
                .path("/v1/projects/proj-a/wake")
                .reply(&routes(r.gw.clone()))
                .await;
            assert_eq!(res.status(), 200);
        }
        assert_eq!(r.api.patches().len(), 3, "a keepalive must stamp on every call");
    }

    /// A read-only gateway still wakes. `readOnly` refuses mutating
    /// FILE operations; a browse UI cannot browse a parked project, so
    /// blocking the wake would make the posture unusable.
    #[tokio::test]
    async fn a_read_only_gateway_can_still_wake_a_share() {
        let mut r = wake_rig(mountable("Ready", true, None)).await;
        {
            let gw = Arc::get_mut(&mut r.gw).expect("sole owner");
            gw.cfg.read_only = true;
        }
        let res = req()
            .method("POST")
            .path("/v1/projects/proj-a/wake")
            .reply(&routes(r.gw.clone()))
            .await;
        assert_eq!(res.status(), 200, "a read-only gateway must still be able to wake");
        assert_eq!(r.api.patches().len(), 1);
    }
}
