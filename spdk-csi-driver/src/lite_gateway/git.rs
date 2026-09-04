//! `Door::Git` — the front door for flint forge (design
//! `docs/plans/flint-forge-design.md` §6).
//!
//! Three routes, one credential rule, and the same resolve-wake-dial
//! decision the file API already uses. What it adds over that door is
//! everything git needs and the file API does not:
//!
//! - **Basic auth whose password is the pod's own ServiceAccount
//!   token.** An agent holds no key. `TokenReview` turns the token into
//!   a principal, `spec.consumers` says whether that principal may
//!   reach this repository at all, and `X-Remote-User` carries it to
//!   the hooks, which is where the branch policy is applied.
//! - **A cache in front of `TokenReview`.** A clone is two to four
//!   requests and every one of them carries the token, so a thousand
//!   agents cloning at once would be three to four thousand
//!   TokenReviews at the apiserver. The verdict is cached by token
//!   hash for a short TTL — short because it is also how quickly a
//!   deleted pod's credential stops working.
//! - **Chunked bodies, both ways, with no length limit.** The file
//!   API's upload route guards itself with `content_length_limit`,
//!   which would answer 411 to every push over its bound; a git push
//!   is a streamed pack of unknown length by construction.
//! - **`Git-Protocol` forwarded.** `http-backend` sees protocol v2
//!   only if that header reaches it as `GIT_PROTOCOL`. A door that
//!   drops it silently degrades every clone to v0 — no `ls-refs`
//!   prefix filtering, and no bundle URIs at all, which is the storm
//!   lever of §8.
//! - **A longer hold.** git clients do not retry a 503, and a wake
//!   after an unclean death is a lease wait plus a restore, so the
//!   request is held rather than refused.
//!
//! ## The path invariant, kept
//!
//! `route`'s rule is that no byte of the upstream path comes from the
//! caller. It holds here too, and it is worth spelling out because git
//! URLs contain a repository name. The caller's `<namespace>/<repo>`
//! is a LOOKUP KEY: it selects a `FlintRepo` or it 404s. The upstream
//! URL is then `status.gitEndpoint` — which the operator wrote, and
//! which already contains whatever path the server expects — plus a
//! `&'static str` suffix from [`GitVerb::suffix`]. A traversal in the
//! caller's segments cannot reach the upstream URL because none of
//! those bytes are ever concatenated into it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use bytes::Buf;
use futures::TryStreamExt;
use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::runtime::reflector::{ObjectRef, Store};
use kube::Client;
use serde::Serialize;
use warp::http::StatusCode;
use warp::{Filter, Rejection, Reply};

use crate::forge_operator::crd::FlintRepo;
use crate::lite_operator::idle::ANN_REQUESTED_AT;
use crate::s3csi::broker::{identity_from_review, Identity};
use crate::s3csi::policy::Consumers;

use super::resolve::{self, Decision, Door, Refusal, ShareView};

/// The audience an agent's projected token must carry. A token minted
/// for the apiserver's own audience is refused — that separation is
/// exactly what audiences are for, and accepting one here would make
/// any pod token in the cluster a forge credential.
pub const AUDIENCE: &str = "forge.chert.us";

/// The two services `git http-backend` implements. The query parameter
/// is filtered by VALUE and not merely by name: it names a program the
/// server will run, and an allowlist of two is cheaper than reasoning
/// about what a future backend might accept.
pub const SERVICES: &[&str] = &["git-upload-pack", "git-receive-pack"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitVerb {
    /// `GET .../info/refs?service=…` — the advertisement.
    InfoRefs,
    /// `POST .../git-upload-pack` — a clone or fetch.
    UploadPack,
    /// `POST .../git-receive-pack` — a push.
    ReceivePack,
}

impl GitVerb {
    /// Appended to the CR's endpoint. `&'static str` by construction —
    /// the module doc's invariant, in the type.
    pub fn suffix(self) -> &'static str {
        match self {
            GitVerb::InfoRefs => "/info/refs",
            GitVerb::UploadPack => "/git-upload-pack",
            GitVerb::ReceivePack => "/git-receive-pack",
        }
    }

    pub fn method(self) -> reqwest::Method {
        match self {
            GitVerb::InfoRefs => reqwest::Method::GET,
            GitVerb::UploadPack | GitVerb::ReceivePack => reqwest::Method::POST,
        }
    }

    /// Does this change the repository? Only a push does — a fetch is
    /// a read however large it is, which is what makes `--read-only`
    /// meaningful for a mirror or a browse deployment.
    pub fn is_mutation(self) -> bool {
        matches!(self, GitVerb::ReceivePack)
    }
}

/// Request headers forwarded upstream, by name.
///
/// `git-protocol` is the load-bearing one (see the module doc).
/// `content-encoding` matters because git gzips a large `upload-pack`
/// request body and the server must be told; `accept-encoding` because
/// the reply may be compressed. `authorization` is deliberately absent:
/// the caller's credential authenticates it to the DOOR and is never
/// forwarded — the server learns who this is from `X-Remote-User`,
/// which the door sets and which no caller can smuggle in, because
/// this list is an allowlist and the header map is built from nothing
/// else.
pub const GIT_REQUEST_HEADERS: &[&str] =
    &["content-type", "content-encoding", "accept", "accept-encoding", "git-protocol"];

