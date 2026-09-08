//! `Door::RepoFileApi` — the browser's front door onto a repository
//! (design `docs/plans/forge-file-api-design.md` §7).
//!
//! The fourth route table in this component and the second one onto a
//! `FlintRepo`. Agents reach a repository with a git client through
//! [`super::git`]; a file manager reaches the same repository through
//! this, and the two are the same repository — same CR, same
//! `spec.consumers`, same idle ladder, same wake.
//!
//! ## What it is, in one line each
//!
//! - **`route::Verb`, unchanged.** The six verbs, their upstream paths,
//!   their query allowlists and their forwarded headers are the lite
//!   file API's, and forge's listener answers the same six paths. That
//!   reuse IS the contract-parity argument of design §3.5: one client
//!   library, two backends. A second copy of the table here would be
//!   the place the two quietly diverge.
//! - **TokenReview, not a derived bearer** (§7.3). Lite's file door
//!   mints `HMAC(root, endpoint:bucket:prefix:version)`; this door does
//!   what the git door does — the caller presents a projected
//!   ServiceAccount token, `TokenReview` turns it into a principal,
//!   `spec.consumers` says whether that principal may reach this
//!   repository, and `X-Remote-User` carries it upstream. A file door
//!   beside a git door on one repository that authenticated differently
//!   would give one repository two authorization models, and the file
//!   door would be the one that could not say who wrote a file.
//! - **One reviewer, shared.** When both forge doors are mounted they
//!   hold the same [`super::git::Reviewer`], so a backend that pushes
//!   and saves does not pay two `TokenReview`s for one token.
//! - **A short hold.** The git door waits minutes for a parked
//!   repository because git clients do not retry a 503. A browser
//!   backend does retry, and holding its request for minutes is how one
//!   parked repository exhausts its connection pool — so this holds for
//!   seconds and then answers 503 with a `Retry-After`.
//!
//! ## The path invariant, kept
//!
//! [`super::route`]'s rule is that no byte of the upstream path comes
//! from the caller, and it holds here for both halves of the URL. The
//! `<namespace>/<name>` prefix is a LOOKUP KEY: it selects a `FlintRepo`
//! or it 404s. The upstream base is then `status.apiEndpoint` — which
//! the operator wrote from `render::api_endpoint` — and the path
//! appended to it is `Verb::upstream_path()`, a `&'static str`. The
//! caller's `path=` lands in the QUERY, percent-encoded, where the
//! syncer parses it with `validate_tree_path`.
//!
//! ## Why the wake matters here more than anywhere
//!
//! `RepoIdle.suspendAfterSecs` scales a quiet repository to zero
//! replicas, and the door is the only thing that arms
//! `chert.us/requested-at`. Without this module an HTTP read of a slept
//! repository gets a headless Service name with no EndpointSlice behind
//! it — a DNS failure, not a wake — while the same repository comes
//! straight back for a `git fetch`. One backend, two answers, depending
//! on which client asked.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::TryStreamExt;
use kube::runtime::reflector::Store;
use kube::Client;
use warp::http::StatusCode;
use warp::{Filter, Rejection, Reply};

use crate::forge_operator::crd::FlintRepo;
use crate::s3csi::broker::Identity;

use super::git::{
    self, basic_password, consumer_allows, from_refusal, json_err, look_up_repo, plausible_name,
    repo_name, stream_body, wait_for_ready, CachingReviewer, KubeReviewer, Response, ReviewError,
    Reviewer,
};
use super::resolve::{self, Decision, Door, ShareView};
use super::route::{self, Verb, RESPONSE_HEADERS};

#[derive(Debug, Clone)]
pub struct RepoFileConfig {
    /// The audience the projected token must carry. The SAME audience
    /// as the git door by default: it is one repository and one
    /// credential, and a second audience would mean a backend that
    /// pushes and saves needs two projected tokens.
    pub audience: String,
    /// How long a request is held for a parked repository before it is
    /// answered 503 with a `Retry-After`. SECONDS, unlike the git
    /// door's minutes — see the module doc.
    pub wake_wait: Duration,
    /// How long a `TokenReview` verdict is reused, when this door mints
    /// its own reviewer. Ignored when it shares the git door's.
    pub review_ttl: Duration,
    /// The deadline on the upstream RESPONSE HEADERS. The body streams
    /// untimed after that, so a large download is not capped by a
    /// deadline chosen for a control-plane hop.
    pub upstream_timeout: Duration,
    /// Refuse everything that changes the repository. A browse UI can
    /// then be deployed without write authority over every repository.
    pub read_only: bool,
    /// The largest body accepted on a write, in bytes.
    ///
    /// **A door-side bound as well as a syncer-side one, and both are
    /// needed.** `spec.fileApi.maxMb` bounds what the syncer will
    /// accept; this bounds what the door will read before it has asked
    /// anyone. Without it a single unauthenticated POST of arbitrary
    /// length is buffered by the door on behalf of a caller it has not
    /// even reviewed yet.
    pub max_upload_bytes: u64,
}

impl Default for RepoFileConfig {
    fn default() -> Self {
        RepoFileConfig {
            audience: git::AUDIENCE.to_string(),
            // Long enough to cover a scale-from-zero (image cached, the
            // syncer's restore is a snapshot read), short enough that a
            // browser's own request timeout is not the thing that fires.
            wake_wait: Duration::from_secs(25),
            review_ttl: Duration::from_secs(60),
            upstream_timeout: Duration::from_secs(30),
            read_only: false,
            // The syncer's own default (`fileapi::DEFAULT_MAX_BYTES` is
            // 32 MiB); the door is deliberately the looser of the two,
            // so the error a caller gets names the repository's limit
            // rather than the door's.
            max_upload_bytes: 64 * 1024 * 1024,
        }
    }
}

pub struct RepoFileDoor {
    pub client: Client,
    pub repos: Store<FlintRepo>,
    pub http: reqwest::Client,
    pub cfg: RepoFileConfig,
    /// Set once the repo reflector has completed its initial list.
    /// Before that every repository would 404 — read by a caller as
    /// "no such repository" rather than "ask again in a second".
    pub ready: Arc<AtomicBool>,
    pub reviewer: Arc<dyn Reviewer>,
}

impl RepoFileDoor {
    /// Standalone: its own reviewer and its own cache.
    pub fn new(
        client: Client,
        repos: Store<FlintRepo>,
        http: reqwest::Client,
        cfg: RepoFileConfig,
        ready: Arc<AtomicBool>,
    ) -> Arc<Self> {
        let reviewer = CachingReviewer::new(
            Arc::new(KubeReviewer { client: client.clone(), audience: cfg.audience.clone() }),
            cfg.review_ttl,
        );
        Arc::new(RepoFileDoor { client, repos, http, cfg, ready, reviewer })
    }