/// Response headers relayed back.
///
/// The cache trio is not decoration: `git http-backend` marks the
/// advertisement uncacheable, and a proxy that drops those headers lets
/// an intermediary serve a stale ref advertisement — which a client
/// then pushes against and is told is stale.
pub const GIT_RESPONSE_HEADERS: &[&str] =
    &["content-type", "content-encoding", "cache-control", "expires", "pragma"];

#[derive(Debug, Clone)]
pub struct GitConfig {
    /// The audience the projected token must carry.
    pub audience: String,
    /// How long a request waits for a parked repository to come back.
    /// git clients do not retry a 503, so this is minutes rather than
    /// the file API's seconds.
    pub wake_wait: Duration,
    /// How long a `TokenReview` verdict is reused. Also the window in
    /// which a deleted pod's token still works, which is why it is
    /// short and why it is configuration.
    pub review_ttl: Duration,
    /// How long to wait for the server's RESPONSE HEADERS. The body
    /// streams untimed after that — a clone of a large repository is a
    /// legitimate request that must not meet a request-scoped
    /// deadline.
    pub upstream_timeout: Duration,
    /// Refuse `git-receive-pack`. A mirror or a read-only deployment
    /// needs no push, and this is the difference between a compromised
    /// door that reads every repository and one that rewrites them.
    pub read_only: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        GitConfig {
            audience: AUDIENCE.to_string(),
            wake_wait: Duration::from_secs(180),
            review_ttl: Duration::from_secs(60),
            upstream_timeout: Duration::from_secs(30),
            read_only: false,
        }
    }
}

/// Turning a token into a principal.
///
/// A trait, so the cache in front of it is a wrapper rather than a
/// branch, and so a test can count the calls that reach the apiserver
/// — which is the only way to check that the cache does the thing it
/// exists for.
#[async_trait::async_trait]
pub trait Reviewer: Send + Sync {
    async fn review(&self, token: &str) -> Result<Identity, String>;
}

/// The real one: `TokenReview` against the apiserver.
pub struct KubeReviewer {
    pub client: Client,
    pub audience: String,
}

#[async_trait::async_trait]
impl Reviewer for KubeReviewer {
    async fn review(&self, token: &str) -> Result<Identity, String> {
        let api: Api<TokenReview> = Api::all(self.client.clone());
        let tr = TokenReview {
            spec: TokenReviewSpec {
                token: Some(token.to_string()),
                audiences: Some(vec![self.audience.clone()]),
            },
            ..Default::default()
        };
        match api.create(&PostParams::default(), &tr).await {
            Ok(out) => identity_from_review(&out, &self.audience),
            Err(e) => Err(format!("TokenReview: {e}")),
        }
    }
}

struct CachedReview {
    at: Instant,
    verdict: Result<Identity, String>,
}

/// A TTL cache in front of any reviewer.
///
/// A clone is two to four requests and each one carries the token, so
/// a thousand agents cloning at once is three to four thousand
/// TokenReviews at the apiserver without this. The TTL is short
/// because it is also the window in which a deleted pod's credential
/// still works.
pub struct CachingReviewer {
    inner: Arc<dyn Reviewer>,
    ttl: Duration,
    cache: Mutex<HashMap<String, CachedReview>>,
}

impl CachingReviewer {
    pub fn new(inner: Arc<dyn Reviewer>, ttl: Duration) -> Arc<Self> {
        Arc::new(CachingReviewer { inner, ttl, cache: Mutex::new(HashMap::new()) })
    }

    /// The cache key.
    ///
    /// A hash, never the token: this map is in memory, is read on every
    /// request, and turns up in any dump of the process. Hashing costs
    /// nothing next to the round trip it saves, and it means the cache
    /// cannot become a credential store.
    fn key_of(token: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(token.as_bytes());
        format!("{:x}", h.finalize())
    }
}

#[async_trait::async_trait]
impl Reviewer for CachingReviewer {
    async fn review(&self, token: &str) -> Result<Identity, String> {
        let key = Self::key_of(token);
        if let Ok(cache) = self.cache.lock() {
            if let Some(hit) = cache.get(&key) {
                if hit.at.elapsed() < self.ttl {
                    return hit.verdict.clone();
                }
            }
        }
        let verdict = self.inner.review(token).await;
        // A REFUSAL is cached; a transport failure is not. An agent
        // with an expired token retries in a loop, and an uncached
        // refusal turns that loop into apiserver load. An apiserver
        // that could not be reached is a different thing entirely:
        // caching it would keep the door shut for the whole TTL after
        // the apiserver came back.
        let cacheable = match &verdict {
            Ok(_) => true,
            Err(e) => !e.starts_with("TokenReview:"),
        };
        if cacheable {
            if let Ok(mut cache) = self.cache.lock() {
                // A crude bound rather than an LRU: the key space is one
                // entry per live pod token, the TTL is a minute, and a
                // map that grows without limit in a door is a slow leak
                // nobody notices until the fleet is large.
                if cache.len() > 8192 {
                    cache.retain(|_, v| v.at.elapsed() < self.ttl);
                }
                cache.insert(key, CachedReview { at: Instant::now(), verdict: verdict.clone() });
            }
        }
        verdict
    }
}

pub struct GitDoor {
    pub client: Client,
    pub repos: Store<FlintRepo>,
    pub http: reqwest::Client,
    pub cfg: GitConfig,
    /// Set once the repo reflector has completed its initial list.
    /// Before that every repository would 404 — read by a client as
    /// "no such repository" rather than "ask again in a second".
    pub ready: Arc<AtomicBool>,
    pub reviewer: Arc<dyn Reviewer>,
}

impl GitDoor {
    pub fn new(
        client: Client,
        repos: Store<FlintRepo>,
        http: reqwest::Client,
        cfg: GitConfig,
        ready: Arc<AtomicBool>,
    ) -> Arc<Self> {
        let reviewer = CachingReviewer::new(
            Arc::new(KubeReviewer { client: client.clone(), audience: cfg.audience.clone() }),
            cfg.review_ttl,
        );
        Arc::new(GitDoor { client, repos, http, cfg, ready, reviewer })
    }

    fn look_up(&self, ns: &str, name: &str) -> Option<Arc<FlintRepo>> {
        self.repos.get(&ObjectRef::<FlintRepo>::new(name).within(ns))
    }

    /// Arm `chert.us/requested-at` on the CR. The operator's ladder is
    /// level-triggered on it; the door never scales anything itself.
    async fn arm_wake(&self, view: &ShareView) {
        let api: Api<FlintRepo> = Api::namespaced(self.client.clone(), &view.namespace);
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let patch = serde_json::json!({ "metadata": { "annotations": { ANN_REQUESTED_AT: now } } });
        if let Err(e) = api.patch(&view.name, &PatchParams::default(), &Patch::Merge(&patch)).await {
            tracing::warn!(
                repo = %format!("{}/{}", view.namespace, view.name),
                error = %e,
                "could not arm the wake annotation; the request will wait and may time out"
            );
        }
    }