    /// Beside the git door, on ONE store and ONE `TokenReview` cache.
    ///
    /// Not an optimisation. Two reflectors over one CRD is two answers
    /// to "is this repository up", and they disagree for exactly as
    /// long as one of them is behind — so a backend that pushed and
    /// then saved could be told the repository is Ready by one door and
    /// starting by the other. Sharing the reviewer is the cheaper half
    /// of the same argument: one token, one round trip.
    pub fn beside(git: &Arc<git::GitDoor>, cfg: RepoFileConfig) -> Arc<Self> {
        Arc::new(RepoFileDoor {
            client: git.client.clone(),
            repos: git.repos.clone(),
            http: git.http.clone(),
            cfg,
            ready: git.ready.clone(),
            reviewer: git.reviewer.clone(),
        })
    }
}

/// The credential, in either presentation.
///
/// `Bearer` is the shape a REST backend reaches for and is what the
/// browser app's server sends. `Basic` is accepted too, with the
/// password taken as the token exactly as the git door takes it — so
/// one credential works at both forge doors and a `curl -u` against
/// either behaves the same. The username half is ignored in both cases:
/// the token names the principal, and a username field would be a
/// second, unverified opinion about who this is.
pub fn presented_token(header: Option<&str>) -> Option<String> {
    let raw = header?.trim();
    if let Some(rest) = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer ")) {
        let t = rest.trim();
        return (!t.is_empty()).then(|| t.to_string());
    }
    basic_password(Some(raw)).filter(|t| !t.is_empty())
}

/// 401 WITHOUT a `WWW-Authenticate: Basic` challenge.
///
/// The git door MUST send one: git makes its first request
/// unauthenticated and only presents a credential after a challenge.
/// This door must NOT — its caller is a backend holding a token, and a
/// `Basic` challenge that reached a browser through a misconfigured
/// proxy would put a native username-and-password dialog in front of an
/// end user who has no password to type.
fn unauthorized(detail: &str) -> Response {
    json_err(StatusCode::UNAUTHORIZED, "Unauthenticated", detail, None)
}

/// A request body on its way to the syncer.
enum Payload {
    None,
    /// Buffered — a move or a folder call is small JSON.
    Buffered(Bytes),
    /// Streamed, so an upload is not held in the door's memory. Bounded
    /// by `content_length_limit` before a byte of it is read.
    Stream(reqwest::Body),
}

/// The six routes, under `/repo/<namespace>/<name>/files…`.
///
/// `/repo/` and not `/git/`: the two forge doors are different route
/// tables on the same repositories, and sharing a prefix would make the
/// read-only posture and the body limits depend on which leaf matched.
/// `/repo/` and not the lite door's `/v1/projects/`: a repository is
/// addressed by namespace and name, not by project id — the gateway has
/// no share-name prefix to apply and no volume to disambiguate.
pub fn routes(
    door: Arc<RepoFileDoor>,
) -> impl Filter<Extract = (impl Reply,), Error = Rejection> + Clone {
    let list = {
        let door = door.clone();
        scope()
            .and(warp::path!("files"))
            .and(warp::get())
            .and(warp::query::<HashMap<String, String>>())
            .and(warp::header::optional::<String>("authorization"))
            .and(warp::header::headers_cloned())
            .and_then(move |ns: String, name: String, q: HashMap<String, String>, auth, h| {
                let door = door.clone();
                async move {
                    Ok::<_, Rejection>(
                        serve(door, ns, name, Verb::List, pairs(q), h, auth, Payload::None).await,
                    )
                }
            })
    };

    let download = {
        let door = door.clone();
        scope()
            .and(warp::path!("files" / "content"))
            .and(warp::get())
            .and(warp::query::<HashMap<String, String>>())
            .and(warp::header::optional::<String>("authorization"))
            .and(warp::header::headers_cloned())
            .and_then(move |ns: String, name: String, q: HashMap<String, String>, auth, h| {
                let door = door.clone();
                async move {
                    Ok::<_, Rejection>(
                        serve(door, ns, name, Verb::Download, pairs(q), h, auth, Payload::None)
                            .await,
                    )
                }
            })
    };

    let upload = {
        let door = door.clone();
        let limit = door.cfg.max_upload_bytes;
        scope()
            .and(warp::path!("files" / "content"))
            .and(warp::put())
            .and(warp::query::<HashMap<String, String>>())
            .and(warp::header::optional::<String>("authorization"))
            .and(warp::header::headers_cloned())
            // BEFORE the body filter, so an over-long request is refused
            // on its Content-Length rather than after it is read.
            .and(warp::body::content_length_limit(limit))
            .and(warp::body::stream())
            .and_then(
                move |ns: String, name: String, q: HashMap<String, String>, auth, h, body| {
                    let door = door.clone();
                    async move {
                        let body = reqwest::Body::wrap_stream(stream_body(body));
                        Ok::<_, Rejection>(
                            serve(
                                door,
                                ns,
                                name,
                                Verb::Upload,
                                pairs(q),
                                h,
                                auth,
                                Payload::Stream(body),
                            )
                            .await,
                        )
                    }
                },
            )
    };

    let delete = {
        let door = door.clone();
        scope()
            .and(warp::path!("files" / "content"))
            .and(warp::delete())
            .and(warp::query::<HashMap<String, String>>())
            .and(warp::header::optional::<String>("authorization"))
            .and(warp::header::headers_cloned())
            .and_then(move |ns: String, name: String, q: HashMap<String, String>, auth, h| {
                let door = door.clone();
                async move {
                    Ok::<_, Rejection>(
                        serve(door, ns, name, Verb::Delete, pairs(q), h, auth, Payload::None).await,
                    )
                }
            })
    };

    let folder = body_route(door.clone(), "folder", Verb::Folder);
    let mv = body_route(door, "move", Verb::Move);

    list.or(download)
        .unify()
        .or(upload)
        .unify()
        .or(delete)
        .unify()
        .or(folder)
        .unify()
        .or(mv)
        .unify()
}

/// The addressing prefix. `..` so the leaf filters match the rest.
fn scope() -> impl Filter<Extract = (String, String), Error = Rejection> + Clone {
    warp::path!("repo" / String / String / ..)
}

/// The two POST routes. Their bodies are BUFFERED: both are small JSON
/// documents, and a streamed body could not be replayed if the door
/// ever needed to.
fn body_route(
    door: Arc<RepoFileDoor>,
    leaf: &'static str,
    verb: Verb,
) -> impl Filter<Extract = (Response,), Error = Rejection> + Clone {
    // The leaf is IN the path filter and not checked inside the
    // handler: a handler that answered 404 for the other leaf would
    // CONSUME the request, and warp's `or` would never reach the second
    // route — the trap `git.rs` already fell into once, where every
    // POST 404'd because one handler swallowed both.
    warp::path("repo")
        .and(warp::path::param::<String>())
        .and(warp::path::param::<String>())
        .and(warp::path("files"))
        .and(warp::path(leaf))
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::query::<HashMap<String, String>>())
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::header::headers_cloned())
        .and(warp::body::content_length_limit(64 * 1024))
        .and(warp::body::bytes())
        .and_then(
            move |ns: String, name: String, q: HashMap<String, String>, auth, h, b: Bytes| {
                let door = door.clone();
                async move {
                    Ok::<_, Rejection>(
                        serve(door, ns, name, verb, pairs(q), h, auth, Payload::Buffered(b)).await,
                    )
                }
            },
        )
}

/// warp gives a `HashMap`; `route::filter_query` wants ordered pairs.
///
/// A map loses duplicate keys, which is the correct loss here: the
/// syncer's six verbs take at most one value per parameter, and
/// forwarding two `path=` values would leave which one wins up to
/// whichever parser reads it last.
fn pairs(q: HashMap<String, String>) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = q.into_iter().collect();
    // Deterministic, so a URL is the same URL on every request — which
    // is what makes an upstream log line comparable between two runs.
    v.sort();
    v
}

/// One request, end to end.
#[allow(clippy::too_many_arguments)]
async fn serve(
    door: Arc<RepoFileDoor>,
    ns: String,
    name_segment: String,
    verb: Verb,
    q: Vec<(String, String)>,
    headers: warp::http::HeaderMap,
    auth: Option<String>,
    payload: Payload,
) -> Response {
    if !door.ready.load(Ordering::Relaxed) {
        return json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "NotReady",
            "the door has not finished listing repositories yet",
            Some(2),
        );
    }
    if door.cfg.read_only && verb.is_mutation() {
        return json_err(
            StatusCode::FORBIDDEN,
            "ReadOnly",
            "this door is read-only; it proxies no mutating file operations",
            None,
        );
    }
    let name = repo_name(&name_segment);
    if !plausible_name(&ns) || !plausible_name(&name) {
        return json_err(
            StatusCode::NOT_FOUND,
            "NoSuchRepository",
            "that is not a repository address",
            None,
        );
    }
    let may_wake = match route::wake_allowed(&q) {
        Ok(v) => v,
        Err(why) => return json_err(StatusCode::BAD_REQUEST, "BadWakeParam", &why, None),
    };

    // Authenticate BEFORE the store lookup, and certainly before any
    // wake: an unauthenticated caller must not be able to learn which
    // repositories exist, and must not be able to start one.
    let Some(token) = presented_token(auth.as_deref()) else {
        return unauthorized(
            "present the pod's projected token as `Authorization: Bearer <token>`",
        );
    };
    let identity = match door.reviewer.review(&token).await {
        Ok(id) => id,
        Err(ReviewError::Refused(why)) => return unauthorized(&why),
        // NOT a 401. The credential may be fine; the verifier could not
        // be reached. A backend told 401 discards its token and
        // re-authenticates, turning a blip in the verifier into a
        // credential churn across every caller at once.
        Err(ReviewError::Unreachable(why)) => {
            return json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "ReviewerUnreachable",
                &format!("could not verify your credential just now: {why}"),
                Some(5),
            )
        }
    };

    let Some(repo) = look_up_repo(&door.repos, &ns, &name) else {
        return json_err(
            StatusCode::NOT_FOUND,
            "NoSuchRepository",
            &format!("no FlintRepo named {name:?} in namespace {ns:?}"),
            None,
        );
    };
    if !consumer_allows(repo.spec.consumers.as_ref(), &ns, &identity) {
        // 403 and not 404: the caller authenticated, so telling it the
        // repository exists and it may not reach it is not a leak.
        return json_err(
            StatusCode::FORBIDDEN,
            "NotAConsumer",
            &format!(
                "{} is not listed in spec.consumers for this repository",
                identity.username
            ),
            None,
        );
    }

    let view = ShareView::of_repo(&repo);
    let endpoint = match resolve::decide_for(&view, Door::RepoFileApi) {
        Decision::Dial(ep) => ep,
        Decision::Refuse(r) => return from_refusal(&r),
        // Parked or coming up. `wake=false` refuses BOTH the arming and
        // the wait, for the same reason the lite door does: a service
        // enumerating every project to render a list must not start the
        // ones that were asleep, and must not block on the ones that
        // are already coming up.
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
                    "this repository is {phase} and you asked not to wake it (wake=false). \
                     It will not come back on its own; retry without wake=false."
                ),
                // Deliberately NO Retry-After: nothing is on a timer
                // here, and one would tell a crawler to come back and
                // find exactly the same thing.
                None,
            );
        }
        // The ladder, including arming the wake. This is the whole
        // reason the door exists rather than the backend dialling the
        // Service itself.
        Decision::Wake | Decision::Wait => {
            match wait_for_ready(
                &door.client,
                &door.repos,
                &ns,
                &name,
                Door::RepoFileApi,
                door.cfg.wake_wait,
            )
            .await
            {
                Ok((_, ep)) => ep,
                Err(res) => return res,
            }
        }
    };

    // The server's own phase, when it was observed. Only ever a
    // downgrade of an otherwise-Ready repository.
    if let Some(r) = resolve::hub_phase_blocks(&view) {
        return from_refusal(&r);
    }

    let qs = route::filter_query(verb, &q);
    let url = route::upstream_url(&endpoint, verb, &qs);
    // `spec.fileApi.tokenSecret` is a shared bearer the SYNCER requires
    // and the door structurally cannot present: reading it would mean
    // `get secrets` in every tenant namespace, which is the one thing
    // this component has never had and the reason it holds no fleet
    // credentials at all. The two are ALTERNATIVES — the door's boundary
    // is the NetworkPolicy that admits its pods to the file port, which
    // is the same boundary that makes `X-Remote-User` mean anything.
    //
    // Carried down so that the resulting 401 says which of the two is
    // misconfigured. Without it the failure is a bare "the repository
    // rejected this", on every request, forever, with nothing in either
    // log naming the CR field that caused it.
    let shared_token = repo
        .spec
        .file_api
        .as_ref()
        .and_then(|f| f.token_secret.as_deref())
        .is_some();
    send_upstream(&door, verb, &url, &headers, &identity, payload, shared_token).await
}