    /// Resolve, waking if needed, until the repository is dialable or
    /// the hold expires. Waits on the CR, never on the pod: polling the
    /// pod would count as activity and pin every repository it touched
    /// awake.
    async fn wait_for_ready(&self, ns: &str, name: &str) -> Result<(ShareView, String), Response> {
        let deadline = Instant::now() + self.cfg.wake_wait;
        let mut armed = false;
        loop {
            let Some(repo) = self.look_up(ns, name) else {
                return Err(json_err(
                    StatusCode::NOT_FOUND,
                    "NoSuchRepository",
                    &format!("no FlintRepo named {name:?} in namespace {ns:?}"),
                    None,
                ));
            };
            let view = ShareView::of_repo(&repo);
            match resolve::decide_for(&view, Door::Git) {
                Decision::Dial(endpoint) => {
                    // The server's own phase can still say "not yet",
                    // and dialling it would burn a round trip to learn
                    // what the CR already recorded.
                    if let Some(r) = resolve::hub_phase_blocks(&view) {
                        if Instant::now() >= deadline {
                            return Err(from_refusal(&r));
                        }
                    } else {
                        return Ok((view, endpoint));
                    }
                }
                Decision::Refuse(r) => return Err(from_refusal(&r)),
                Decision::Wake => {
                    if !armed && !view.wake_requested {
                        self.arm_wake(&view).await;
                        armed = true;
                    }
                }
                Decision::Wait => {}
            }
            if Instant::now() >= deadline {
                return Err(json_err(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "RepositoryNotReady",
                    "the repository is starting; it was not serving within the door's hold",
                    Some(10),
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

type Response = warp::reply::Response;

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    reason: String,
}

fn json_err(status: StatusCode, reason: &str, msg: &str, retry_after: Option<u64>) -> Response {
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

fn from_refusal(r: &Refusal) -> Response {
    json_err(
        StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        r.reason,
        &r.message,
        r.retry_after,
    )
}

/// 401 WITH `WWW-Authenticate`. git makes its first request
/// unauthenticated and only sends a credential after a challenge, so a
/// door that answered a bare 401 would make every clone fail rather
/// than prompt the helper.
fn challenge(detail: &str) -> Response {
    let mut res = json_err(StatusCode::UNAUTHORIZED, "Unauthenticated", detail, None);
    res.headers_mut().insert(
        "www-authenticate",
        warp::http::HeaderValue::from_static("Basic realm=\"flint forge\", charset=\"UTF-8\""),
    );
    res
}

/// The password half of HTTP basic. The username is ignored: the
/// credential is the pod's token and the token names the principal, so
/// a username field would be a second, unverified opinion about who
/// this is.
fn basic_password(header: Option<&str>) -> Option<String> {
    let raw = header?.strip_prefix("Basic ").or_else(|| header?.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(raw.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (_user, pass) = text.split_once(':')?;
    Some(pass.to_string())
}

/// Is this principal allowed to reach this repository at all?
///
/// Two spellings are accepted, and the difference matters. A bare
/// `agent-runner` means "that ServiceAccount IN THIS REPOSITORY'S
/// NAMESPACE" — the common case, and the one where a bare name is
/// unambiguous. A fully qualified `system:serviceaccount:<ns>:<sa>`
/// names a principal in any namespace, which is what a repository
/// shared across tenant namespaces needs. Matching a bare name against
/// any namespace would have made `agent-runner` in a namespace the
/// repository's owner has never heard of into a consumer of it.
pub fn consumer_allows(consumers: Option<&Consumers>, repo_ns: &str, id: &Identity) -> bool {
    let Some(c) = consumers else { return false };
    c.service_accounts.iter().any(|entry| {
        entry == "*"
            || entry == &id.username
            || (entry == &id.service_account && id.namespace == repo_ns)
    })
}

/// A namespace or object name from the request path. Not a security
/// boundary — the upstream path never contains these bytes — but a
/// cheap way to answer a nonsense request without a store lookup and
/// without putting arbitrary text in a log line.
fn plausible_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        && !s.starts_with('.')
        && !s.contains("..")
}

/// git addresses a repository as `<name>.git` by convention and as
/// `<name>` when someone types it by hand. Both resolve to the same CR.
fn repo_name(segment: &str) -> String {
    segment.strip_suffix(".git").unwrap_or(segment).to_string()
}

/// The three routes.
///
/// Mounted under `/git/` so that a repository named `v1` cannot
/// collide with the file API's own prefix, and so the whole git surface
/// can be disabled by not mounting this filter at all.
pub fn routes(
    door: Arc<GitDoor>,
) -> impl Filter<Extract = (impl Reply,), Error = Rejection> + Clone {
    let info_refs = {
        let door = door.clone();
        warp::path!("git" / String / String / "info" / "refs")
            .and(warp::get())
            .and(warp::query::<HashMap<String, String>>())
            .and(warp::header::optional::<String>("authorization"))
            .and(warp::header::headers_cloned())
            .and_then(move |ns: String, repo: String, q: HashMap<String, String>, auth, hdrs| {
                let door = door.clone();
                async move {
                    Ok::<_, Rejection>(
                        proxy(
                            door,
                            ns,
                            repo,
                            GitVerb::InfoRefs,
                            q.get("service").cloned(),
                            auth,
                            hdrs,
                            None,
                        )
                        .await,
                    )
                }
            })
    };

    let upload = {
        let door = door.clone();
        // The literal is IN the path filter, not checked inside the
        // handler. A handler that answered 404 for the other verb would
        // CONSUME the request, and warp's `or` would never reach the
        // second route — which is precisely what it did until the
        // read-only and large-push tests found every POST 404ing.
        warp::path!("git" / String / String / "git-upload-pack")
            .and(warp::post())
            .and(warp::header::optional::<String>("authorization"))
            .and(warp::header::headers_cloned())
            .and(warp::body::stream())
            .and_then(move |ns: String, repo: String, auth, hdrs, body| {
                let door = door.clone();
                async move {
                    Ok::<_, Rejection>(
                        proxy(
                            door,
                            ns,
                            repo,
                            GitVerb::UploadPack,
                            None,
                            auth,
                            hdrs,
                            Some(stream_body(body)),
                        )
                        .await,
                    )
                }
            })
    };

    let receive = warp::path!("git" / String / String / "git-receive-pack")
        .and(warp::post())
        // NO `content_length_limit`. A push is a streamed pack of
        // unknown length; the file API's own upload route answers 411
        // to exactly this shape, which is the trap the design names.
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::header::headers_cloned())
        .and(warp::body::stream())
        .and_then(move |ns: String, repo: String, auth, hdrs, body| {
            let door = door.clone();
            async move {
                Ok::<_, Rejection>(
                    proxy(
                        door,
                        ns,
                        repo,
                        GitVerb::ReceivePack,
                        None,
                        auth,
                        hdrs,
                        Some(stream_body(body)),
                    )
                    .await,
                )
            }
        });

    info_refs.or(upload).unify().or(receive).unify()
}

/// Adapt warp's request-body stream into a reqwest streaming body.
///
/// No `collect()` appears in it, on purpose: a push is a pack of
/// unbounded size and buffering one per concurrent writer is how a
/// door with a small memory limit is killed by two agents pushing at
/// once.
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

#[allow(clippy::too_many_arguments)]
async fn proxy(
    door: Arc<GitDoor>,
    ns: String,
    repo_segment: String,
    verb: GitVerb,
    service: Option<String>,
    auth: Option<String>,
    headers: warp::http::HeaderMap,
    payload: Option<reqwest::Body>,
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
            "this door is read-only; pushes are refused",
            None,
        );
    }
    let name = repo_name(&repo_segment);
    if !plausible_name(&ns) || !plausible_name(&name) {
        return json_err(
            StatusCode::NOT_FOUND,
            "NoSuchRepository",
            "that is not a repository address",
            None,
        );
    }
    // The advertisement names the service it is advertising, and it is
    // the one query parameter that reaches the server — by value, from
    // a list of two.
    if verb == GitVerb::InfoRefs {
        match service.as_deref() {
            Some(s) if SERVICES.contains(&s) => {}
            // No `service=` at all is a DUMB protocol probe. Forge's
            // dumb remote is the bucket, not the door, and answering
            // it here would serve a ref advertisement no smart client
            // asked for.
            _ => {
                return json_err(
                    StatusCode::FORBIDDEN,
                    "SmartHttpOnly",
                    "this door serves the smart protocol only; use a current git client",
                    None,
                )
            }
        }
    }
    if door.cfg.read_only && service.as_deref() == Some("git-receive-pack") {
        return json_err(
            StatusCode::FORBIDDEN,
            "ReadOnly",
            "this door is read-only; pushes are refused",
            None,
        );
    }

    let Some(token) = basic_password(auth.as_deref()) else {
        return challenge("present the pod's projected token as the HTTP basic password");
    };
    let identity = match door.reviewer.review(&token).await {
        Ok(id) => id,
        Err(e) => return challenge(&e),
    };

    let Some(repo) = door.look_up(&ns, &name) else {
        return json_err(
            StatusCode::NOT_FOUND,
            "NoSuchRepository",
            &format!("no FlintRepo named {name:?} in namespace {ns:?}"),
            None,
        );
    };
    if !consumer_allows(repo.spec.consumers.as_ref(), &ns, &identity) {
        // 403 and not 404: the caller authenticated, so telling it the
        // repository exists and it may not reach it is not a leak — and
        // a 404 here would send an operator hunting a missing CR.
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

    let (_view, endpoint) = match door.wait_for_ready(&ns, &name).await {
        Ok(v) => v,
        Err(res) => return res,
    };

    // The upstream URL: the operator's endpoint plus a static suffix.
    // Nothing the caller sent is concatenated into it.
    let mut url = format!("{}{}", endpoint.trim_end_matches('/'), verb.suffix());
    if let Some(s) = service.as_deref() {
        url.push_str("?service=");
        url.push_str(s);
    }

    let mut req = door.http.request(verb.method(), &url).timeout(door.cfg.upstream_timeout);
    for name in GIT_REQUEST_HEADERS {
        if let Some(v) = headers.get(*name) {
            req = req.header(*name, v.as_bytes());
        }
    }
    // Who this is, as the door verified it. The hooks read it as
    // `REMOTE_USER`; nothing the caller sent can reach this header,
    // because the request's headers are built from the allowlist above
    // and this line.
    req = req.header("x-remote-user", identity.username.as_str());
    if let Some(body) = payload {
        req = req.body(body);
    }

    match req.send().await {
        Ok(res) => relay(res),
        Err(e) => json_err(
            StatusCode::BAD_GATEWAY,
            "UpstreamUnreachable",
            &format!("the repository server did not answer: {e}"),
            Some(5),
        ),
    }
}

fn relay(res: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut headers = warp::http::HeaderMap::new();
    for name in GIT_RESPONSE_HEADERS {
        if let Some(v) = res.headers().get(*name) {
            if let (Ok(n), Ok(v)) = (
                warp::http::header::HeaderName::from_bytes(name.as_bytes()),
                warp::http::HeaderValue::from_bytes(v.as_bytes()),
            ) {
                headers.insert(n, v);
            }
        }
    }
    // Streamed, not buffered: a clone is as large as the repository.
    let body = warp::hyper::Body::wrap_stream(res.bytes_stream());
    let mut out = Response::new(body);
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

#[cfg(test)]
mod tests {
    //! End-to-end where it matters, over real sockets.
    //!
    //! The pure decision is tested in `resolve`; what these add is the
    //! part that faces a git client — a fake git server on a real port,
    //! a `FlintRepo` whose `status.gitEndpoint` points at it, and
    //! requests driven through the assembled route table. Every test
    //! that asserts an ABSENCE (the server was not reached, a header
    //! did not arrive) pairs it with a positive control in the same
    //! test, because "nothing happened" is also what a broken rig
    //! produces.

    use super::*;
    use crate::forge_operator::crd::RepoPhase;
    use bytes::Bytes;
    use kube::runtime::{reflector, watcher};
    use std::sync::atomic::AtomicU64;
    use warp::http::HeaderMap;

    /// What the fake git server saw.
    #[derive(Debug, Clone, Default)]
    struct Seen {
        method: String,
        path: String,
        query: String,
        headers: HeaderMap,
        body_len: usize,
    }

    type Log = Arc<Mutex<Vec<Seen>>>;

    async fn fake_git_server(log: Log) -> String {
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
                    log.lock().unwrap().push(Seen {
                        method: m.to_string(),
                        path: p.as_str().to_string(),
                        query: q
                            .iter()
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect::<Vec<_>>()
                            .join("&"),
                        headers: h,
                        body_len: b.len(),
                    });
                    warp::reply::with_header(
                        "001e# service=git-upload-pack\n0000",
                        "content-type",
                        "application/x-git-upload-pack-advertisement",
                    )
                },
            );
        let (addr, srv) = warp::serve(route).bind_ephemeral(([127, 0, 0, 1], 0));
        tokio::spawn(srv);
        format!("http://{addr}/tenant/proj.git")
    }

    fn repo(endpoint: Option<&str>, consumers: Vec<&str>, phase: RepoPhase) -> FlintRepo {
        use crate::forge_operator::crd::{FlintRepoSpec, FlintRepoStatus};
        let mut r = FlintRepo::new(
            "proj",
            FlintRepoSpec {
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
                idle: None,
                export: None,
                wip_snapshots: None,
            },
        );
        r.metadata.namespace = Some("tenant".into());
        r.status = Some(FlintRepoStatus {
            phase: Some(phase),
            git_endpoint: endpoint.map(String::from),
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

    /// Counts what reaches the apiserver, and can be told to fail.
    struct CountingReviewer {
        calls: Arc<AtomicU64>,
        verdict: Result<Identity, String>,
    }

    #[async_trait::async_trait]
    impl Reviewer for CountingReviewer {
        async fn review(&self, _token: &str) -> Result<Identity, String> {
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
    ) -> Arc<GitDoor> {
        crate::install_crypto_provider();
        // Never dialled: the reviewer is a double and the wake PATCH is
        // best effort, which one test asserts explicitly.
        let client = kube::Client::try_from(
            kube::Config::new("http://127.0.0.1:1".parse().expect("uri")),
        )
        .expect("client");
        Arc::new(GitDoor {
            client,
            repos: store_of(repos),
            http: reqwest::Client::builder().build().expect("http"),
            cfg: GitConfig {
                wake_wait: Duration::from_millis(200),
                upstream_timeout: Duration::from_secs(5),
                read_only,
                ..GitConfig::default()
            },
            ready: Arc::new(AtomicBool::new(true)),
            reviewer,
        })
    }

    fn basic(token: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("pod:{token}"))
        )
    }

    const ADVERT: &str = "/git/tenant/proj.git/info/refs";

    /// git asks unauthenticated first and only sends a credential after
    /// a challenge, so the 401 must carry `WWW-Authenticate` — without
    /// it every clone fails instead of prompting the helper. The
    /// control in the same test is the authenticated request, which
    /// does reach the server.
    #[tokio::test]
    async fn an_unauthenticated_request_is_challenged_and_never_dialled() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_git_server(log.clone()).await;
        let calls = Arc::new(AtomicU64::new(0));
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["agent-runner"], RepoPhase::Ready)],
            Arc::new(CountingReviewer {
                calls: calls.clone(),
                verdict: Ok(identity("tenant", "agent-runner")),
            }),
            false,
        );
        let routes = routes(door);

        let res = warp::test::request()
            .method("GET")
            .path(&format!("{ADVERT}?service=git-upload-pack"))
            .reply(&routes)
            .await;
        assert_eq!(res.status(), 401);
        assert!(
            res.headers().get("www-authenticate").is_some(),
            "git needs the challenge to send its credential at all"
        );
        assert!(log.lock().unwrap().is_empty(), "the server was not dialled");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no token, no TokenReview");

        let res = warp::test::request()
            .method("GET")
            .path(&format!("{ADVERT}?service=git-upload-pack"))
            .header("authorization", basic("tok"))
            .reply(&routes)
            .await;
        assert_eq!(res.status(), 200, "the control must reach the server");
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    /// What the server learns, and what it must not. `X-Remote-User` is
    /// the door's verdict; `Git-Protocol` is what makes the session v2
    /// and therefore what makes bundle URIs possible at all; the
    /// caller's `Authorization` stops at the door.
    #[tokio::test]
    async fn the_principal_is_forwarded_and_the_credential_is_not() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_git_server(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["agent-runner"], RepoPhase::Ready)],
            Arc::new(CountingReviewer {
                calls: Arc::new(AtomicU64::new(0)),
                verdict: Ok(identity("tenant", "agent-runner")),
            }),
            false,
        );
        let res = warp::test::request()
            .method("GET")
            .path(&format!("{ADVERT}?service=git-upload-pack"))
            .header("authorization", basic("tok"))
            .header("git-protocol", "version=2")
            .reply(&routes(door))
            .await;
        assert_eq!(res.status(), 200);

        let seen = log.lock().unwrap()[0].clone();
        assert_eq!(
            seen.headers.get("x-remote-user").map(|v| v.to_str().unwrap()),
            Some("system:serviceaccount:tenant:agent-runner")
        );
        assert_eq!(
            seen.headers.get("git-protocol").map(|v| v.to_str().unwrap()),
            Some("version=2"),
            "dropping this silently degrades every clone to protocol v0"
        );
        assert!(seen.headers.get("authorization").is_none(), "the credential stops at the door");
        // The path invariant: the endpoint the OPERATOR wrote, plus a
        // static suffix. The caller's segments were a lookup key.
        assert_eq!(seen.path, "/tenant/proj.git/info/refs");
        assert_eq!(seen.query, "service=git-upload-pack");
    }