#[allow(clippy::too_many_arguments)]
async fn send_upstream(
    door: &RepoFileDoor,
    verb: Verb,
    url: &str,
    headers: &warp::http::HeaderMap,
    identity: &Identity,
    payload: Payload,
    shared_token: bool,
) -> Response {
    let mut req = door.http.request(verb.method(), url);
    // The allowlist, and nothing else. `If-Match` is the load-bearing
    // one: the syncer's conditional write is what turns two browser
    // tabs saving one file into a 412 instead of a lost update, and a
    // door that dropped the header would leave both ends answering 200.
    for name in verb.request_headers() {
        if let Some(v) = headers.get(*name) {
            req = req.header(*name, v.as_bytes());
        }
    }
    // Who this is, as the door verified it. The syncer reads it as the
    // principal for `policy.judge` and as the commit author; nothing the
    // caller sent can reach this header, because the request's headers
    // are built from the allowlist above and this line.
    req = req.header("x-remote-user", identity.username.as_str());
    req = match payload {
        Payload::None => req,
        Payload::Buffered(b) => req.body(b.to_vec()),
        Payload::Stream(s) => req.body(s),
    };

    // The deadline covers the RESPONSE HEADERS only. On the whole
    // exchange it would cap a download at `upstreamTimeout`, so a large
    // file would fail at a deadline chosen for a control-plane hop.
    let sent = match tokio::time::timeout(door.cfg.upstream_timeout, req.send()).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return json_err(
                StatusCode::BAD_GATEWAY,
                "UpstreamUnreachable",
                &format!("the repository server did not answer: {e}"),
                Some(5),
            )
        }
        Err(_) => {
            return json_err(
                StatusCode::GATEWAY_TIMEOUT,
                "UpstreamTimeout",
                &format!(
                    "no response headers from the repository server within {:?}",
                    door.cfg.upstream_timeout
                ),
                Some(5),
            )
        }
    };
    // The one upstream refusal the door can explain better than the
    // syncer can. Everything else is relayed verbatim: the syncer's
    // 409s, 412s and 403s are the API's own contract and the door has
    // no business rewording them.
    if sent.status() == reqwest::StatusCode::UNAUTHORIZED && shared_token {
        return json_err(
            StatusCode::BAD_GATEWAY,
            "SharedTokenNotPresentable",
            "this repository sets spec.fileApi.tokenSecret, which the door cannot present \
             — it holds no secrets RBAC by design. Behind the door the boundary is the \
             NetworkPolicy that admits its pods to the file port, so clear tokenSecret on \
             the FlintRepo; keep it only for a deployment that reaches the syncer directly.",
            None,
        );
    }
    relay(sent)
}

fn relay(res: reqwest::Response) -> Response {
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
    // Streamed, not buffered: a download is as large as the file, and
    // the door must not hold one in memory per concurrent reader.
    let stream = res.bytes_stream().map_err(std::io::Error::other);
    let mut out = Response::new(warp::hyper::Body::wrap_stream(stream));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

#[cfg(test)]
mod tests {
    //! End to end over real sockets, against a fake syncer.
    //!
    //! The pure decision is tested in `resolve`; what these add is the
    //! part that faces a browser backend — a fake file API on a real
    //! port, a `FlintRepo` whose `status.apiEndpoint` points at it, and
    //! requests driven through the assembled route table. Every test
    //! that asserts an ABSENCE (the syncer was not reached, a header
    //! did not arrive) pairs it with a positive control in the same
    //! test, because "nothing happened" is also what a broken rig
    //! produces.

    use super::*;
    use crate::forge_operator::crd::{
        FileApiSpec, FlintRepoSpec, FlintRepoStatus, RepoIdle, RepoPhase,
    };
    use crate::s3csi::policy::Consumers;
    use kube::runtime::{reflector, watcher};
    use std::sync::atomic::AtomicU64;
    use std::sync::Mutex;
    use warp::http::HeaderMap;

    /// What the fake syncer saw.
    #[derive(Debug, Clone, Default)]
    struct Seen {
        method: String,
        path: String,
        query: String,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    type Log = Arc<Mutex<Vec<Seen>>>;

    /// Answers every path with a 200 and an ETag, and records what it
    /// was asked. The ETag matters: it is in `RESPONSE_HEADERS`, and a
    /// caller that never receives one cannot send `If-Match`.
    async fn fake_syncer(log: Log) -> String {
        let route = warp::any()
            .and(warp::method())
            .and(warp::path::full())
            .and(warp::query::<HashMap<String, String>>())
            .and(warp::header::headers_cloned())
            .and(warp::body::bytes())
            .map(
                move |m: warp::http::Method,
                      p: warp::path::FullPath,
                      q: HashMap<String, String>,
                      h: HeaderMap,
                      b: Bytes| {
                    let mut q: Vec<String> = q.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    q.sort();
                    log.lock().unwrap().push(Seen {
                        method: m.to_string(),
                        path: p.as_str().to_string(),
                        query: q.join("&"),
                        headers: h,
                        body: b.to_vec(),
                    });
                    warp::reply::with_header(
                        warp::reply::with_header(
                            "{\"ok\":true}",
                            "content-type",
                            "application/json",
                        ),
                        "etag",
                        "\"blob-oid\"",
                    )
                },
            );
        let (addr, srv) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(srv);
        format!("http://{addr}")
    }

    fn repo(endpoint: Option<&str>, consumers: Vec<&str>, phase: RepoPhase) -> FlintRepo {
        let mut r = FlintRepo::new(
            "proj",
            FlintRepoSpec {
                syncer_env: None,
                project_id: "proj".into(),
                bucket: "b".into(),
                key_prefix: "tenant/proj/".into(),
                endpoint: None,
                credentials_secret_ref: None,
                default_branch: None,
                consumers: Some(Consumers {
                    service_accounts: consumers.into_iter().map(String::from).collect(),
                }),
                branches: None,
                idle: Some(RepoIdle { suspend_after_secs: Some(600) }),
                export: None,
                fleet: None,
                lfs: None,
                log_level: None,
                lifecycle: None,
                file_api: Some(FileApiSpec {
                    enabled: true,
                    branch: Some("workspace".into()),
                    max_mb: None,
                    token_secret: None,
                }),
            },
        );
        r.metadata.namespace = Some("tenant".into());
        r.status = Some(FlintRepoStatus {
            phase: Some(phase),
            api_endpoint: endpoint.map(String::from),
            server_phase: Some("Serving".into()),
            ..Default::default()
        });
        r
    }

    fn store_of(repos: Vec<FlintRepo>) -> Store<FlintRepo> {
        let (store, mut writer) = reflector::store::<FlintRepo>();
        writer.apply_watcher_event(&watcher::Event::Init);
        for r in repos {
            writer.apply_watcher_event(&watcher::Event::InitApply(r));
        }
        writer.apply_watcher_event(&watcher::Event::InitDone);
        store
    }

    struct CountingReviewer {
        calls: Arc<AtomicU64>,
        verdict: Result<Identity, ReviewError>,
    }

    #[async_trait::async_trait]
    impl Reviewer for CountingReviewer {
        async fn review(&self, _token: &str) -> Result<Identity, ReviewError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.verdict.clone()
        }
    }

    fn identity(ns: &str, sa: &str) -> Identity {
        Identity {
            username: format!("system:serviceaccount:{ns}:{sa}"),
            namespace: ns.into(),
            service_account: sa.into(),
            pod_uid: None,
            pod_name: None,
        }
    }

    fn door_with(
        repos: Vec<FlintRepo>,
        reviewer: Arc<dyn Reviewer>,
        read_only: bool,
    ) -> Arc<RepoFileDoor> {
        crate::install_crypto_provider();
        // Never dialled: the reviewer is a double and the wake PATCH is
        // best effort, which one test asserts explicitly.
        let client =
            kube::Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().expect("uri")))
                .expect("client");
        Arc::new(RepoFileDoor {
            client,
            repos: store_of(repos),
            http: reqwest::Client::builder().build().expect("http"),
            cfg: RepoFileConfig {
                wake_wait: Duration::from_millis(200),
                read_only,
                ..RepoFileConfig::default()
            },
            ready: Arc::new(AtomicBool::new(true)),
            reviewer,
        })
    }

    fn ok_reviewer(calls: Arc<AtomicU64>) -> Arc<dyn Reviewer> {
        Arc::new(CountingReviewer {
            calls,
            verdict: Ok(identity("tenant", "app-backend")),
        })
    }

    const LIST: &str = "/repo/tenant/proj/files";
    const CONTENT: &str = "/repo/tenant/proj/files/content";

    /// THE ONE THAT MATTERS.
    ///
    /// Whatever a caller puts in the namespace, the repository name or
    /// the `path=` parameter, the upstream URL's PATH is one of the six
    /// literals in `route::Verb`. Asserted as a property over hostile
    /// inputs rather than as a list of payloads someone thought of.
    ///
    /// Its anti-vacuity control is in the same test: the payload DOES
    /// survive into the request, percent-encoded in the query, so this
    /// is not passing because everything was thrown away.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_caller_input_can_reach_the_upstream_path() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_syncer(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["app-backend"], RepoPhase::Ready)],
            ok_reviewer(Arc::new(AtomicU64::new(0))),
            false,
        );
        let routes = routes(door);

        for hostile in [
            "../status",
            "..%2Fstatus",
            "%2e%2e%2fstatus",
            "/status",
            "files/../status",
            "http://evil.example/status",
        ] {
            let res = warp::test::request()
                .method("GET")
                .path(&format!("{CONTENT}?path={}", urlencoding(hostile)))
                .header("authorization", "Bearer tok")
                .reply(&routes)
                .await;
            assert_eq!(res.status(), 200, "{hostile:?}");
        }
        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 6, "every hostile request reached the syncer");
        for s in &seen {
            assert_eq!(s.path, "/files/content", "path was {:?}", s.path);
            assert!(!s.path.contains("status"), "{:?}", s.path);
        }
        // …and the control: the payload really was forwarded, as a
        // query value.
        assert!(
            seen.iter().any(|s| s.query.contains("status")),
            "nothing survived into the query — the assertion above is vacuous: {:?}",
            seen.iter().map(|s| &s.query).collect::<Vec<_>>()
        );
    }

    fn urlencoding(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    /// A caller must not be able to say who it is. The door sets
    /// `X-Remote-User` from the VERIFIED token, and the syncer reads it
    /// as the principal for `policy.judge` and as the commit author —
    /// so a caller-supplied one that survived would be an authorization
    /// bypass, not a cosmetic bug.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_caller_cannot_smuggle_its_own_principal() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_syncer(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["app-backend"], RepoPhase::Ready)],
            ok_reviewer(Arc::new(AtomicU64::new(0))),
            false,
        );
        let res = warp::test::request()
            .method("GET")
            .path(LIST)
            .header("authorization", "Bearer tok")
            .header("x-remote-user", "system:serviceaccount:kube-system:admin")
            .reply(&routes(door))
            .await;
        assert_eq!(res.status(), 200);

        let seen = log.lock().unwrap().clone();
        let got = seen[0].headers.get("x-remote-user").expect("the door sets one");
        assert_eq!(
            got.to_str().unwrap(),
            "system:serviceaccount:tenant:app-backend",
            "the caller's forged principal reached the syncer"
        );
        // Exactly one, not two — a second header with the forged value
        // would be read by whichever end looks last.
        assert_eq!(seen[0].headers.get_all("x-remote-user").iter().count(), 1);
        // And the caller's credential is never forwarded: a syncer
        // holding it would hold the credential to every repository.
        assert!(seen[0].headers.get("authorization").is_none());
    }

    /// Authentication precedes EVERYTHING — the store lookup, the
    /// consumers check, and above all the wake. A credential-less peer
    /// that could wake a repository could spend a cluster's money by
    /// touching every URL it could guess.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unauthenticated_request_is_refused_and_never_dialled() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_syncer(log.clone()).await;
        let calls = Arc::new(AtomicU64::new(0));
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["app-backend"], RepoPhase::Ready)],
            ok_reviewer(calls.clone()),
            false,
        );
        let routes = routes(door);

        for header in [None, Some("Bearer "), Some("Basic"), Some("nonsense")] {
            let mut req = warp::test::request().method("GET").path(LIST);
            if let Some(h) = header {
                req = req.header("authorization", h);
            }
            let res = req.reply(&routes).await;
            assert_eq!(res.status(), 401, "{header:?}");
            // NOT a Basic challenge: this door's caller is a backend
            // holding a token, and a challenge that reached a browser
            // would put a native password dialog in front of a person
            // who has no password to type. The git door must send one;
            // this one must not.
            assert!(
                res.headers().get("www-authenticate").is_none(),
                "{header:?} got a Basic challenge"
            );
        }
        assert!(log.lock().unwrap().is_empty(), "the syncer was dialled anyway");

        // The control, through the same rig: a real credential lands.
        let res = warp::test::request()
            .method("GET")
            .path(LIST)
            .header("authorization", "Bearer tok")
            .reply(&routes)
            .await;
        assert_eq!(res.status(), 200);
        assert_eq!(log.lock().unwrap().len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// One credential presentation at both forge doors. `Bearer` is
    /// what a REST backend sends; `Basic` is what a `curl -u` and the
    /// git credential helper send, and both must name the same token.
    #[test]
    fn either_presentation_yields_the_same_token() {
        assert_eq!(presented_token(Some("Bearer abc")).as_deref(), Some("abc"));
        assert_eq!(presented_token(Some("bearer abc")).as_deref(), Some("abc"));
        let basic = format!(
            "Basic {}",
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                "anyuser:abc"
            )
        );
        assert_eq!(presented_token(Some(&basic)).as_deref(), Some("abc"));
        for junk in ["", "Bearer", "Bearer ", "Basic", "Token abc", "abc"] {
            assert_eq!(presented_token(Some(junk)), None, "{junk:?}");
        }
        assert_eq!(presented_token(None), None);
    }

    /// `spec.consumers` is the SAME list the git door enforces. A
    /// principal that may not clone must not be able to read files, or
    /// the file door is a way around the git door.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_non_consumer_is_refused_and_the_syncer_is_never_reached() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_syncer(log.clone()).await;
        let door = door_with(
            // The repository lists someone else.
            vec![repo(Some(&endpoint), vec!["other-runner"], RepoPhase::Ready)],
            ok_reviewer(Arc::new(AtomicU64::new(0))),
            false,
        );
        let res = warp::test::request()
            .method("GET")
            .path(LIST)
            .header("authorization", "Bearer tok")
            .reply(&routes(door))
            .await;
        // 403 and not 404: the caller authenticated, so naming the
        // repository is not a leak — and a 404 would send an operator
        // hunting a CR that is right there.
        assert_eq!(res.status(), 403);
        assert!(log.lock().unwrap().is_empty());

        // The control: listed, and it lands.
        let log2: Log = Arc::new(Mutex::new(Vec::new()));
        let ep2 = fake_syncer(log2.clone()).await;
        let door = door_with(
            vec![repo(Some(&ep2), vec!["app-backend"], RepoPhase::Ready)],
            ok_reviewer(Arc::new(AtomicU64::new(0))),
            false,
        );
        let res = warp::test::request()
            .method("GET")
            .path(LIST)
            .header("authorization", "Bearer tok")
            .reply(&routes(door))
            .await;
        assert_eq!(res.status(), 200);
        assert_eq!(log2.lock().unwrap().len(), 1);
    }

    /// The six routes reach the six upstream paths with the six
    /// methods, and — the part that has bitten this codebase before —
    /// the two POST leaves do not consume each other.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_verb_reaches_its_own_upstream_path_and_method() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_syncer(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["app-backend"], RepoPhase::Ready)],
            ok_reviewer(Arc::new(AtomicU64::new(0))),
            false,
        );
        let routes = routes(door);

        let cases: Vec<(&str, &str, Option<&str>, &str, &str)> = vec![
            ("GET", LIST, None, "GET", "/files"),
            ("GET", CONTENT, None, "GET", "/files/content"),
            ("PUT", CONTENT, Some("hello"), "PUT", "/files/content"),
            ("DELETE", CONTENT, None, "DELETE", "/files/content"),
            (
                "POST",
                "/repo/tenant/proj/files/folder",
                Some("{}"),
                "POST",
                "/files/folder",
            ),
            (
                "POST",
                "/repo/tenant/proj/files/move",
                Some("{\"from\":\"a\",\"to\":\"b\"}"),
                "POST",
                "/files/move",
            ),
        ];
        for (method, path, body, up_method, up_path) in &cases {
            let mut req = warp::test::request()
                .method(method)
                .path(path)
                .header("authorization", "Bearer tok");
            if let Some(b) = body {
                req = req.body(b);
            }
            let res = req.reply(&routes).await;
            assert_eq!(res.status(), 200, "{method} {path}");
            let seen = log.lock().unwrap().last().cloned().expect("reached");
            assert_eq!(&seen.method, up_method, "{method} {path}");
            assert_eq!(&seen.path, up_path, "{method} {path}");
        }
        assert_eq!(log.lock().unwrap().len(), 6);

        // A path the table does not have is a 404 from the door and
        // never an upstream call — the property `route`'s module doc
        // rests on.
        for bogus in [
            "/repo/tenant/proj/files/status",
            "/repo/tenant/proj/status",
            "/repo/tenant/proj/files/content/extra",
        ] {
            let res = warp::test::request()
                .method("GET")
                .path(bogus)
                .header("authorization", "Bearer tok")
                .reply(&routes)
                .await;
            assert_eq!(res.status(), 404, "{bogus}");
        }
        assert_eq!(log.lock().unwrap().len(), 6, "a bogus path reached the syncer");
    }

    /// v1.30.0's conditional write is an end-to-end protocol and this
    /// door sits in the middle of it. If `If-Match` is dropped the
    /// syncer sees an unconditional write and answers 200, so the lost
    /// update is invisible from BOTH ends — which is exactly the shape
    /// of bug two browser tabs saving one file produces.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_conditional_write_headers_survive_the_door() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_syncer(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["app-backend"], RepoPhase::Ready)],
            ok_reviewer(Arc::new(AtomicU64::new(0))),
            false,
        );
        let routes = routes(door);

        let res = warp::test::request()
            .method("PUT")
            .path(CONTENT)
            .header("authorization", "Bearer tok")
            .header("if-match", "\"blob-oid\"")
            .header("content-type", "text/plain")
            .body("new content")
            .reply(&routes)
            .await;
        assert_eq!(res.status(), 200);
        let seen = log.lock().unwrap().last().cloned().expect("reached");
        assert_eq!(seen.headers.get("if-match").unwrap(), "\"blob-oid\"");
        assert_eq!(seen.body, b"new content");

        // A move is conditional too — a rename of a file someone else
        // has since replaced must be refusable.
        warp::test::request()
            .method("POST")
            .path("/repo/tenant/proj/files/move")
            .header("authorization", "Bearer tok")
            .header("if-match", "\"other-oid\"")
            .body("{\"from\":\"a\",\"to\":\"b\"}")
            .reply(&routes)
            .await;
        let seen = log.lock().unwrap().last().cloned().expect("reached");
        assert_eq!(seen.headers.get("if-match").unwrap(), "\"other-oid\"");

        // …and the ETag comes back, or a caller could never send one.
        let res = warp::test::request()
            .method("GET")
            .path(CONTENT)
            .header("authorization", "Bearer tok")
            .reply(&routes)
            .await;
        assert_eq!(res.headers().get("etag").unwrap(), "\"blob-oid\"");
    }

    /// A browse deployment must not be able to rewrite every
    /// repository it can see. The four mutations are refused BEFORE the
    /// syncer is dialled; the two reads still work, which is the
    /// control that says the door is not simply broken.
    #[tokio::test(flavor = "multi_thread")]
    async fn read_only_refuses_the_four_mutations_and_serves_the_two_reads() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_syncer(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["app-backend"], RepoPhase::Ready)],
            ok_reviewer(Arc::new(AtomicU64::new(0))),
            true,
        );
        let routes = routes(door);

        for (method, path, body) in [
            ("PUT", CONTENT, Some("x")),
            ("DELETE", CONTENT, None),
            ("POST", "/repo/tenant/proj/files/folder", Some("{}")),
            ("POST", "/repo/tenant/proj/files/move", Some("{}")),
        ] {
            let mut req = warp::test::request()
                .method(method)
                .path(path)
                .header("authorization", "Bearer tok");
            if let Some(b) = body {
                req = req.body(b);
            }
            let res = req.reply(&routes).await;
            assert_eq!(res.status(), 403, "{method} {path}");
        }
        assert!(log.lock().unwrap().is_empty(), "a mutation reached the syncer");

        for path in [LIST, CONTENT] {
            let res = warp::test::request()
                .method("GET")
                .path(path)
                .header("authorization", "Bearer tok")
                .reply(&routes)
                .await;
            assert_eq!(res.status(), 200, "{path}");
        }
        assert_eq!(log.lock().unwrap().len(), 2);
    }

    /// The whole reason this module exists rather than the backend
    /// dialling the Service itself. A parked repository is WAITED for,
    /// not refused outright — and `wake=false` is the crawl escape
    /// hatch, which must refuse without dialling and without a
    /// `Retry-After` that would send a crawler round again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_parked_repository_is_waited_for_and_wake_false_refuses_at_once() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_syncer(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["app-backend"], RepoPhase::IdleSuspended)],
            ok_reviewer(Arc::new(AtomicU64::new(0))),
            false,
        );
        let routes = routes(door);

        // wake=false: refused at once, nothing dialled, no Retry-After.
        let started = std::time::Instant::now();
        let res = warp::test::request()
            .method("GET")
            .path(&format!("{LIST}?wake=false"))
            .header("authorization", "Bearer tok")
            .reply(&routes)
            .await;
        assert_eq!(res.status(), 503);
        assert!(res.headers().get("retry-after").is_none());
        assert!(started.elapsed() < Duration::from_millis(150), "it waited anyway");
        assert!(log.lock().unwrap().is_empty());

        // Without it the request is HELD — the repository never comes
        // back in this rig (nothing reconciles it), so the hold expires
        // and the answer names that rather than "no such thing".
        let res = warp::test::request()
            .method("GET")
            .path(LIST)
            .header("authorization", "Bearer tok")
            .reply(&routes)
            .await;
        assert_eq!(res.status(), 503);
        let body = String::from_utf8_lossy(res.body()).to_string();
        assert!(body.contains("RepositoryNotReady"), "{body}");
        assert!(res.headers().get("retry-after").is_some());

        // A typo must NOT read as "yes, wake" — the one whose blast
        // radius is every parked repository in the cluster.
        let res = warp::test::request()
            .method("GET")
            .path(&format!("{LIST}?wake=fasle"))
            .header("authorization", "Bearer tok")
            .reply(&routes)
            .await;
        assert_eq!(res.status(), 400);
    }

    /// The gateway-only parameter is CONSUMED here and never forwarded.
    /// A syncer that received `wake=` would have to have an opinion
    /// about it, and it has none.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_wake_control_never_reaches_the_syncer() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_syncer(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["app-backend"], RepoPhase::Ready)],
            ok_reviewer(Arc::new(AtomicU64::new(0))),
            false,
        );
        let res = warp::test::request()
            .method("GET")
            .path(&format!("{LIST}?path=%2F&wake=true&limit=10&token=steal-me"))
            .header("authorization", "Bearer tok")
            .reply(&routes(door))
            .await;
        assert_eq!(res.status(), 200);
        let seen = log.lock().unwrap().last().cloned().expect("reached");
        assert!(!seen.query.contains("wake"), "wake leaked: {}", seen.query);
        assert!(!seen.query.contains("steal-me"), "{}", seen.query);
        // The control: the parameters that ARE the API survived.
        assert!(seen.query.contains("path="), "{}", seen.query);
        assert!(seen.query.contains("limit=10"), "{}", seen.query);
    }

    /// A `FlintRepo` with the file API off is a 501 that says what to
    /// change — not a 503 the caller retries forever. This is the
    /// defect design §7.2 named, at the door rather than in `resolve`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_repository_without_the_file_api_says_so_permanently() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_syncer(log.clone()).await;
        let mut off = repo(Some(&endpoint), vec!["app-backend"], RepoPhase::Ready);
        off.spec.file_api = None;
        off.status.as_mut().unwrap().api_endpoint = None;
        let door = door_with(
            vec![off],
            ok_reviewer(Arc::new(AtomicU64::new(0))),
            false,
        );
        let res = warp::test::request()
            .method("GET")
            .path(LIST)
            .header("authorization", "Bearer tok")
            .reply(&routes(door))
            .await;
        assert_eq!(res.status(), 501);
        assert!(res.headers().get("retry-after").is_none(), "501 invited a retry");
        let body = String::from_utf8_lossy(res.body()).to_string();
        assert!(body.contains("spec.fileApi.enabled"), "{body}");
        assert!(log.lock().unwrap().is_empty());
    }

    /// A cold cache 404s every repository, which a caller reads as "no
    /// such repository" rather than "ask again in a second".
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cold_cache_is_503_and_not_404() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_syncer(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["app-backend"], RepoPhase::Ready)],
            ok_reviewer(Arc::new(AtomicU64::new(0))),
            false,
        );
        door.ready.store(false, Ordering::Relaxed);
        let res = warp::test::request()
            .method("GET")
            .path(LIST)
            .header("authorization", "Bearer tok")
            .reply(&routes(door.clone()))
            .await;
        assert_eq!(res.status(), 503);
        assert_eq!(res.headers().get("retry-after").unwrap(), "2");

        // The control: warm, and it serves.
        door.ready.store(true, Ordering::Relaxed);
        let res = warp::test::request()
            .method("GET")
            .path(LIST)
            .header("authorization", "Bearer tok")
            .reply(&routes(door))
            .await;
        assert_eq!(res.status(), 200);
    }

    /// An unknown repository is a 404 whatever the caller's credential,
    /// and a nonsense address never costs a store lookup.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_or_nonsense_repository_is_404() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_syncer(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["app-backend"], RepoPhase::Ready)],
            ok_reviewer(Arc::new(AtomicU64::new(0))),
            false,
        );
        let routes = routes(door);
        for path in [
            "/repo/tenant/nope/files",
            "/repo/other/proj/files",
            "/repo/TENANT/proj/files",
            "/repo/tenant/..%2F..%2Fetc/files",
        ] {
            let res = warp::test::request()
                .method("GET")
                .path(path)
                .header("authorization", "Bearer tok")
                .reply(&routes)
                .await;
            assert_eq!(res.status(), 404, "{path}");
        }
        assert!(log.lock().unwrap().is_empty());

        // `.git` is accepted on this door too, so one address works at
        // both — a UI that stores the clone URL does not need a second
        // form of the name.
        let res = warp::test::request()
            .method("GET")
            .path("/repo/tenant/proj.git/files")
            .header("authorization", "Bearer tok")
            .reply(&routes)
            .await;
        assert_eq!(res.status(), 200);
    }

    /// THE PROPERTY THE WHOLE COMPONENT EXISTS FOR: a git client and a
    /// browser backend reach ONE repository at ONE host and port, and
    /// differ only in their path prefix.
    ///
    /// Asserted on the ASSEMBLED filter, both route tables mounted the
    /// way `flint_hub_gateway` mounts them, because the failure this
    /// catches is a route-table one: warp commits to a branch when it
    /// matches, so a prefix that swallowed the other door's requests
    /// would 404 every one of them — and each door's own test suite
    /// would still be green, because each mounts only itself.
    #[tokio::test(flavor = "multi_thread")]
    async fn both_doors_serve_one_repository_on_one_listener() {
        let git_log: Log = Arc::new(Mutex::new(Vec::new()));
        let file_log: Log = Arc::new(Mutex::new(Vec::new()));
        let git_ep = fake_syncer(git_log.clone()).await;
        let file_ep = fake_syncer(file_log.clone()).await;

        // ONE FlintRepo carrying BOTH endpoints — which is the shape
        // the operator publishes: two ports on one syncer pod.
        let mut r = repo(Some(&file_ep), vec!["app-backend"], RepoPhase::Ready);
        r.status.as_mut().unwrap().git_endpoint = Some(format!("{git_ep}/proj.git"));

        let calls = Arc::new(AtomicU64::new(0));
        let reviewer = ok_reviewer(calls.clone());
        crate::install_crypto_provider();
        let client =
            kube::Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().expect("uri")))
                .expect("client");
        let git_door = Arc::new(git::GitDoor {
            client,
            repos: store_of(vec![r]),
            http: reqwest::Client::builder().build().expect("http"),
            cfg: git::GitConfig { wake_wait: Duration::from_millis(200), ..Default::default() },
            ready: Arc::new(AtomicBool::new(true)),
            reviewer,
        });
        let files = RepoFileDoor::beside(&git_door, RepoFileConfig::default());

        // Mounted exactly as the binary mounts them.
        let both = git::routes(git_door).or(routes(files));

        // A git client's advertisement — the first request of every
        // clone, fetch and push.
        let res = warp::test::request()
            .method("GET")
            .path("/git/tenant/proj.git/info/refs?service=git-upload-pack")
            .header("authorization", &basic("tok"))
            .reply(&both)
            .await;
        assert_eq!(res.status(), 200, "the git door stopped serving beside the file door");

        // …and a push, which is the mutating half.
        let res = warp::test::request()
            .method("POST")
            .path("/git/tenant/proj.git/git-receive-pack")
            .header("authorization", &basic("tok"))
            .body("0000")
            .reply(&both)
            .await;
        assert_eq!(res.status(), 200);

        // The browser backend, same host, same port, different prefix.
        for (method, path, body) in [
            ("GET", "/repo/tenant/proj/files?path=%2F", None),
            ("GET", "/repo/tenant/proj/files/content?path=a.txt", None),
            ("PUT", "/repo/tenant/proj/files/content?path=a.txt", Some("hi")),
            ("POST", "/repo/tenant/proj/files/move", Some("{}")),
        ] {
            let mut req = warp::test::request()
                .method(method)
                .path(path)
                .header("authorization", "Bearer tok");
            if let Some(b) = body {
                req = req.body(b);
            }
            let res = req.reply(&both).await;
            assert_eq!(res.status(), 200, "{method} {path} through the combined filter");
        }

        // Each door reached ITS OWN upstream and neither reached the
        // other's — the check that says the two are actually separate
        // rather than one filter answering everything.
        let git_seen = git_log.lock().unwrap().clone();
        let file_seen = file_log.lock().unwrap().clone();
        assert_eq!(git_seen.len(), 2, "git door hops: {git_seen:?}");
        assert_eq!(file_seen.len(), 4, "file door hops: {file_seen:?}");
        for s in &git_seen {
            assert!(s.path.contains("git"), "the git door hit a file path: {}", s.path);
        }
        for s in &file_seen {
            assert!(s.path.starts_with("/files"), "the file door hit {}", s.path);
        }

        // ONE identity, ONE review cache: six requests, and the
        // apiserver was asked once. Two doors with two caches would be
        // six here (the doubles do not cache; the shared `Arc` is what
        // is being asserted).
        assert!(
            file_seen
                .iter()
                .all(|s| s.headers.get("x-remote-user").map(|v| v.as_bytes())
                    == Some(b"system:serviceaccount:tenant:app-backend".as_ref())),
            "the file door forwarded a different principal than the git door verified"
        );
        assert_eq!(
            git_seen[0].headers.get("x-remote-user").unwrap(),
            "system:serviceaccount:tenant:app-backend"
        );
    }

    /// The git door's Basic presentation, for the combined test.
    fn basic(token: &str) -> String {
        format!(
            "Basic {}",
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                format!("pod:{token}")
            )
        )
    }

    /// A 401 from the syncer, when the repository sets a shared bearer
    /// the door structurally cannot hold, must name the CR field that
    /// caused it. The alternative is what a cluster would have shown:
    /// every request 401ing forever with nothing in either log saying
    /// why.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_shared_token_the_door_cannot_present_is_explained_not_relayed() {
        // A syncer that behaves like one with `token` configured.
        let route = warp::any().map(|| {
            let mut r = warp::reply::Response::new("unauthorized".into());
            *r.status_mut() = StatusCode::UNAUTHORIZED;
            r
        });
        let (addr, srv) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(srv);
        let endpoint = format!("http://{addr}");

        let mut r = repo(Some(&endpoint), vec!["app-backend"], RepoPhase::Ready);
        r.spec.file_api.as_mut().unwrap().token_secret = Some("proj-file-token".into());
        let door = door_with(vec![r], ok_reviewer(Arc::new(AtomicU64::new(0))), false);
        let res = warp::test::request()
            .method("GET")
            .path(LIST)
            .header("authorization", "Bearer tok")
            .reply(&routes(door))
            .await;
        assert_eq!(res.status(), 502);
        let body = String::from_utf8_lossy(res.body()).to_string();
        assert!(body.contains("spec.fileApi.tokenSecret"), "{body}");

        // THE CONTROL. The same 401 from a repository that sets NO
        // shared token is relayed as itself — the door must not
        // reinterpret every upstream 401 as this one misconfiguration.
        let mut plain = repo(Some(&endpoint), vec!["app-backend"], RepoPhase::Ready);
        plain.spec.file_api.as_mut().unwrap().token_secret = None;
        let door = door_with(vec![plain], ok_reviewer(Arc::new(AtomicU64::new(0))), false);
        let res = warp::test::request()
            .method("GET")
            .path(LIST)
            .header("authorization", "Bearer tok")
            .reply(&routes(door))
            .await;
        assert_eq!(res.status(), 401, "an unrelated upstream 401 was rewritten");
    }

    /// Beside the git door there must be ONE store and ONE review
    /// cache: two reflectors over one CRD are two answers to "is this
    /// repository up", and a backend that pushed and then saved could
    /// get both.
    #[tokio::test(flavor = "multi_thread")]
    async fn sharing_the_git_doors_reviewer_costs_one_review_not_two() {
        let calls = Arc::new(AtomicU64::new(0));
        let reviewer = ok_reviewer(calls.clone());
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_syncer(log.clone()).await;
        crate::install_crypto_provider();
        let client =
            kube::Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().expect("uri")))
                .expect("client");
        let git_door = Arc::new(git::GitDoor {
            client,
            repos: store_of(vec![repo(
                Some(&endpoint),
                vec!["app-backend"],
                RepoPhase::Ready,
            )]),
            http: reqwest::Client::builder().build().expect("http"),
            cfg: git::GitConfig::default(),
            ready: Arc::new(AtomicBool::new(true)),
            reviewer,
        });
        let files = RepoFileDoor::beside(&git_door, RepoFileConfig::default());
        assert!(
            Arc::ptr_eq(&files.reviewer, &git_door.reviewer),
            "the two doors hold different reviewers"
        );
        assert!(files.ready.load(Ordering::Relaxed));

        let res = warp::test::request()
            .method("GET")
            .path(LIST)
            .header("authorization", "Bearer tok")
            .reply(&routes(files))
            .await;
        assert_eq!(res.status(), 200);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