    /// The one query parameter that reaches the server names a program
    /// it will run, so it is filtered by value. A request with none is
    /// a dumb-protocol probe, and forge's dumb remote is the bucket.
    #[tokio::test]
    async fn only_the_two_known_services_reach_the_server() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_git_server(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["agent-runner"], RepoPhase::Ready)],
            Arc::new(CountingReviewer {
                calls: Arc::new(AtomicU64::new(0)),
                verdict: Ok(identity("tenant", "agent-runner")),
            }),
            false,
        );
        let routes = routes(door);
        for (query, want) in [
            ("", 403),
            ("?service=git-daemon-export-ok", 403),
            ("?service=../../etc", 403),
            ("?service=git-upload-pack", 200),
        ] {
            let res = warp::test::request()
                .method("GET")
                .path(&format!("{ADVERT}{query}"))
                .header("authorization", basic("tok"))
                .reply(&routes)
                .await;
            assert_eq!(res.status(), want, "service {query:?}");
        }
        let seen = log.lock().unwrap();
        assert_eq!(seen.len(), 1, "only the allowed service was proxied");
    }

    /// A principal the repository does not list is refused before the
    /// server is dialled, and told which principal was refused — the
    /// 403 is an operator's error message.
    #[tokio::test]
    async fn a_principal_outside_consumers_is_refused() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_git_server(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["someone-else"], RepoPhase::Ready)],
            Arc::new(CountingReviewer {
                calls: Arc::new(AtomicU64::new(0)),
                verdict: Ok(identity("tenant", "agent-runner")),
            }),
            false,
        );
        let res = warp::test::request()
            .method("GET")
            .path(&format!("{ADVERT}?service=git-upload-pack"))
            .header("authorization", basic("tok"))
            .reply(&routes(door))
            .await;
        assert_eq!(res.status(), 403);
        let body = String::from_utf8_lossy(res.body()).to_string();
        assert!(body.contains("agent-runner"), "{body}");
        assert!(log.lock().unwrap().is_empty(), "the server was not dialled");
    }

    /// A bare consumer name means "in this repository's namespace". The
    /// alternative — matching a bare name anywhere — would make
    /// `agent-runner` in a namespace the repository's owner has never
    /// heard of into a consumer of it.
    #[test]
    fn a_bare_consumer_name_does_not_cross_namespaces() {
        let c = Consumers { service_accounts: vec!["agent-runner".into()] };
        assert!(consumer_allows(Some(&c), "tenant", &identity("tenant", "agent-runner")));
        assert!(!consumer_allows(Some(&c), "tenant", &identity("other", "agent-runner")));

        let q = Consumers {
            service_accounts: vec!["system:serviceaccount:other:agent-runner".into()],
        };
        assert!(consumer_allows(Some(&q), "tenant", &identity("other", "agent-runner")));
        assert!(!consumer_allows(Some(&q), "tenant", &identity("tenant", "agent-runner")));

        let all = Consumers { service_accounts: vec!["*".into()] };
        assert!(consumer_allows(Some(&all), "tenant", &identity("anywhere", "anyone")));
        // No list at all is nobody: the credential IS the pod's token,
        // so an absent allow-list must not mean an open repository.
        assert!(!consumer_allows(None, "tenant", &identity("tenant", "agent-runner")));
    }

    /// A read-only door refuses a push and does not dial. The control
    /// is the fetch in the same test, which does.
    #[tokio::test]
    async fn a_read_only_door_refuses_the_push_and_serves_the_fetch() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_git_server(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["agent-runner"], RepoPhase::Ready)],
            Arc::new(CountingReviewer {
                calls: Arc::new(AtomicU64::new(0)),
                verdict: Ok(identity("tenant", "agent-runner")),
            }),
            true,
        );
        let routes = routes(door);
        let res = warp::test::request()
            .method("POST")
            .path("/git/tenant/proj.git/git-receive-pack")
            .header("authorization", basic("tok"))
            .body("0000")
            .reply(&routes)
            .await;
        assert_eq!(res.status(), 403);
        assert!(log.lock().unwrap().is_empty());

        let res = warp::test::request()
            .method("POST")
            .path("/git/tenant/proj.git/git-upload-pack")
            .header("authorization", basic("tok"))
            .body("0000")
            .reply(&routes)
            .await;
        assert_eq!(res.status(), 200, "a fetch is a read however large it is");
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    /// The cache is the feature: a clone is several requests and each
    /// carries the token, so without it a thousand agents are thousands
    /// of TokenReviews. Counting what reaches the double is the only
    /// way to see it work.
    #[tokio::test]
    async fn the_review_cache_spares_the_apiserver() {
        let calls = Arc::new(AtomicU64::new(0));
        let inner = Arc::new(CountingReviewer {
            calls: calls.clone(),
            verdict: Ok(identity("tenant", "agent-runner")),
        });
        let reviewer = CachingReviewer::new(inner, Duration::from_millis(150));
        for _ in 0..8 {
            reviewer.review("same-token").await.expect("review");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "eight requests, one review");
        reviewer.review("a-different-token").await.expect("review");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a different token is a different verdict");
        tokio::time::sleep(Duration::from_millis(200)).await;
        reviewer.review("same-token").await.expect("review");
        assert_eq!(calls.load(Ordering::SeqCst), 3, "the TTL is also how fast a deleted pod loses access");
    }

    /// A refusal is cached (an expired token retries in a loop); an
    /// apiserver that could not be reached is NOT, because caching it
    /// would keep the door shut for the whole TTL after it came back.
    #[tokio::test]
    async fn a_refusal_is_cached_and_an_unreachable_apiserver_is_not() {
        let calls = Arc::new(AtomicU64::new(0));
        let refused = CachingReviewer::new(
            Arc::new(CountingReviewer {
                calls: calls.clone(),
                verdict: Err("token is not authenticated".into()),
            }),
            Duration::from_secs(60),
        );
        for _ in 0..4 {
            assert!(refused.review("t").await.is_err());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a refusal is answered from the cache");

        let calls = Arc::new(AtomicU64::new(0));
        let down = CachingReviewer::new(
            Arc::new(CountingReviewer {
                calls: calls.clone(),
                verdict: Err("TokenReview: connection refused".into()),
            }),
            Duration::from_secs(60),
        );
        for _ in 0..4 {
            assert!(down.review("t").await.is_err());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 4, "a transport failure is retried, not cached");
    }

    /// A push is a streamed pack of unknown length. The file API's
    /// upload route guards itself with `content_length_limit` and would
    /// answer 411 to exactly this shape; this route has no limit, and
    /// the assertion is that the whole body arrived.
    #[tokio::test]
    async fn a_large_push_streams_through_with_no_length_limit() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_git_server(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["agent-runner"], RepoPhase::Ready)],
            Arc::new(CountingReviewer {
                calls: Arc::new(AtomicU64::new(0)),
                verdict: Ok(identity("tenant", "agent-runner")),
            }),
            false,
        );
        let pack = vec![b'x'; 4 * 1024 * 1024];
        let res = warp::test::request()
            .method("POST")
            .path("/git/tenant/proj.git/git-receive-pack")
            .header("authorization", basic("tok"))
            .header("content-type", "application/x-git-receive-pack-request")
            .body(pack.clone())
            .reply(&routes(door))
            .await;
        assert_eq!(res.status(), 200);
        let seen = log.lock().unwrap()[0].clone();
        assert_eq!(seen.body_len, pack.len(), "the whole pack reached the server");
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.path, "/tenant/proj.git/git-receive-pack");
        assert_eq!(
            seen.headers.get("content-type").map(|v| v.to_str().unwrap()),
            Some("application/x-git-receive-pack-request")
        );
    }

    /// A repository nobody registered is a 404 that says so, and a
    /// nonsense address never reaches the store at all.
    #[tokio::test]
    async fn an_unknown_repository_is_a_404() {
        let door = door_with(
            vec![],
            Arc::new(CountingReviewer {
                calls: Arc::new(AtomicU64::new(0)),
                verdict: Ok(identity("tenant", "agent-runner")),
            }),
            false,
        );
        let routes = routes(door);
        for path in [
            "/git/tenant/proj.git/info/refs?service=git-upload-pack",
            "/git/TENANT/proj.git/info/refs?service=git-upload-pack",
            "/git/tenant/..%2f..%2fetc/info/refs?service=git-upload-pack",
        ] {
            let res = warp::test::request()
                .method("GET")
                .path(path)
                .header("authorization", basic("tok"))
                .reply(&routes)
                .await;
            assert_eq!(res.status(), 404, "{path}");
        }
    }

    /// A repository the operator has parked is woken, and the request
    /// is HELD rather than refused — git clients do not retry a 503.
    /// Here the wake PATCH cannot be delivered (the client points at a
    /// dead address), so the hold expires and the answer says so, which
    /// is the failure mode an operator has to be able to read.
    #[tokio::test]
    async fn a_parked_repository_holds_the_request_rather_than_refusing_it() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_git_server(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["agent-runner"], RepoPhase::IdleSuspended)],
            Arc::new(CountingReviewer {
                calls: Arc::new(AtomicU64::new(0)),
                verdict: Ok(identity("tenant", "agent-runner")),
            }),
            false,
        );
        let res = warp::test::request()
            .method("GET")
            .path(&format!("{ADVERT}?service=git-upload-pack"))
            .header("authorization", basic("tok"))
            .reply(&routes(door))
            .await;
        assert_eq!(res.status(), 503);
        let body = String::from_utf8_lossy(res.body()).to_string();
        assert!(body.contains("starting"), "{body}");
        assert!(log.lock().unwrap().is_empty(), "a parked repo is never dialled");
    }

    /// An admin-suspended repository is refused outright: waking it
    /// here would be the door quietly reversing an operator's decision.
    #[tokio::test]
    async fn an_admin_suspended_repository_is_refused_not_woken() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let endpoint = fake_git_server(log.clone()).await;
        let door = door_with(
            vec![repo(Some(&endpoint), vec!["agent-runner"], RepoPhase::Suspended)],
            Arc::new(CountingReviewer {
                calls: Arc::new(AtomicU64::new(0)),
                verdict: Ok(identity("tenant", "agent-runner")),
            }),
            false,
        );
        let res = warp::test::request()
            .method("GET")
            .path(&format!("{ADVERT}?service=git-upload-pack"))
            .header("authorization", basic("tok"))
            .reply(&routes(door))
            .await;
        assert_eq!(res.status(), 409);
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn the_basic_password_is_the_token_and_the_username_is_ignored() {
        assert_eq!(basic_password(Some(&basic("abc"))).as_deref(), Some("abc"));
        // git sends an empty username with some helpers; the password
        // is the whole credential.
        let empty_user =
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(":abc"));
        assert_eq!(basic_password(Some(&empty_user)).as_deref(), Some("abc"));
        assert_eq!(basic_password(None), None);
        assert_eq!(basic_password(Some("Bearer abc")), None);
        assert_eq!(basic_password(Some("Basic not-base64!!")), None);
    }

    #[test]
    fn a_repository_is_addressable_with_or_without_the_dot_git() {
        assert_eq!(repo_name("proj.git"), "proj");
        assert_eq!(repo_name("proj"), "proj");
        assert_eq!(repo_name("my.repo.git"), "my.repo");
    }

    /// The upstream path is a `&'static str` by construction. A test
    /// pins it, because the whole invariant is that no caller-supplied
    /// byte can ever appear in it.
    #[test]
    fn every_upstream_suffix_is_static() {
        for v in [GitVerb::InfoRefs, GitVerb::UploadPack, GitVerb::ReceivePack] {
            let s: &'static str = v.suffix();
            assert!(s.starts_with('/'), "{s}");
            assert!(!s.contains(".."), "{s}");
        }
        assert!(GitVerb::ReceivePack.is_mutation());
        assert!(!GitVerb::UploadPack.is_mutation(), "a fetch is a read however large it is");
        assert!(!GitVerb::InfoRefs.is_mutation());
    }
}
