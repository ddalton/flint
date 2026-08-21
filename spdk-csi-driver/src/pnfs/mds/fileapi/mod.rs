//! The hub's HTTP file API — browse and edit a project without mounting it.
//!
//! Six endpoints, each a translation of an NFS compound (see
//! [`hubfs`], which explains why it routes through the dispatcher rather
//! than the filesystem):
//!
//! | Method | Path | Compound |
//! |---|---|---|
//! | GET | `/files?path=&recursive=&limit=&cursor=` | PUTROOTFH + LOOKUP* + READDIR |
//! | GET | `/files/content?path=` | LOOKUP* + READ |
//! | PUT | `/files/content?path=` | OPEN(CREATE) + WRITE* + COMMIT + CLOSE + [VERIFY +] RENAME |
//! | DELETE | `/files/content?path=` | [VERIFY +] REMOVE |
//! | POST | `/files/folder` | CREATE(NF4DIR) |
//! | POST | `/files/move` | [VERIFY +] SAVEFH + RENAME |
//!
//! ## Conditional requests
//!
//! Every object carries an `ETag` — on downloads, on upload responses,
//! and on every listing entry. It is rendered from the fattr4 CHANGE
//! attribute and the fileid, which means it is the SAME validator a
//! mounted client uses to order its cache: a UI holding an entity-tag
//! and a process holding a change value are talking about one version
//! of one file, not two schemes that happen to agree.
//!
//! `If-Match` on a write becomes a VERIFY (RFC 5661 §18.30) inside the
//! same compound as the RENAME or REMOVE it guards. A compound stops at
//! its first error, so a file that moved under the caller is never
//! replaced — the answer is 412 and the write is refused whole.
//! `If-None-Match: *` is create-if-absent, and `If-None-Match` on a GET
//! revalidates to 304.
//!
//! **What this is not: a lock.** A COMPOUND is explicitly not atomic,
//! so another writer can still interleave between the VERIFY and the
//! mutation. This detects a lost update between callers that use it; it
//! does not exclude a client that has the volume mounted. That is not a
//! shortfall against some stronger HTTP idiom — it is exactly the
//! strength of NFS's own optimistic concurrency control, which this
//! surface is re-exposing rather than reinventing. Describe it to
//! callers as detection, never as exclusion.
//!
//! The drill measures the difference rather than asserting it: eight
//! writers appending to one file, 200 writes. The control loses 168-174
//! of them; `If-Match` loses 32-66 on an idle machine and 90-102 under
//! CPU load. So the benefit ranges from 5x down to under 2x, and the
//! residual from 16% to 51%. CPU contention widens the VERIFY→RENAME
//! gap by descheduling a task inside it, which means the guard is
//! weakest exactly when concurrent writers are most likely.
//!
//! ## Where this listens, and why it matters
//!
//! Beside `/status` on the health port — never on the consumer-facing
//! Service. That Service can be a LoadBalancer, and publishing a
//! read-write file API on it would put the whole volume on the internet.
//! The health port is ClusterIP-only and carries a bearer token besides.
//!
//! ## Two things this surface must not do
//!
//! **Report a truncated download as success.** A tiered file's bytes may
//! be in S3, and eviction can happen while a response body is still
//! streaming. `Content-Length` is set from the size sampled when the
//! transfer starts, and if the body cannot deliver exactly that many
//! bytes the stream returns an error so the connection resets. A short
//! body under HTTP 200 is a silently corrupt file on the caller's disk.
//!
//! Downloads are split at `streamThresholdBytes` (8 MiB) because that
//! guarantee has two prices and only one of them scales. At or below the
//! threshold the body is buffered whole, so a shrink or a rename-over is
//! caught BEFORE the status line ships and answered as a clean 409.
//! Above it the body streams, memory stays O(chunk) whatever the file
//! size, and the same conditions instead end the stream with an error —
//! a reset connection, never a clean short body. Buffering everything
//! bounded hub memory by the DOWNLOAD CAP (5 GiB by default): a 512 MiB
//! request measured `VmHWM` 30 MB → 541 MiB, and under a 256Mi limit the
//! same GET was OOM-killed, taking the NFS export down with it because
//! one process serves both. Hubs are 1:1 with projects, so that was a
//! per-project cost.
//!
//! Both the cap and the threshold are checked against the RANGE, not the
//! file — so a small `Range` of a huge file is still buffered, and still
//! gets the clean 409.
//!
//! **Keep the project awake by existing.** Every call here is real user
//! intent and counts as activity, which is correct for a person clicking
//! through files and fatal for a liveness poller — a UI refreshing on a
//! timer would pin every project in the fleet awake forever, and the
//! idle ladder would never fire. The front door polls `/status` for
//! liveness; `/status` is deliberately NOT activity.
//!
//! **Read a 304 as free.** It is cheap in bytes and identical in
//! activity: a conditional GET went through the dispatcher exactly as an
//! unconditional one did. Revalidating on a timer pins a project awake
//! precisely as re-downloading on a timer does. Conditional requests
//! make that poll cheaper and therefore more tempting, which is why the
//! rule is written here next to the feature that invites it.

pub mod hubfs;
pub mod token;

use hubfs::{Entry, FsError, FsPath, HubFs, Precondition};
use token::TokenSource;
use crate::nfs::v4::protocol::Nfs4Status;
use bytes::Bytes;
use std::sync::Arc;
use warp::http::StatusCode;
use warp::Filter;

/// How the API is configured and authenticated.
#[derive(Clone)]
pub struct ApiConfig {
    /// Shared secret presented as `Authorization: Bearer <token>`.
    /// Absent = the API is not served at all. It is never optional-auth:
    /// an unauthenticated read-write file API on a volume is not a
    /// degraded mode, it is a breach.
    ///
    /// A [`TokenSource`] rather than a `String`, so a rotated Secret
    /// reaches a running hub without a restart — see [`token`], which
    /// explains why a restart is the wrong price for a credential
    /// change. Whether the routes exist at all is still decided once, at
    /// boot: absent here means no route table.
    pub token: Option<Arc<TokenSource>>,
    /// Largest single upload accepted, in bytes.
    pub max_upload_bytes: u64,
    /// Largest single download served in one request. A browse click can
    /// otherwise pull an arbitrarily large file out of S3 — real egress,
    /// billed, triggered by a UI.
    pub max_download_bytes: u64,
    /// How long a download waits for hydration before answering 503.
    pub hydrate_wait_secs: u64,
    /// Downloads at or below this size are buffered whole; larger ones
    /// stream. This is the hub's download memory bound — see
    /// `FileApiConfig::stream_threshold_bytes`.
    pub stream_threshold_bytes: u64,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            token: None,
            max_upload_bytes: 5 * 1024 * 1024 * 1024,
            max_download_bytes: 5 * 1024 * 1024 * 1024,
            hydrate_wait_secs: 30,
            stream_threshold_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Whether a download of `want` bytes takes the streaming path.
///
/// Named rather than inlined so a test can exercise the REAL decision
/// against the REAL configured threshold. The boundary test used to
/// restate `>` against a local `let t = 8 * 1024 * 1024`, which pinned
/// nothing: changing `stream_threshold_bytes` — or inverting this
/// comparison — left it green.
///
/// The boundary is deliberate. AT the threshold the body is buffered, so
/// the status code and Content-Length are decided with every byte in
/// hand; one byte OVER, memory becomes O(CHUNK) and a mid-read change
/// can no longer be a clean 409.
pub(crate) fn streams_rather_than_buffers(want: u64, cfg: &ApiConfig) -> bool {
    want > cfg.stream_threshold_bytes
}

/// A parsed `If-Match` / `If-None-Match` header value.
struct Validators {
    /// The header was `*` — "any current representation".
    any: bool,
    /// Entity-tags this server minted, already split into their halves.
    tags: Vec<(u64, u64)>,
    /// At least one tag arrived weak (`W/"…"`). Fine for revalidating a
    /// GET, refused on a write: RFC 9110 §13.1.1 requires strong
    /// comparison for `If-Match`, and a weak tag promises only that two
    /// representations are equivalent — not that they are the same
    /// bytes, which is the only thing worth conditioning a write on.
    weak: bool,
}

impl Validators {
    /// Weak comparison, for revalidating a GET.
    fn matches(&self, etag: &str) -> bool {
        self.any || self.tags.iter().any(|t| hubfs::render_etag(t.0, t.1) == etag)
    }
}

/// Parse one precondition header. An entity-tag this server did not
/// mint is an error rather than a non-match: a caller sending a token
/// from somewhere else has a bug, and answering 412 would let it retry
/// forever without ever learning that.
fn parse_validators(raw: &str) -> Result<Validators, String> {
    let raw = raw.trim();
    if raw == "*" {
        return Ok(Validators { any: true, tags: Vec::new(), weak: false });
    }
    let mut out = Validators { any: false, tags: Vec::new(), weak: false };
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (body, weak) = match part.strip_prefix("W/") {
            Some(rest) => (rest.trim(), true),
            None => (part, false),
        };
        match hubfs::parse_etag(body) {
            Some(t) => {
                out.weak |= weak;
                out.tags.push(t);
            }
            None => {
                return Err(format!(
                    "not an entity-tag this server issued: {part} — send back an `etag` \
                     from a listing or a download, quotes included"
                ))
            }
        }
    }
    if out.tags.is_empty() {
        return Err("no usable entity-tag in the precondition header".into());
    }
    Ok(out)
}

/// Turn an `If-Match` header into the condition the mutating compound
/// will carry.
fn write_precondition(raw: &str) -> Result<Precondition, String> {
    let v = parse_validators(raw)?;
    if v.weak {
        return Err("If-Match requires a strong entity-tag; drop the W/ prefix".into());
    }
    if v.any {
        return Ok(Precondition::Exists);
    }
    if v.tags.len() > 1 {
        // A list is legal HTTP and would need one compound per tag to
        // evaluate honestly. Refusing beats picking the first and
        // reporting a guarantee that was never checked.
        return Err("If-Match on a write accepts a single entity-tag or `*`".into());
    }
    let (fileid, change) = v.tags[0];
    Ok(Precondition::Is { fileid, change })
}

/// Does `entry` satisfy `p`? Used only for the cheap pre-checks; the
/// binding evaluation is the VERIFY inside the mutating compound.
fn precondition_holds(p: Precondition, entry: &Entry) -> bool {
    match p {
        Precondition::Exists => true,
        Precondition::Is { fileid, change } => entry.fileid == fileid && entry.change == change,
    }
}

fn precondition_failed(what: &str) -> warp::reply::Response {
    plain(
        StatusCode::PRECONDITION_FAILED,
        &format!("{what} changed since the entity-tag you sent; re-read it and retry"),
    )
}

/// Render a failed mutation, knowing whether a precondition was in play.
///
/// With `If-Match` present, a missing object is a FAILED CONDITION, not
/// a missing resource — RFC 9110 §13.1.1 is explicit that a target with
/// no current representation fails `If-Match`. Answering 404 there
/// would tell a caller its file vanished when what actually happened is
/// that someone replaced it.
fn mutate_err_reply(e: &FsError, conditioned: bool) -> warp::reply::Response {
    // `Stale` belongs here beside `NoEnt`, and the drill is why. Under
    // concurrent writers a mutating compound can reach its target
    // through a filehandle that another caller's rename-over has just
    // invalidated. Semantically that is the SAME event a failed VERIFY
    // reports — the object you conditioned on is not the object here
    // any more — but it arrives as a different status, and a caller
    // told to "handle 412" would meet an unexplained 409 the first time
    // two of its tabs saved at once. One event, one code.
    if conditioned
        && matches!(
            e,
            FsError::Nfs(Nfs4Status::NoEnt)
                | FsError::Nfs(Nfs4Status::Stale)
                | FsError::Nfs(Nfs4Status::FhExpired)
        )
    {
        return precondition_failed("the target");
    }
    err_reply(e)
}

/// Chunk size for both directions. 1 MiB is well inside the NFS write
/// ceiling and keeps the number of compounds per gigabyte modest.
const CHUNK: usize = 1024 * 1024;

#[derive(serde::Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    path: String,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct PathQuery {
    #[serde(default)]
    path: String,
}

#[derive(serde::Deserialize)]
pub struct FolderBody {
    path: String,
}

#[derive(serde::Deserialize)]
pub struct MoveBody {
    from: String,
    to: String,
}

#[derive(serde::Serialize)]
struct OkBody {
    status: String,
}

#[derive(serde::Serialize)]
struct ErrorBody {
    error: String,
    /// The NFS status behind the answer, when there was one. Diagnostics
    /// beat a bare 500 when a caller reports "it stopped working".
    #[serde(skip_serializing_if = "Option::is_none")]
    nfs_status: Option<String>,
}

/// Map an NFS status to the HTTP answer that means the same thing.
///
/// The mapping is the point: a caller must be able to distinguish "no
/// such file" from "the directory is not empty" from "come back, the
/// data is being fetched from S3" — all of which flatten to 500 if the
/// status is thrown away.
fn http_status(e: &FsError) -> StatusCode {
    match e {
        FsError::StaleCursor => StatusCode::GONE,
        FsError::Invalid(_) => StatusCode::BAD_REQUEST,
        FsError::Nfs(s) => match s {
            Nfs4Status::NoEnt => StatusCode::NOT_FOUND,
            Nfs4Status::Exist => StatusCode::CONFLICT,
            Nfs4Status::NotEmpty => StatusCode::CONFLICT,
            Nfs4Status::NotDir | Nfs4Status::IsDir => StatusCode::CONFLICT,
            // The object is a symlink; the server will not follow it and
            // neither will this API. The caller resolves it if it wants
            // to — in its own namespace, where it means something.
            Nfs4Status::SymLink => StatusCode::CONFLICT,
            Nfs4Status::Access | Nfs4Status::Perm => StatusCode::FORBIDDEN,
            Nfs4Status::NoSpc | Nfs4Status::DQuot => StatusCode::INSUFFICIENT_STORAGE,
            Nfs4Status::FBig => StatusCode::PAYLOAD_TOO_LARGE,
            Nfs4Status::Inval | Nfs4Status::BadName | Nfs4Status::NameTooLong => {
                StatusCode::BAD_REQUEST
            }
            // Hydration in flight, or a write gate refusing during a
            // flush epoch swap. Both are "retry", not "failed".
            Nfs4Status::Delay | Nfs4Status::Grace => StatusCode::SERVICE_UNAVAILABLE,
            Nfs4Status::RoFs => StatusCode::FORBIDDEN,
            // A VERIFY inside the mutating compound said the object is
            // no longer the one the caller conditioned on.
            Nfs4Status::NotSame => StatusCode::PRECONDITION_FAILED,
            // The object was replaced while this compound was walking
            // to it — a filehandle minted moments ago no longer
            // resolves. That is "someone else wrote; come back", the
            // same answer a mid-read replacement gets, and emphatically
            // not a server fault. Under concurrent writers to one path
            // this is ordinary, and 500 would tell a caller to stop
            // rather than retry. (Drill-found.)
            Nfs4Status::Stale | Nfs4Status::FhExpired => StatusCode::CONFLICT,
            // Transient server-side scarcity, retryable by definition.
            Nfs4Status::Resource => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        },
    }
}

fn err_reply(e: &FsError) -> warp::reply::Response {
    use warp::Reply;
    let code = http_status(e);
    let body = ErrorBody {
        error: match e {
            FsError::Invalid(m) => (*m).to_string(),
            FsError::StaleCursor => "listing cursor is no longer valid; restart the listing".into(),
            FsError::Nfs(s) => format!("{s:?}"),
        },
        nfs_status: e.status().map(|s| format!("{s:?}")),
    };
    let mut res = warp::reply::json(&body).into_response();
    *res.status_mut() = code;
    if code == StatusCode::SERVICE_UNAVAILABLE {
        // Tell the caller when to come back rather than making it
        // guess. The two 503 causes have very different waits:
        // hydration is seconds, a grace period is up to ninety of them,
        // and a client polling every two seconds through a grace window
        // is 45 pointless requests.
        let secs = retry_after_secs(e);
        if let Ok(v) = warp::http::HeaderValue::from_str(&secs.to_string()) {
            res.headers_mut().insert("retry-after", v);
        }
    }
    res
}

fn retry_after_secs(e: &FsError) -> u64 {
    match e {
        FsError::Nfs(Nfs4Status::Grace) => 5,
        _ => 2,
    }
}

fn plain(code: StatusCode, msg: &str) -> warp::reply::Response {
    use warp::Reply;
    let mut res = warp::reply::json(&ErrorBody { error: msg.to_string(), nfs_status: None })
        .into_response();
    *res.status_mut() = code;
    res
}

/// A SUCCESS body.
///
/// Separate from [`plain`] because these two must not share a shape:
/// `plain` serialises through `ErrorBody`, so every successful mutation
/// used to answer `{"error":"created"}` — success reported under an
/// `error` key, which reads as a failure to anything scanning for one
/// and is simply wrong to publish.
fn note(code: StatusCode, msg: &str) -> warp::reply::Response {
    use warp::Reply;
    let mut res = warp::reply::json(&OkBody { status: msg.to_string() }).into_response();
    *res.status_mut() = code;
    res
}

/// The whole route table, with its own rejections already rendered.
///
/// Recovery is applied HERE rather than left to the caller: warp turns
/// an unhandled rejection into a 404, so forgetting it would make a
/// missing bearer token look like a missing endpoint — the one error a
/// caller must not misread.
pub fn routes(
    fs: Arc<HubFs>,
    cfg: ApiConfig,
) -> impl Filter<Extract = (warp::reply::Response,), Error = warp::Rejection> + Clone {
    raw_routes(fs, cfg)
        .recover(recover)
        .unify()
        .map(|r: warp::reply::Response| r)
}

/// [`routes`], but refusing everything until the hub is actually
/// serving.
///
/// The API is bound on the same listener as `/status`, which comes up
/// before the tier and before the NFS listener — deliberately, so a
/// slow epoch claim or a DR import is observable rather than looking
/// like a wedge. But that means the file routes exist during a window
/// when the namespace is still being rebuilt from the bucket: a listing
/// then shows a partial tree as though it were the whole one, and a
/// write races the import that is placing files. Both answer 503 with a
/// Retry-After instead, which is the same shape a caller already
/// handles for hydration.
pub fn routes_gated(
    fs: Arc<HubFs>,
    cfg: ApiConfig,
    status: Arc<crate::pnfs::mds::status::HubStatus>,
) -> impl Filter<Extract = (warp::reply::Response,), Error = warp::Rejection> + Clone {
    let gate = warp::any().map(move || status.phase()).and_then(
        |phase: crate::pnfs::mds::status::HubPhase| async move {
            use crate::pnfs::mds::status::HubPhase as P;
            match phase {
                // Sweeping serves: the listener is up and the tree is
                // whole; only foreign keys are still being folded in.
                P::Serving | P::Sweeping => Ok(()),
                _ => Err(warp::reject::custom(NotReady(phase))),
            }
        },
    );
    gate.and(raw_routes(fs, cfg))
        .map(|_, r: warp::reply::Response| r)
        .recover(recover)
        .unify()
        .map(|r: warp::reply::Response| r)
}

fn raw_routes(
    fs: Arc<HubFs>,
    cfg: ApiConfig,
) -> impl Filter<Extract = (warp::reply::Response,), Error = warp::Rejection> + Clone {
    let auth = auth_filter(cfg.token.clone());

    let list = {
        let fs = fs.clone();
        warp::path!("files")
            .and(warp::get())
            .and(auth.clone())
            .and(warp::query::<ListQuery>())
            .then(move |_, q: ListQuery| {
                let fs = fs.clone();
                async move { handle_list(fs, q).await }
            })
    };

    let download = {
        let fs = fs.clone();
        let cfg = cfg.clone();
        warp::path!("files" / "content")
            .and(warp::get())
            .and(auth.clone())
            .and(warp::query::<PathQuery>())
            .and(warp::header::optional::<String>("range"))
            .and(warp::header::optional::<String>("if-none-match"))
            .then(
                move |_, q: PathQuery, range: Option<String>, inm: Option<String>| {
                    let fs = fs.clone();
                    let cfg = cfg.clone();
                    async move { handle_download(fs, cfg, q, range, inm).await }
                },
            )
    };

    let upload = {
        let fs = fs.clone();
        let cfg = cfg.clone();
        warp::path!("files" / "content")
            .and(warp::put())
            .and(auth.clone())
            .and(warp::query::<PathQuery>())
            .and(warp::header::optional::<String>("if-match"))
            .and(warp::header::optional::<String>("if-none-match"))
            .and(warp::body::content_length_limit(cfg.max_upload_bytes))
            .and(warp::body::bytes())
            .then(
                move |_, q: PathQuery, im: Option<String>, inm: Option<String>, body: Bytes| {
                    let fs = fs.clone();
                    async move { handle_upload(fs, q, body, im, inm).await }
                },
            )
    };

    let delete = {
        let fs = fs.clone();
        warp::path!("files" / "content")
            .and(warp::delete())
            .and(auth.clone())
            .and(warp::query::<PathQuery>())
            .and(warp::header::optional::<String>("if-match"))
            .then(move |_, q: PathQuery, im: Option<String>| {
                let fs = fs.clone();
                async move { handle_delete(fs, q, im).await }
            })
    };

    let folder = {
        let fs = fs.clone();
        warp::path!("files" / "folder")
            .and(warp::post())
            .and(auth.clone())
            .and(warp::body::json::<FolderBody>())
            .then(move |_, b: FolderBody| {
                let fs = fs.clone();
                async move { handle_folder(fs, b).await }
            })
    };

    let mv = {
        let fs = fs.clone();
        warp::path!("files" / "move")
            .and(warp::post())
            .and(auth)
            .and(warp::header::optional::<String>("if-match"))
            .and(warp::body::json::<MoveBody>())
            .then(move |_, im: Option<String>, b: MoveBody| {
                let fs = fs.clone();
                async move { handle_move(fs, b, im).await }
            })
    };

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

/// Bearer-token gate.
///
/// A configured-but-absent token is a hard refusal, not a fallback to
/// open: this route table can rewrite any file in the project.
fn auth_filter(
    token: Option<Arc<TokenSource>>,
) -> impl Filter<Extract = ((),), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("authorization").and_then(move |given: Option<String>| {
        let source = token.clone();
        async move {
            let Some(source) = source else {
                // No token configured: the route table is not mounted in
                // that case, so this is unreachable — deny anyway rather
                // than depend on a caller elsewhere getting it right.
                return Err(warp::reject::custom(Unauthorized));
            };
            // Read per request, not per router: a rotation lands on the
            // next request rather than the next pod.
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

#[derive(Debug)]
struct Unauthorized;
impl warp::reject::Reject for Unauthorized {}

#[derive(Debug)]
struct NotReady(crate::pnfs::mds::status::HubPhase);
impl warp::reject::Reject for NotReady {}

/// Compare without an early return on the first differing byte, so the
/// time taken does not reveal how much of a guessed token was right.
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

async fn handle_list(fs: Arc<HubFs>, q: ListQuery) -> warp::reply::Response {
    use warp::Reply;
    let path = match FsPath::parse(&q.path) {
        Ok(p) => p,
        Err(e) => return err_reply(&e),
    };
    let limit = q.limit.unwrap_or(1000).clamp(1, 10_000);
    let res = if q.recursive {
        fs.list_recursive(&path, limit).await
    } else {
        fs.list_page(&path, q.cursor.as_deref(), limit).await
    };
    match res {
        Ok(listing) => warp::reply::json(&listing).into_response(),
        Err(e) => err_reply(&e),
    }
}

/// Parse a single-range `Range: bytes=a-b` header. Multi-range is not
/// supported and is answered by serving the whole object, which is what
/// RFC 9110 permits when a server chooses to ignore the header.
fn parse_range(h: &str, size: u64) -> Option<(u64, u64)> {
    let spec = h.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", suffix) => {
            // `-N`: the last N bytes.
            let n: u64 = suffix.parse().ok()?;
            (size.saturating_sub(n), size.saturating_sub(1))
        }
        (s, "") => (s.parse().ok()?, size.saturating_sub(1)),
        (s, e) => (s.parse().ok()?, e.parse().ok()?),
    };
    if size == 0 || start > end || start >= size {
        return None;
    }
    Some((start, end.min(size - 1)))
}

/// Did only the hydration move under us, or did the file actually change?
///
/// Called after a read probe has forced a cold file local, to decide
/// whether the post-hydration stat may become the baseline the terminal
/// read guard compares against. Identity and logical size are the two
/// things a hydration must NOT alter — it restores the bytes the
/// manifest already named, into the same inode — so if both survived,
/// the only thing that moved was `change`, and that is the tier's own
/// pwrite rather than a writer.
///
/// Deliberately ignores `change` itself: that is the whole point, since
/// hydration always moves it. Everything a writer could do that matters
/// here shows up as a new `fileid` (rename-over, re-create) or a new
/// size (truncate, append).
fn hydration_is_benign(before: (u64, u64), after: (u64, u64)) -> bool {
    before == after
}

async fn handle_download(
    fs: Arc<HubFs>,
    cfg: ApiConfig,
    q: PathQuery,
    range: Option<String>,
    if_none_match: Option<String>,
) -> warp::reply::Response {
    let path = match FsPath::parse(&q.path) {
        Ok(p) => p,
        Err(e) => return err_reply(&e),
    };
    if path.is_root() {
        return plain(StatusCode::BAD_REQUEST, "path names the export root, not a file");
    }

    // Stat first: it settles the status code (404 / 409-on-a-directory)
    // and gives the size the response commits to.
    let entry = match fs.stat(&path).await {
        Ok(e) => e,
        Err(e) => return err_reply(&e),
    };
    match entry.kind {
        "file" => {}
        "directory" => {
            return plain(StatusCode::CONFLICT, "path is a directory; list it with GET /files")
        }
        // A symlink is DATA here, never something to follow. Its target
        // means something only in the caller's namespace.
        "symlink" => {
            return plain(
                StatusCode::CONFLICT,
                "path is a symbolic link; read its target from the listing",
            )
        }
        _ => return plain(StatusCode::CONFLICT, "path is not a regular file"),
    }

    // Revalidation, before any byte is moved. On an evicted file this
    // is the difference between a 304 and a hydration: the bytes would
    // come back from S3 as real, billed egress to answer a request whose
    // answer is "you already have it".
    //
    // A 304 is still ACTIVITY. That is deliberate and it is the trap to
    // watch: a UI that revalidates on a timer keeps the project awake
    // exactly as a UI that re-downloads does, and the idle ladder never
    // fires. Liveness belongs on /status, which is not activity.
    let etag = hubfs::render_etag(entry.fileid, entry.change);
    if let Some(raw) = if_none_match.as_deref() {
        match parse_validators(raw) {
            Ok(v) if v.matches(&etag) => {
                let mut res = warp::reply::Response::new(Vec::<u8>::new().into());
                *res.status_mut() = StatusCode::NOT_MODIFIED;
                if let Ok(v) = warp::http::HeaderValue::from_str(&etag) {
                    res.headers_mut().insert("etag", v);
                }
                return res;
            }
            Ok(_) => {}
            Err(m) => return plain(StatusCode::BAD_REQUEST, &m),
        }
    }

    let (start, end, partial) = match range.as_deref().and_then(|h| parse_range(h, entry.size)) {
        Some((s, e)) => (s, e, true),
        None => (0, entry.size.saturating_sub(1), false),
    };
    let want = if entry.size == 0 { 0 } else { end - start + 1 };

    if want > cfg.max_download_bytes {
        return plain(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file exceeds the single-request download cap; use Range to fetch it in pieces",
        );
    }

    // Settle hydration BEFORE the read window opens.
    //
    // The terminal guard below refuses when `change` moved under the
    // read, which is how a rename-over is caught. But the tier rewrites
    // the local inode when it hydrates (pwrite into the marker inode —
    // see `hubfs::render_etag`), so on a cold file the hub's OWN
    // hydration moves `change` and the read fails its own guard. Every
    // first read of every stub after a DR import 409s, and the caller
    // has to ask twice for bytes that were never in doubt; on the
    // streaming path it is worse, because there the mismatch poisons an
    // already-committed 200 instead of answering cleanly.
    //
    // So: force the hydration first with a one-byte probe, park on it
    // exactly as the read loop would, and only then take the baseline
    // the guard compares against. A replacement that lands in this
    // window is still caught — `fileid` and the logical size must both
    // survive it — and the file is local afterwards, so the read that
    // follows cannot trip over a hydration again.
    //
    // Measured on a real cluster before this: 13 of 13 files 409'd on
    // first touch after a hibernate/DR wake, every one of them 200 on
    // the immediate retry.
    let (entry, etag) = if want > 0 {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(cfg.hydrate_wait_secs);
        loop {
            match fs.read_at(&path, start, 1).await {
                Ok(_) => break,
                Err(FsError::Nfs(Nfs4Status::Delay)) => {
                    if std::time::Instant::now() >= deadline {
                        return err_reply(&FsError::Nfs(Nfs4Status::Delay));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                Err(e) => return err_reply(&e),
            }
        }
        match fs.stat(&path).await {
            Ok(after) => {
                if !hydration_is_benign(
                    (entry.fileid, entry.size),
                    (after.fileid, after.size),
                ) {
                    return plain(
                        StatusCode::CONFLICT,
                        "file changed while being read; retry the request",
                    );
                }
                let e2 = hubfs::render_etag(after.fileid, after.change);
                (after, e2)
            }
            Err(e) => return err_reply(&e),
        }
    } else {
        (entry, etag)
    };

    // Too big to hold? Stream it.
    //
    // Buffering buys a real property (see below), but it costs the
    // response size in ANONYMOUS MEMORY, and hubs are 1:1 with
    // projects, so that cost multiplies across the fleet. Measured
    // before this split: a 512 MiB request took `VmHWM` from 30 MB to
    // 541 MiB, and the same GET under a 256Mi limit was OOM-killed —
    // which kills the NFS export with it, because one process serves
    // both. Above the threshold, memory becomes O(CHUNK) regardless of
    // file size.
    //
    // What streaming gives up, stated plainly: the status line is sent
    // before the last byte is read, so a mid-read change cannot be a
    // clean 409 any more. It ends the stream with an error instead, so
    // the connection resets and the caller sees a failed transfer —
    // never a short body under 200, which is a silently corrupt file on
    // their disk.
    if streams_rather_than_buffers(want, &cfg) {
        return streaming_download(fs, cfg, path, entry, etag, start, end, want, partial);
    }

    // Read it all before answering. Buffering costs memory bounded by
    // the stream threshold, and it buys the property that matters: the
    // status code and Content-Length are decided when every byte is
    // already in hand, so a 200 can never be followed by a body that
    // stops early, and a file that shrinks or is renamed over mid-read
    // is refused BEFORE a single byte ships.
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(cfg.hydrate_wait_secs);
    let mut buf = Vec::with_capacity(want.min(8 * 1024 * 1024) as usize);
    let mut offset = start;
    while (buf.len() as u64) < want {
        let remaining = want - buf.len() as u64;
        let chunk = remaining.min(CHUNK as u64) as u32;
        match fs.read_at(&path, offset, chunk).await {
            Ok((data, eof)) => {
                if data.is_empty() {
                    if eof {
                        // The file shrank under us. Answering with what
                        // we have would be a short body under a
                        // Content-Length we already computed.
                        return plain(
                            StatusCode::CONFLICT,
                            "file changed size while being read; retry the request",
                        );
                    }
                    break;
                }
                offset += data.len() as u64;
                buf.extend_from_slice(&data);
            }
            // The bytes are in S3 and hydration has been started. Wait
            // — bounded — because a browse click that succeeds after a
            // pause beats one that fails and has to be repeated.
            Err(FsError::Nfs(Nfs4Status::Delay)) => {
                if std::time::Instant::now() >= deadline {
                    return err_reply(&FsError::Nfs(Nfs4Status::Delay));
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Err(e) => return err_reply(&e),
        }
    }

    if buf.len() as u64 != want {
        return plain(
            StatusCode::CONFLICT,
            "file changed size while being read; retry the request",
        );
    }

    // Re-stat, and refuse if the object moved under the read.
    //
    // The bytes did NOT necessarily come from the file the opening stat
    // described: every chunk re-resolves the path through LOOKUP, so a
    // rename-over mid-read hands back the new file's bytes under the
    // old file's `ETag`. Sizes matching does not rule it out — a
    // same-size replacement passes the length check above — and a
    // validator that names a version other than the bytes shipped
    // beside it is worse than none, because a later `If-Match` built
    // from it is checking the wrong thing.
    //
    // Found by the concurrency drill; no unit test raced hard enough to
    // see it.
    match fs.stat(&path).await {
        Ok(after) if after.fileid == entry.fileid && after.change == entry.change => {}
        Ok(_) | Err(_) => {
            return plain(
                StatusCode::CONFLICT,
                "file changed while being read; retry the request",
            )
        }
    }

    let mut res = warp::reply::Response::new(buf.into());
    let h = res.headers_mut();
    h.insert(
        "content-type",
        warp::http::HeaderValue::from_static("application/octet-stream"),
    );
    h.insert("accept-ranges", warp::http::HeaderValue::from_static("bytes"));
    // The tag comes from the SAME stat that fixed Content-Length, so the
    // validator a caller stores can never describe a different version
    // than the bytes it stored alongside it.
    if let Ok(v) = warp::http::HeaderValue::from_str(&etag) {
        h.insert("etag", v);
    }
    if partial {
        h.insert(
            "content-range",
            warp::http::HeaderValue::from_str(&format!(
                "bytes {start}-{end}/{}",
                entry.size
            ))
            .unwrap(),
        );
        *res.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    res
}

/// Everything a streamed body needs to carry between chunks.
struct DownloadSrc {
    fs: Arc<HubFs>,
    path: FsPath,
    offset: u64,
    remaining: u64,
    deadline: std::time::Instant,
    /// The identity the opening stat described. The bytes must still
    /// belong to THIS file when the last one ships.
    fileid: u64,
    change: u64,
    /// Set once the stream has yielded its terminal item, so `unfold`
    /// stops rather than polling a finished source.
    done: bool,
}

fn stream_err(kind: std::io::ErrorKind, msg: &'static str) -> std::io::Error {
    std::io::Error::new(kind, msg)
}

/// Serve a download whose body is too large to hold in memory.
///
/// `Content-Length` is committed from the opening stat, exactly as the
/// buffered path commits it, and the invariant is the same: the caller
/// either gets exactly that many faithful bytes, or the transfer fails
/// visibly. It just fails as a reset connection rather than a 409,
/// because by then the status line has shipped.
#[allow(clippy::too_many_arguments)]
fn streaming_download(
    fs: Arc<HubFs>,
    cfg: ApiConfig,
    path: FsPath,
    entry: Entry,
    etag: String,
    start: u64,
    end: u64,
    want: u64,
    partial: bool,
) -> warp::reply::Response {
    let src = DownloadSrc {
        fs,
        path,
        offset: start,
        remaining: want,
        deadline: std::time::Instant::now()
            + std::time::Duration::from_secs(cfg.hydrate_wait_secs),
        fileid: entry.fileid,
        change: entry.change,
        done: false,
    };

    let body = futures::stream::unfold(src, |mut s| async move {
        if s.done {
            return None;
        }
        if s.remaining == 0 {
            // Every byte has shipped. The buffered path re-stats before
            // answering, because each chunk re-resolves the path through
            // LOOKUP and a rename-over mid-read would otherwise hand
            // back the NEW file's bytes under the OLD file's ETag. That
            // check has to happen here too — it just cannot be a 409 any
            // more, so a mismatch poisons the stream instead.
            s.done = true;
            return match s.fs.stat(&s.path).await {
                Ok(a) if a.fileid == s.fileid && a.change == s.change => None,
                _ => Some((
                    Err(stream_err(
                        std::io::ErrorKind::Other,
                        "file changed while being read; the body is not a faithful copy",
                    )),
                    s,
                )),
            };
        }
        let chunk = s.remaining.min(CHUNK as u64) as u32;
        loop {
            match s.fs.read_at(&s.path, s.offset, chunk).await {
                Ok((data, _eof)) if data.is_empty() => {
                    // Nothing came back for a range the opening stat
                    // said was inside the file: it shrank, or the read
                    // window closed. Either way the promised
                    // `Content-Length` can no longer be met.
                    s.done = true;
                    return Some((
                        Err(stream_err(
                            std::io::ErrorKind::UnexpectedEof,
                            "file shrank while being read",
                        )),
                        s,
                    ));
                }
                Ok((data, _)) => {
                    s.offset += data.len() as u64;
                    s.remaining -= data.len() as u64;
                    return Some((Ok(data), s));
                }
                // Evicted mid-stream: the bytes are in S3 and hydration
                // has been kicked off. Wait, bounded — the same budget
                // the buffered path uses.
                Err(FsError::Nfs(Nfs4Status::Delay)) => {
                    if std::time::Instant::now() >= s.deadline {
                        s.done = true;
                        return Some((
                            Err(stream_err(
                                std::io::ErrorKind::TimedOut,
                                "timed out waiting for the file to hydrate",
                            )),
                            s,
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                Err(_) => {
                    s.done = true;
                    return Some((
                        Err(stream_err(
                            std::io::ErrorKind::Other,
                            "read failed while streaming the body",
                        )),
                        s,
                    ));
                }
            }
        }
    });

    let mut res = warp::reply::Response::new(warp::hyper::Body::wrap_stream(body));
    let h = res.headers_mut();
    h.insert(
        "content-type",
        warp::http::HeaderValue::from_static("application/octet-stream"),
    );
    h.insert("accept-ranges", warp::http::HeaderValue::from_static("bytes"));
    // Committed from the opening stat, like the buffered path. A body
    // that cannot deliver exactly this many bytes errors out.
    h.insert(
        "content-length",
        warp::http::HeaderValue::from_str(&want.to_string()).unwrap(),
    );
    if let Ok(v) = warp::http::HeaderValue::from_str(&etag) {
        h.insert("etag", v);
    }
    if partial {
        h.insert(
            "content-range",
            warp::http::HeaderValue::from_str(&format!("bytes {start}-{end}/{}", entry.size))
                .unwrap(),
        );
        *res.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    res
}

/// Reserved prefix for in-progress uploads. Named so a crashed upload is
/// recognisable — and so the tier's own reserved names are not shadowed.
const UPLOAD_TMP_PREFIX: &str = ".flint-upload.";

/// Makes each upload's temp name unique WITHIN this process.
///
/// The pid alone is not enough and the bug it hid was real: two
/// concurrent PUTs to the same path from the same hub derived the SAME
/// temp name, wrote into one file interleaved, and each renamed it over
/// the target. The result is a file holding a mix of both bodies,
/// reported to both callers as 201 Created. One hub serves every
/// request for a share, so "same process" is the common case, not the
/// exotic one — and a UI that retries a slow upload is enough to
/// trigger it.
static UPLOAD_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The temp name one upload writes into before renaming it over the
/// target. pid AND a per-process counter: the pid keeps a crashed
/// upload attributable to an incarnation, the counter is what actually
/// makes two concurrent uploads to one path distinct.
fn upload_tmp_name(leaf: &str) -> String {
    format!(
        "{UPLOAD_TMP_PREFIX}{leaf}.{}.{}",
        std::process::id(),
        UPLOAD_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

async fn handle_upload(
    fs: Arc<HubFs>,
    q: PathQuery,
    body: Bytes,
    if_match: Option<String>,
    if_none_match: Option<String>,
) -> warp::reply::Response {
    let path = match FsPath::parse(&q.path) {
        Ok(p) => p,
        Err(e) => return err_reply(&e),
    };
    let Some((parent, leaf)) = path.split_leaf() else {
        return plain(StatusCode::BAD_REQUEST, "path names the export root, not a file");
    };

    // ── preconditions (RFC 9110 §13.1) ──────────────────────────────
    let expect = match if_match.as_deref().map(write_precondition).transpose() {
        Ok(p) => p,
        Err(m) => return plain(StatusCode::BAD_REQUEST, &m),
    };
    let must_be_absent = match if_none_match.as_deref() {
        None => false,
        Some(raw) => match parse_validators(raw) {
            Ok(v) if v.any => true,
            // `If-None-Match: "<tag>"` on a write asks "unless it is
            // still exactly this", which no caller of this API has a
            // use for and which would need its own compound arm.
            Ok(_) => {
                return plain(
                    StatusCode::BAD_REQUEST,
                    "If-None-Match on a write is supported only as `*` (create if absent)",
                )
            }
            Err(m) => return plain(StatusCode::BAD_REQUEST, &m),
        },
    };
    if must_be_absent && expect.is_some() {
        return plain(
            StatusCode::BAD_REQUEST,
            "If-Match and If-None-Match: * cannot both hold on one request",
        );
    }

    // The create-if-absent arm is a check with a window, and says so.
    // NFS has no operation that fails a compound BECAUSE a name
    // resolved, so unlike If-Match this one cannot ride along with the
    // rename. Two callers racing a create can therefore both pass here
    // and one will silently win. It is offered because it makes the
    // common single-writer case correct and the alternative is offering
    // nothing; do not describe it as a guarantee.
    if must_be_absent {
        match fs.stat(&path).await {
            Ok(_) => {
                return plain(
                    StatusCode::PRECONDITION_FAILED,
                    "the file already exists and If-None-Match: * was sent",
                )
            }
            Err(FsError::Nfs(Nfs4Status::NoEnt)) => {}
            Err(e) => return err_reply(&e),
        }
    }

    // Fail fast on a tag that already disagrees: the rename's VERIFY is
    // the authority, but there is no reason to write a whole temp file
    // for a body that cannot land.
    if let Some(p) = expect {
        match fs.stat(&path).await {
            Ok(e) if !precondition_holds(p, &e) => return precondition_failed(&path.display()),
            Err(FsError::Nfs(Nfs4Status::NoEnt)) => return precondition_failed(&path.display()),
            // Anything else — including a hydration DELAY — is the
            // compound's to judge, not this shortcut's.
            _ => {}
        }
    }

    // Write to a temp name and RENAME over the target.
    //
    // A concurrent reader then sees either the old file or the new one
    // and never a half-written mixture, and a crashed upload leaves a
    // recognisable temp rather than a corrupt file under the real name.
    // It also sidesteps the evicted-file truncate: replacing a stub
    // through a fresh inode never asks the tier to hydrate bytes that
    // are about to be discarded.
    //
    // The tier handles the rename correctly on its own account — the
    // rename-over tombstones the covered generation, so the old S3
    // object is retired rather than orphaned.
    let mut tmp = parent.clone();
    let tmp_name = upload_tmp_name(&leaf);
    tmp.push_component(tmp_name);

    let stateid = match fs.create_open(&tmp).await {
        Ok(s) => s,
        Err(e) => return err_reply(&e),
    };

    let mut offset = 0u64;
    for chunk in body.chunks(CHUNK) {
        let want = chunk.len() as u32;
        match fs
            .write_at(&tmp, stateid, offset, Bytes::copy_from_slice(chunk))
            .await
        {
            Ok(n) if n == want => offset += n as u64,
            Ok(n) => {
                // A short write reported as success is how an upload
                // silently loses its tail. Refuse, and take the temp
                // with us so no partial file survives under any name.
                let _ = fs.remove(&tmp).await;
                return plain(
                    StatusCode::INSUFFICIENT_STORAGE,
                    &format!("short write ({n} of {want} bytes); upload abandoned"),
                );
            }
            Err(e) => {
                let _ = fs.remove(&tmp).await;
                return err_reply(&e);
            }
        }
    }

    if let Err(e) = fs.commit_and_close(&tmp, stateid).await {
        let _ = fs.remove(&tmp).await;
        return err_reply(&e);
    }
    // The swap, with the caller's condition evaluated INSIDE the same
    // compound: VERIFY then RENAME, and a compound stops at its first
    // error, so a file that moved under the caller is never replaced.
    if let Err(e) = fs.rename_checked(&tmp, &path, None, expect).await {
        let _ = fs.remove(&tmp).await;
        return mutate_err_reply(&e, expect.is_some());
    }

    // Hand back the version that now exists, so a caller writing twice
    // in a row does not have to re-read between them.
    let mut res = note(StatusCode::CREATED, "written");
    if let Ok(e) = fs.stat(&path).await {
        if let Ok(v) = warp::http::HeaderValue::from_str(&e.etag) {
            res.headers_mut().insert("etag", v);
        }
    }
    res
}

async fn handle_delete(
    fs: Arc<HubFs>,
    q: PathQuery,
    if_match: Option<String>,
) -> warp::reply::Response {
    let path = match FsPath::parse(&q.path) {
        Ok(p) => p,
        Err(e) => return err_reply(&e),
    };
    let expect = match if_match.as_deref().map(write_precondition).transpose() {
        Ok(p) => p,
        Err(m) => return plain(StatusCode::BAD_REQUEST, &m),
    };
    match fs.remove_checked(&path, expect).await {
        Ok(()) => note(StatusCode::OK, "removed"),
        Err(e) => mutate_err_reply(&e, expect.is_some()),
    }
}

async fn handle_folder(fs: Arc<HubFs>, b: FolderBody) -> warp::reply::Response {
    let path = match FsPath::parse(&b.path) {
        Ok(p) => p,
        Err(e) => return err_reply(&e),
    };
    match fs.mkdir(&path).await {
        Ok(()) => note(StatusCode::CREATED, "created"),
        Err(e) => err_reply(&e),
    }
}

async fn handle_move(
    fs: Arc<HubFs>,
    b: MoveBody,
    if_match: Option<String>,
) -> warp::reply::Response {
    let from = match FsPath::parse(&b.from) {
        Ok(p) => p,
        Err(e) => return err_reply(&e),
    };
    let to = match FsPath::parse(&b.to) {
        Ok(p) => p,
        Err(e) => return err_reply(&e),
    };
    // A move conditions the object being MOVED. The destination is not
    // conditioned: RENAME replaces it by definition, and a caller that
    // wants to protect it should be uploading, not moving.
    let expect = match if_match.as_deref().map(write_precondition).transpose() {
        Ok(p) => p,
        Err(m) => return plain(StatusCode::BAD_REQUEST, &m),
    };
    match fs.rename_checked(&from, &to, expect, None).await {
        Ok(()) => note(StatusCode::OK, "moved"),
        Err(e) => mutate_err_reply(&e, expect.is_some()),
    }
}

/// Render a rejection this filter raised.
///
/// Body-shape rejections (a malformed JSON body, a missing query
/// parameter, an over-length upload) are rendered too: warp's default
/// for those is also a bare status with no body, and a caller debugging
/// an integration deserves to be told which field it got wrong.
pub async fn recover(err: warp::Rejection) -> Result<warp::reply::Response, warp::Rejection> {
    if err.find::<Unauthorized>().is_some() {
        return Ok(plain(StatusCode::UNAUTHORIZED, "bearer token required"));
    }
    if let Some(NotReady(phase)) = err.find::<NotReady>() {
        let mut res = plain(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("hub is not serving yet (phase: {phase:?}); poll /status"),
        );
        res.headers_mut()
            .insert("retry-after", warp::http::HeaderValue::from_static("5"));
        return Ok(res);
    }
    if let Some(e) = err.find::<warp::filters::body::BodyDeserializeError>() {
        return Ok(plain(StatusCode::BAD_REQUEST, &format!("malformed body: {e}")));
    }
    if err.find::<warp::reject::PayloadTooLarge>().is_some() {
        return Ok(plain(
            StatusCode::PAYLOAD_TOO_LARGE,
            "upload exceeds the configured size cap",
        ));
    }
    if let Some(e) = err.find::<warp::reject::InvalidQuery>() {
        return Ok(plain(StatusCode::BAD_REQUEST, &format!("bad query: {e}")));
    }
    Err(err)
}

#[cfg(test)]
mod tests {

    // ---------------------------------------------------------------
    // Streaming downloads. Buffering the whole body bounded the hub's
    // memory by the DOWNLOAD CAP, which defaults to 5 GiB; with hubs
    // 1:1 with projects that is a fleet-wide cost. These pin both the
    // memory split and the properties buffering used to be the only
    // way to get.
    // ---------------------------------------------------------------

    /// The threshold is a boundary, not an approximation. Exactly at it
    /// buffers; one byte over streams. Pinned separately from the
    /// behavioural tests because the two paths differ in what they can
    /// promise, so which one runs is itself part of the contract.
    #[test]
    fn the_stream_threshold_is_an_exact_boundary() {
        let cfg = ApiConfig::default();
        let t = cfg.stream_threshold_bytes;
        assert_eq!(t, 8 * 1024 * 1024, "the shipped default moved — intended?");

        assert!(!streams_rather_than_buffers(t, &cfg), "AT the threshold must buffer");
        assert!(streams_rather_than_buffers(t + 1, &cfg), "one byte OVER must stream");
        assert!(!streams_rather_than_buffers(0, &cfg), "an empty body must never stream");

        // The boundary must track the CONFIGURED value, not the default.
        // Without this leg the whole test passes on a hardcoded 8 MiB.
        let small = ApiConfig { stream_threshold_bytes: 1024, ..ApiConfig::default() };
        assert!(!streams_rather_than_buffers(1024, &small), "AT a lowered threshold must buffer");
        assert!(streams_rather_than_buffers(1025, &small), "over a lowered threshold must stream");
        assert!(
            streams_rather_than_buffers(8 * 1024 * 1024, &small),
            "the old default must stream once the threshold is lowered under it"
        );
    }

    // ---------------------------------------------------------------
    // The cold-read guard. A download's terminal check refuses when
    // `change` moved under the read — that is how a rename-over is
    // caught. But the tier rewrites the local inode when it HYDRATES,
    // so before this the hub's own hydration tripped its own guard and
    // every first read of every stub after a DR wake answered 409.
    // Measured on a real cluster: 13 of 13 files, every one of them 200
    // on the immediate retry.
    // ---------------------------------------------------------------

    /// Success is not an error, and must not be published under an
    /// `error` key. Every mutating route used to answer through the
    /// error body — `{"error":"created"}` on a 201 — which reads as a
    /// failure to any client scanning for that field.
    #[tokio::test]
    async fn a_successful_mutation_does_not_answer_under_an_error_key() {
        let (api, _fs, _t) = harness().await;

        let cases = vec![
            (
                warp::test::request()
                    .method("POST")
                    .path("/files/folder")
                    .header("authorization", bearer())
                    .json(&serde_json::json!({"path": "/d"}))
                    .reply(&api)
                    .await,
                "created",
            ),
            (
                warp::test::request()
                    .method("PUT")
                    .path("/files/content?path=/d/f.bin")
                    .header("authorization", bearer())
                    .body("hello")
                    .reply(&api)
                    .await,
                "written",
            ),
            (
                warp::test::request()
                    .method("POST")
                    .path("/files/move")
                    .header("authorization", bearer())
                    .json(&serde_json::json!({"from": "/d/f.bin", "to": "/d/g.bin"}))
                    .reply(&api)
                    .await,
                "moved",
            ),
            (
                warp::test::request()
                    .method("DELETE")
                    .path("/files/content?path=/d/g.bin")
                    .header("authorization", bearer())
                    .reply(&api)
                    .await,
                "removed",
            ),
        ];

        for (res, expected) in cases {
            assert!(res.status().is_success(), "expected a 2xx, got {}", res.status());
            let v: serde_json::Value = serde_json::from_slice(res.body().as_ref()).expect("a JSON body");
            assert!(
                v.get("error").is_none(),
                "a successful response carried an `error` key: {v}"
            );
            assert_eq!(
                v.get("status").and_then(|s| s.as_str()),
                Some(expected),
                "success must be reported under `status`"
            );
        }
    }

    /// A hydration keeps the inode and the logical size and moves only
    /// `change`. That is the case the download must absorb rather than
    /// refuse.
    #[test]
    fn a_hydration_that_only_moves_change_is_benign() {
        assert!(
            hydration_is_benign((42, 4096), (42, 4096)),
            "same inode, same logical size — only `change` moved, which is what \
             hydrating a stub does; refusing this is the 409-on-every-cold-read bug"
        );
    }

    /// The guard must not have been widened into "never refuse". A
    /// rename-over lands a DIFFERENT inode at the same path, and a
    /// caller handed those bytes under the old file's validator has
    /// been given the wrong object.
    #[test]
    fn a_replacement_inode_is_never_benign() {
        assert!(
            !hydration_is_benign((42, 4096), (43, 4096)),
            "a different fileid at the same path is a rename-over, not a hydration"
        );
    }

    /// Same inode, different logical size: a truncate or an append,
    /// against a response that has already committed a Content-Length
    /// built from the first stat.
    #[test]
    fn a_resize_under_the_read_is_never_benign() {
        assert!(
            !hydration_is_benign((42, 4096), (42, 8192)),
            "the file grew under the read — the committed Content-Length is now a lie"
        );
        assert!(
            !hydration_is_benign((42, 4096), (42, 0)),
            "the file was truncated under the read"
        );
    }

    /// The re-baseline has to publish the tag of the bytes it actually
    /// shipped. Returning the pre-hydration tag would hand every caller
    /// a validator that is already stale, so their next `If-Match`
    /// would 412 for no reason they could see.
    #[tokio::test]
    async fn a_download_returns_the_tag_of_the_bytes_it_shipped() {
        let (api, fs, _t) = harness().await;
        let _ = seed(&fs, "/f.bin", 4096).await;

        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/f.bin")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 200);
        let served = res.headers().get("etag").unwrap().to_str().unwrap().to_string();

        let now = fs.stat(&FsPath::parse("/f.bin").unwrap()).await.unwrap();
        assert_eq!(
            served,
            hubfs::render_etag(now.fileid, now.change),
            "the ETag on the response must name the file as it stands after the read"
        );
    }

    async fn seed(fs: &Arc<HubFs>, name: &str, len: usize) -> Vec<u8> {
        let body: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let p = FsPath::parse(name).unwrap();
        let sid = fs.create_open(&p).await.unwrap();
        let mut off = 0u64;
        for c in body.chunks(256 * 1024) {
            let n = fs.write_at(&p, sid, off, Bytes::copy_from_slice(c)).await.unwrap();
            assert_eq!(n as usize, c.len());
            off += c.len() as u64;
        }
        fs.commit_and_close(&p, sid).await.unwrap();
        body
    }

    fn tiny_threshold() -> ApiConfig {
        ApiConfig {
            token: Some(TokenSource::fixed(TOKEN)),
            stream_threshold_bytes: 64 * 1024,
            ..Default::default()
        }
    }

    /// The whole point: a body far above the threshold still arrives
    /// byte-for-byte, with the length it promised.
    #[tokio::test]
    async fn a_streamed_download_is_byte_identical() {
        let (api, fs, _t) = harness_with(tiny_threshold(), None).await;
        let body = seed(&fs, "/big.bin", 1024 * 1024).await;

        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/big.bin")
            .header("authorization", bearer())
            .reply(&api)
            .await;

        assert_eq!(res.status(), 200);
        assert_eq!(
            res.headers()["content-length"],
            body.len().to_string().as_str(),
            "Content-Length must be committed from the opening stat"
        );
        assert_eq!(res.body().as_ref(), body.as_slice(), "streamed body must be identical");
    }

    /// Regression: the buffered path is untouched for small bodies, and
    /// it is still the one that runs.
    #[tokio::test]
    async fn a_small_download_still_buffers_and_is_identical() {
        let (api, fs, _t) = harness_with(tiny_threshold(), None).await;
        let body = seed(&fs, "/small.bin", 4096).await;

        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/small.bin")
            .header("authorization", bearer())
            .reply(&api)
            .await;

        assert_eq!(res.status(), 200);
        assert_eq!(res.body().as_ref(), body.as_slice());
    }

    /// An empty file must not take the streaming path — `want` is 0 and
    /// a zero-length stream that then re-stats would be pure cost.
    #[tokio::test]
    async fn an_empty_file_downloads_as_an_empty_body() {
        let (api, fs, _t) = harness_with(tiny_threshold(), None).await;
        let _ = seed(&fs, "/empty.bin", 0).await;

        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/empty.bin")
            .header("authorization", bearer())
            .reply(&api)
            .await;

        assert_eq!(res.status(), 200);
        assert!(res.body().is_empty());
    }

    /// Range on a streamed file: 206, the right slice, and a
    /// `Content-Range` naming the FULL size.
    #[tokio::test]
    async fn a_range_on_a_streamed_file_returns_the_exact_slice() {
        let (api, fs, _t) = harness_with(tiny_threshold(), None).await;
        let body = seed(&fs, "/big.bin", 1024 * 1024).await;

        // Big enough to stream, offset so an off-by-one shows up.
        let (start, end) = (12_345u64, 12_345 + 200_000u64);
        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/big.bin")
            .header("authorization", bearer())
            .header("range", format!("bytes={start}-{end}"))
            .reply(&api)
            .await;

        assert_eq!(res.status(), 206);
        assert_eq!(
            res.headers()["content-range"],
            format!("bytes {start}-{end}/{}", body.len()).as_str()
        );
        assert_eq!(
            res.body().as_ref(),
            &body[start as usize..=end as usize],
            "the streamed slice must match exactly"
        );
    }

    /// The operational lever this makes real: the threshold and the cap
    /// are both checked against the RANGE, not the file. A small range
    /// of a huge file is served by the BUFFERED path, so it keeps the
    /// clean pre-byte 409 — and costs the range, not the file.
    #[tokio::test]
    async fn a_small_range_of_a_large_file_is_buffered() {
        let (api, fs, _t) = harness_with(tiny_threshold(), None).await;
        let body = seed(&fs, "/big.bin", 1024 * 1024).await;

        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/big.bin")
            .header("authorization", bearer())
            .header("range", "bytes=0-1023")
            .reply(&api)
            .await;

        assert_eq!(res.status(), 206);
        assert_eq!(res.body().len(), 1024);
        assert_eq!(res.body().as_ref(), &body[0..1024]);
    }

    /// Regression: the download cap still refuses BEFORE anything is
    /// read, and streaming does not quietly make it unreachable.
    #[tokio::test]
    async fn the_download_cap_still_refuses_before_streaming() {
        let cfg = ApiConfig {
            token: Some(TokenSource::fixed(TOKEN)),
            stream_threshold_bytes: 1024,
            max_download_bytes: 4096,
            ..Default::default()
        };
        let (api, fs, _t) = harness_with(cfg, None).await;
        let _ = seed(&fs, "/big.bin", 64 * 1024).await;

        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/big.bin")
            .header("authorization", bearer())
            .reply(&api)
            .await;

        assert_eq!(res.status(), 413, "the cap outranks the streaming path");
    }

    /// THE PROPERTY BUFFERING USED TO BE THE ONLY WAY TO GET.
    ///
    /// A streamed body cannot answer 409 once the status line has
    /// shipped, so a file that is replaced mid-read must make the
    /// STREAM FAIL. A short body under a 200 is a silently corrupt file
    /// on the caller's disk, and that is the outcome this forbids.
    ///
    /// Driven deterministically: `.filter()` hands back the Response
    /// without collecting the body, so the file can be mutated between
    /// two chunks.
    #[tokio::test]
    async fn a_streamed_download_whose_file_changes_fails_the_stream() {
        use futures::StreamExt;

        let (api, fs, _t) = harness_with(tiny_threshold(), None).await;
        let _ = seed(&fs, "/big.bin", 1024 * 1024).await;

        let res: warp::reply::Response = warp::test::request()
            .method("GET")
            .path("/files/content?path=/big.bin")
            .header("authorization", bearer())
            .filter(&api)
            .await
            .expect("the route must answer");
        assert_eq!(res.status(), 200);

        let mut body = res.into_body();

        // One chunk out of the door — the status line is now committed.
        let first = body.next().await.expect("a first chunk").expect("it must be Ok");
        assert!(!first.is_empty());

        // Now replace the file underneath the reader.
        let p = FsPath::parse("/big.bin").unwrap();
        let sid = fs.create_open(&p).await.unwrap();
        fs.write_at(&p, sid, 0, Bytes::from_static(b"replaced")).await.unwrap();
        fs.commit_and_close(&p, sid).await.unwrap();

        // Drain. The stream MUST end in an error rather than simply
        // stopping: stopping early under a committed Content-Length is
        // exactly the silent truncation this guards.
        let mut errored = false;
        let mut got = first.len();
        while let Some(item) = body.next().await {
            match item {
                Ok(b) => got += b.len(),
                Err(_) => {
                    errored = true;
                    break;
                }
            }
        }
        assert!(
            errored,
            "the stream ended cleanly after {got} of 1048576 bytes — a truncated \
             download reported as success"
        );
    }

    /// Two concurrent PUTs to ONE path must not share a temp file.
    ///
    /// They did: the name was keyed on the process id alone, and one
    /// hub serves every request for a share, so "same process" is the
    /// ordinary case rather than the exotic one. Both uploads opened
    /// the same temp, wrote into it interleaved, and each renamed it
    /// over the target — leaving a file holding a mix of two bodies and
    /// reporting 201 Created to both callers. A UI retrying a slow
    /// upload is enough to trigger it.
    #[test]
    fn concurrent_uploads_to_one_path_get_distinct_temp_files() {
        let names: std::collections::HashSet<String> =
            (0..64).map(|_| upload_tmp_name("report.pdf")).collect();
        assert_eq!(names.len(), 64, "temp names collided: {names:?}");

        // Still recognisable as ours, so startup reaping keeps working.
        for n in &names {
            assert!(n.starts_with(UPLOAD_TMP_PREFIX), "{n} lost the reserved prefix");
            assert!(n.contains("report.pdf"), "{n} lost the leaf name");
        }

        // Distinct leaves stay distinct too.
        assert_ne!(
            upload_tmp_name("a").split('.').nth(2),
            None,
            "the name shape changed; startup reaping and this guard both key on it"
        );
    }
    use super::*;
    use crate::nfs::v4::filehandle::FileHandleManager;
    use crate::nfs::v4::state::StateManager;
    use crate::nfs::v4::CompoundDispatcher;
    use crate::nfs::v4::operations::lockops::LockManager;
    use tempfile::TempDir;

    const TOKEN: &str = "test-token";

    /// Build a hub the way the server does, INCLUDING the pre-listener
    /// `load_from_backend`. That call is what ends the grace period on a
    /// hub with nothing to reclaim — skip it and every OPEN answers
    /// NFS4ERR_GRACE for ninety seconds, which is precisely the bug
    /// these tests exist to keep fixed.
    async fn harness() -> (
        impl Filter<Extract = (warp::reply::Response,), Error = warp::Rejection> + Clone,
        Arc<HubFs>,
        TempDir,
    ) {
        harness_with(ApiConfig { token: Some(TokenSource::fixed(TOKEN)), ..Default::default() }, None).await
    }

    async fn harness_with(
        cfg: ApiConfig,
        dir: Option<TempDir>,
    ) -> (
        impl Filter<Extract = (warp::reply::Response,), Error = warp::Rejection> + Clone,
        Arc<HubFs>,
        TempDir,
    ) {
        let temp = dir.unwrap_or_else(|| TempDir::new().unwrap());
        let fh_mgr = Arc::new(FileHandleManager::new(temp.path().to_path_buf()));
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        state_mgr.load_from_backend().await.unwrap();
        let lock_mgr = Arc::new(LockManager::new());
        let dispatcher = Arc::new(CompoundDispatcher::new(fh_mgr, state_mgr, lock_mgr));
        let fs = Arc::new(HubFs::new(dispatcher));
        (routes(fs.clone(), cfg), fs, temp)
    }

    fn bearer() -> String {
        format!("Bearer {TOKEN}")
    }

    /// An unauthenticated read-write file API on a project volume is not
    /// a degraded mode, it is a breach — so every verb refuses without
    /// the token, including the read-only ones.
    #[tokio::test]
    async fn every_endpoint_refuses_without_the_token() {
        let (api, _fs, _t) = harness().await;

        for (method, path) in [
            ("GET", "/files?path=/"),
            ("GET", "/files/content?path=/a"),
            ("DELETE", "/files/content?path=/a"),
        ] {
            let res = warp::test::request()
                .method(method)
                .path(path)
                .reply(&api)
                .await;
            assert_eq!(res.status(), 401, "{method} {path} served without a token");
        }

        // A wrong token is refused as firmly as a missing one.
        let res = warp::test::request()
            .method("GET")
            .path("/files?path=/")
            .header("authorization", "Bearer wrong")
            .reply(&api)
            .await;
        assert_eq!(res.status(), 401);
    }

    /// Rotating the projected Secret takes effect on the NEXT REQUEST,
    /// against a router that was never rebuilt.
    ///
    /// This is the whole point of `token::TokenSource`. The token used
    /// to be captured at boot, so a rotation reached a running hub only
    /// through a pod restart — and restarting a hub stalls every mounted
    /// client on that share until the new pod answers. Paying an NFS
    /// availability event to change an HTTP credential coupled the two
    /// doors; this test is what keeps them apart.
    #[tokio::test]
    async fn a_rotated_token_takes_effect_without_a_restart() {
        let dir = TempDir::new().unwrap();
        let token_path = dir.path().join("token");
        std::fs::write(&token_path, "first").unwrap();

        let source = TokenSource::new("first", Some(token_path.clone()));
        let (api, _fs, _t) = harness_with(
            ApiConfig { token: Some(Arc::clone(&source)), ..Default::default() },
            None,
        )
        .await;

        let get = |tok: &'static str| {
            let api = api.clone();
            async move {
                warp::test::request()
                    .method("GET")
                    .path("/files?path=/")
                    .header("authorization", format!("Bearer {tok}"))
                    .reply(&api)
                    .await
                    .status()
            }
        };

        assert_eq!(get("first").await, 200, "the boot token must work");
        assert_eq!(get("second").await, 401, "the future token must not work yet");

        // What the kubelet does when the Secret is edited.
        std::fs::write(&token_path, "second").unwrap();
        assert!(source.refresh(), "the refresher must see the rotation");

        // Same router, same process, no restart.
        assert_eq!(get("second").await, 200, "the rotated token must be accepted");
        assert_eq!(
            get("first").await,
            401,
            "the old token must stop working — otherwise a rotation revokes nothing"
        );
    }

    /// The whole surface, exercised as a caller would: create a folder,
    /// upload into it, list it, download it back byte-identical, move
    /// it, then delete it.
    #[tokio::test]
    async fn the_six_endpoints_round_trip() {
        let (api, _fs, temp) = harness().await;
        let body = b"the quick brown fox".to_vec();

        // POST /files/folder
        let res = warp::test::request()
            .method("POST")
            .path("/files/folder")
            .header("authorization", bearer())
            .json(&serde_json::json!({"path": "/project"}))
            .reply(&api)
            .await;
        assert_eq!(res.status(), 201, "{:?}", String::from_utf8_lossy(res.body()));
        assert!(temp.path().join("project").is_dir());

        // PUT /files/content
        let res = warp::test::request()
            .method("PUT")
            .path("/files/content?path=/project/notes.txt")
            .header("authorization", bearer())
            .body(body.clone())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 201, "{:?}", String::from_utf8_lossy(res.body()));
        assert_eq!(std::fs::read(temp.path().join("project/notes.txt")).unwrap(), body);

        // The temp file the upload wrote through must not survive it.
        let leftovers: Vec<_> = std::fs::read_dir(temp.path().join("project"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".flint-upload."))
            .collect();
        assert!(leftovers.is_empty(), "upload temp survived: {leftovers:?}");

        // GET /files
        let res = warp::test::request()
            .method("GET")
            .path("/files?path=/project")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 200);
        let listing: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        let entries = listing["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "{listing}");
        assert_eq!(entries[0]["name"], "notes.txt");
        assert_eq!(entries[0]["type"], "file");
        assert_eq!(entries[0]["size"], body.len());

        // GET /files/content
        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/project/notes.txt")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 200);
        assert_eq!(res.body().as_ref(), body.as_slice(), "download must be byte-identical");

        // POST /files/move
        let res = warp::test::request()
            .method("POST")
            .path("/files/move")
            .header("authorization", bearer())
            .json(&serde_json::json!({"from": "/project/notes.txt", "to": "/project/renamed.txt"}))
            .reply(&api)
            .await;
        assert_eq!(res.status(), 200, "{:?}", String::from_utf8_lossy(res.body()));
        assert!(temp.path().join("project/renamed.txt").exists());
        assert!(!temp.path().join("project/notes.txt").exists());

        // DELETE /files/content
        let res = warp::test::request()
            .method("DELETE")
            .path("/files/content?path=/project/renamed.txt")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 200);
        assert!(!temp.path().join("project/renamed.txt").exists());
    }

    /// Path traversal is refused at the API boundary, using the
    /// server's own component validator — so the API cannot even
    /// express a name the protocol would reject, and the dispatcher's
    /// containment check stays a backstop rather than the only guard.
    #[tokio::test]
    async fn traversal_is_refused_before_it_reaches_the_dispatcher() {
        let (api, _fs, temp) = harness().await;
        let secret = temp.path().parent().unwrap().join("flint-api-escape-target");
        std::fs::write(&secret, b"secret").unwrap();

        for p in [
            "/../flint-api-escape-target",
            "/a/../../flint-api-escape-target",
            "/%2e%2e/x",
        ] {
            let res = warp::test::request()
                .method("GET")
                .path(&format!("/files/content?path={p}"))
                .header("authorization", bearer())
                .reply(&api)
                .await;
            assert!(
                res.status() == 400 || res.status() == 404,
                "traversal {p} answered {}",
                res.status()
            );
            assert_ne!(res.body().as_ref(), b"secret");
        }
        let _ = std::fs::remove_file(&secret);
    }

    /// A symlink is DATA on this surface, never a thing to follow. The
    /// server refuses to dereference it (see `nfs::v4::open_beneath`)
    /// and the API says so with 409 instead of leaking the target or
    /// reporting a confusing 500.
    #[tokio::test]
    async fn a_symlink_is_never_followed_and_answers_409() {
        let (api, _fs, temp) = harness().await;
        let outside = temp.path().parent().unwrap().join("flint-api-symlink-target");
        std::fs::write(&outside, b"credentials").unwrap();
        std::os::unix::fs::symlink(&outside, temp.path().join("link.txt")).unwrap();

        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/link.txt")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 409, "{:?}", String::from_utf8_lossy(res.body()));
        assert!(!res.body().as_ref().windows(11).any(|w| w == b"credentials"));

        // But it is VISIBLE in a listing, with its target carried as
        // data — a browser must be able to show the link exists.
        let res = warp::test::request()
            .method("GET")
            .path("/files?path=/")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        let listing: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        let link = listing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "link.txt")
            .expect("the link must be listed");
        assert_eq!(link["type"], "symlink");
        let _ = std::fs::remove_file(&outside);
    }

    /// A directory is not a file. Downloading one must say so rather
    /// than serving something surprising, and deleting a non-empty one
    /// must refuse rather than recursing.
    #[tokio::test]
    async fn directories_are_not_files_and_non_empty_ones_are_not_deleted() {
        let (api, _fs, temp) = harness().await;
        std::fs::create_dir(temp.path().join("d")).unwrap();
        std::fs::write(temp.path().join("d/keep.txt"), b"x").unwrap();

        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/d")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 409);

        let res = warp::test::request()
            .method("DELETE")
            .path("/files/content?path=/d")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 409, "a non-empty directory must not be deleted");
        assert!(temp.path().join("d/keep.txt").exists(), "and its contents must survive");
    }

    #[tokio::test]
    async fn a_missing_file_is_404_not_500() {
        let (api, _fs, _t) = harness().await;
        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/nope.txt")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 404);
    }

    /// Paging must resume, not restart: an offset-based cursor would
    /// re-walk the directory each page and can silently skip or repeat
    /// entries when it changes underneath. Every file must appear
    /// exactly once across the pages.
    #[tokio::test]
    async fn cursor_pagination_visits_every_entry_exactly_once() {
        let (api, _fs, temp) = harness().await;
        for i in 0..25 {
            std::fs::write(temp.path().join(format!("f{i:02}.bin")), b"x").unwrap();
        }

        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..40 {
            let path = match &cursor {
                Some(c) => format!("/files?path=/&limit=7&cursor={c}"),
                None => "/files?path=/&limit=7".to_string(),
            };
            let res = warp::test::request()
                .method("GET")
                .path(&path)
                .header("authorization", bearer())
                .reply(&api)
                .await;
            assert_eq!(res.status(), 200);
            let doc: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
            for e in doc["entries"].as_array().unwrap() {
                seen.push(e["name"].as_str().unwrap().to_string());
            }
            match doc["nextCursor"].as_str() {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        }

        seen.sort();
        let unique: std::collections::BTreeSet<_> = seen.iter().cloned().collect();
        assert_eq!(unique.len(), 25, "every entry exactly once; saw {seen:?}");
        assert_eq!(seen.len(), 25, "no entry repeated across pages");
    }

    /// A recursive listing has no NFS equivalent — it is the hub
    /// issuing N READDIRs on the caller's behalf — so it is bounded,
    /// and when it stops early it SAYS so. A short list that looks
    /// complete is worse than an explicit truncation.
    #[tokio::test]
    async fn a_recursive_listing_is_bounded_and_admits_it() {
        let (api, _fs, temp) = harness().await;
        std::fs::create_dir_all(temp.path().join("a/b/c")).unwrap();
        std::fs::write(temp.path().join("a/one.txt"), b"1").unwrap();
        std::fs::write(temp.path().join("a/b/two.txt"), b"2").unwrap();
        std::fs::write(temp.path().join("a/b/c/three.txt"), b"3").unwrap();

        let res = warp::test::request()
            .method("GET")
            .path("/files?path=/a&recursive=true")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 200);
        let doc: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        let names: Vec<&str> = doc["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        for want in ["one.txt", "two.txt", "three.txt", "b", "c"] {
            assert!(names.contains(&want), "recursive walk missed {want}: {names:?}");
        }
        assert_eq!(doc["truncated"], false);

        // Bounded: ask for fewer than exist and be told it was cut.
        let res = warp::test::request()
            .method("GET")
            .path("/files?path=/a&recursive=true&limit=2")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        let doc: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        assert_eq!(doc["entries"].as_array().unwrap().len(), 2);
        assert_eq!(doc["truncated"], true, "a truncated walk must say so");
    }

    /// Upload replaces through a temp + RENAME, so a reader sees the old
    /// file or the new one and never a mixture — and a replace never
    /// leaves the target half-written.
    #[tokio::test]
    async fn upload_replaces_atomically() {
        let (api, _fs, temp) = harness().await;
        std::fs::write(temp.path().join("x.bin"), b"old contents here").unwrap();

        let res = warp::test::request()
            .method("PUT")
            .path("/files/content?path=/x.bin")
            .header("authorization", bearer())
            .body(b"new".to_vec())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 201);
        assert_eq!(std::fs::read(temp.path().join("x.bin")).unwrap(), b"new");
    }

    /// Range requests: the download reports a byte range honestly, with
    /// 206 and a Content-Range, so a caller can fetch a large file in
    /// pieces instead of tripping the single-request cap.
    #[tokio::test]
    async fn range_requests_serve_partial_content() {
        let (api, _fs, temp) = harness().await;
        std::fs::write(temp.path().join("r.bin"), b"0123456789").unwrap();

        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/r.bin")
            .header("authorization", bearer())
            .header("range", "bytes=2-5")
            .reply(&api)
            .await;
        assert_eq!(res.status(), 206);
        assert_eq!(res.body().as_ref(), b"2345");
        assert_eq!(res.headers()["content-range"], "bytes 2-5/10");

        // Suffix form.
        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/r.bin")
            .header("authorization", bearer())
            .header("range", "bytes=-3")
            .reply(&api)
            .await;
        assert_eq!(res.status(), 206);
        assert_eq!(res.body().as_ref(), b"789");
    }

    /// The download cap is a real gate: a browse click can otherwise
    /// pull an arbitrarily large file out of S3, which is billed egress
    /// a UI triggered by accident.
    #[tokio::test]
    async fn the_download_cap_refuses_rather_than_spending_egress() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("big.bin"), vec![7u8; 4096]).unwrap();
        let (api, _fs, _t) = harness_with(
            ApiConfig {
                token: Some(TokenSource::fixed(TOKEN)),
                max_download_bytes: 1024,
                ..Default::default()
            },
            Some(temp),
        )
        .await;

        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/big.bin")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 413);

        // And a Range within the cap still serves, which is what makes
        // the cap a speed bump rather than a wall.
        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/big.bin")
            .header("authorization", bearer())
            .header("range", "bytes=0-1023")
            .reply(&api)
            .await;
        assert_eq!(res.status(), 206);
        assert_eq!(res.body().len(), 1024);
    }

    /// A hub with nothing to reclaim must not sit in the grace period.
    ///
    /// Grace protects a client's chance to reclaim state it held before
    /// a restart. A hub woken from HIBERNATION comes back on a fresh
    /// PVC with an empty state database, so no such client exists — and
    /// yet it would spend ninety seconds answering NFS4ERR_GRACE to
    /// every OPEN. Reads need no OPEN, so they serve throughout, which
    /// makes the symptom maddening from outside: the project browses
    /// perfectly and refuses every save.
    #[tokio::test]
    async fn a_hub_with_no_state_to_reclaim_accepts_writes_immediately() {
        let (api, _fs, temp) = harness().await;

        let res = warp::test::request()
            .method("PUT")
            .path("/files/content?path=/first.txt")
            .header("authorization", bearer())
            .body(b"written the instant the hub came up".to_vec())
            .reply(&api)
            .await;
        assert_eq!(
            res.status(),
            201,
            "a freshly woken hub must accept writes at once: {:?}",
            String::from_utf8_lossy(res.body())
        );
        assert!(temp.path().join("first.txt").exists());
    }

    /// The other half of the same contract: while a hub genuinely IS in
    /// grace — state was loaded, so somebody could still reclaim — a
    /// write is refused with 503 and a Retry-After long enough to be
    /// worth honouring, not a bare 500.
    #[tokio::test]
    async fn a_real_grace_period_answers_503_with_a_useful_retry_after() {
        let temp = TempDir::new().unwrap();
        let fh_mgr = Arc::new(FileHandleManager::new(temp.path().to_path_buf()));
        // No load_from_backend: this hub does not know whether anything
        // is reclaimable, so it stays in grace, exactly as a hub with
        // surviving client records does.
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        assert!(state_mgr.leases.in_grace_period());
        let lock_mgr = Arc::new(LockManager::new());
        let dispatcher = Arc::new(CompoundDispatcher::new(fh_mgr, state_mgr, lock_mgr));
        let api = routes(
            Arc::new(HubFs::new(dispatcher)),
            ApiConfig { token: Some(TokenSource::fixed(TOKEN)), ..Default::default() },
        );

        let res = warp::test::request()
            .method("PUT")
            .path("/files/content?path=/x.txt")
            .header("authorization", bearer())
            .body(b"nope".to_vec())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 503);
        assert_eq!(res.headers()["retry-after"], "5");

        // And reads still serve during grace — they take the anonymous
        // stateid and need no OPEN.
        std::fs::write(temp.path().join("readable.txt"), b"yes").unwrap();
        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/readable.txt")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 200);
        assert_eq!(res.body().as_ref(), b"yes");
    }

    /// The API shares a listener with `/status`, which binds before the
    /// tier and before the NFS listener so that a slow epoch claim or a
    /// DR import is observable. During that window the namespace is
    /// still being rebuilt from the bucket, so a listing would show a
    /// partial tree as though it were whole and a write would race the
    /// import placing files. Both must refuse.
    #[tokio::test]
    async fn the_api_refuses_until_the_hub_is_actually_serving() {
        use crate::pnfs::mds::status::{HubPhase, HubStatus};

        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("a.txt"), b"x").unwrap();
        let fh_mgr = Arc::new(FileHandleManager::new(temp.path().to_path_buf()));
        let state_mgr = Arc::new(StateManager::new_in_memory(""));
        state_mgr.load_from_backend().await.unwrap();
        let lock_mgr = Arc::new(LockManager::new());
        let dispatcher = Arc::new(CompoundDispatcher::new(fh_mgr, state_mgr, lock_mgr));
        let status = Arc::new(HubStatus::new());
        let api = routes_gated(
            Arc::new(HubFs::new(dispatcher)),
            ApiConfig { token: Some(TokenSource::fixed(TOKEN)), ..Default::default() },
            status.clone(),
        );

        // Every pre-listener phase refuses, including the read paths —
        // a partial tree is worse than an honest 503.
        for phase in [
            HubPhase::Starting,
            HubPhase::ClaimingEpoch,
            HubPhase::Importing,
            HubPhase::Reconciling,
            HubPhase::Draining,
        ] {
            status.set_phase(phase);
            let res = warp::test::request()
                .method("GET")
                .path("/files?path=/")
                .header("authorization", bearer())
                .reply(&api)
                .await;
            assert_eq!(res.status(), 503, "phase {phase:?} must not serve files");
            assert_eq!(res.headers()["retry-after"], "5");
        }

        // Serving and Sweeping both serve: by Sweeping the listener is
        // up and the tree is whole — only foreign keys are still being
        // folded in.
        for phase in [HubPhase::Serving, HubPhase::Sweeping] {
            status.set_phase(phase);
            let res = warp::test::request()
                .method("GET")
                .path("/files?path=/")
                .header("authorization", bearer())
                .reply(&api)
                .await;
            assert_eq!(res.status(), 200, "phase {phase:?} must serve");
        }

        // The readiness gate is not a substitute for auth: an
        // unauthenticated call to a ready hub is still 401.
        status.set_phase(HubPhase::Serving);
        let res = warp::test::request()
            .method("GET")
            .path("/files?path=/")
            .reply(&api)
            .await;
        assert_eq!(res.status(), 401);
    }

    /// Browsing is real user intent and must keep the project awake;
    /// this is the signal the idle ladder suspends on. Pinned here
    /// because the coupling is invisible: it works only because every
    /// call goes through the compound dispatcher, which notes activity
    /// on its own.
    #[tokio::test]
    async fn api_calls_count_as_activity() {
        use crate::nfs::activity;
        let (api, _fs, temp) = harness().await;
        std::fs::write(temp.path().join("a.txt"), b"x").unwrap();

        let before = activity::snapshot();
        let res = warp::test::request()
            .method("GET")
            .path("/files?path=/")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(res.status(), 200);
        let after = activity::snapshot();
        assert!(
            after.browse_ops > before.browse_ops,
            "a listing must register as browse activity, or a project a user is \
             looking at would be suspended under them"
        );
    }

    // ── conditional requests ─────────────────────────────────────────

    /// Upload `body` to `path` with optional preconditions; return the
    /// response.
    async fn put(
        api: &(impl Filter<Extract = (warp::reply::Response,), Error = warp::Rejection> + Clone
                  + 'static),
        path: &str,
        body: &[u8],
        if_match: Option<&str>,
        if_none_match: Option<&str>,
    ) -> warp::http::Response<bytes::Bytes> {
        let mut req = warp::test::request()
            .method("PUT")
            .path(&format!("/files/content?path={path}"))
            .header("authorization", bearer());
        if let Some(v) = if_match {
            req = req.header("if-match", v);
        }
        if let Some(v) = if_none_match {
            req = req.header("if-none-match", v);
        }
        req.body(body.to_vec()).reply(api).await
    }

    fn etag_of(res: &warp::http::Response<bytes::Bytes>) -> String {
        res.headers()
            .get("etag")
            .unwrap_or_else(|| panic!("no ETag on {:?}", res.status()))
            .to_str()
            .unwrap()
            .to_string()
    }

    /// The whole point: a caller that reads, edits and writes back can
    /// find out that someone else wrote in between, instead of silently
    /// discarding their work.
    ///
    /// Before this, two PUTs to one path both answered 201 and one was
    /// dropped with nothing anywhere recording it — the lost-update half
    /// of the bug whose interleaving half was fixed by giving each
    /// upload its own temp name.
    #[tokio::test]
    async fn a_stale_entity_tag_refuses_the_write_instead_of_losing_it() {
        let (api, _fs, temp) = harness().await;

        let first = put(&api, "/doc.txt", b"one", None, None).await;
        assert_eq!(first.status(), 201);
        let stale = etag_of(&first);

        // Somebody else writes. Their tag is fresh, ours is not.
        let second = put(&api, "/doc.txt", b"two", Some(&stale), None).await;
        assert_eq!(second.status(), 201, "the holder of the current tag must be able to write");
        let fresh = etag_of(&second);
        assert_ne!(stale, fresh, "a write that changed the file must change its tag");

        // Now the first caller writes back what it had. It must not win.
        let third = put(&api, "/doc.txt", b"three", Some(&stale), None).await;
        assert_eq!(
            third.status(),
            412,
            "a stale tag wrote anyway: {:?}",
            String::from_utf8_lossy(third.body())
        );
        assert_eq!(
            std::fs::read(temp.path().join("doc.txt")).unwrap(),
            b"two".to_vec(),
            "the refused write landed anyway"
        );
    }

    /// A refused conditional upload must not leave its temp behind.
    ///
    /// Every other failure path in the upload deletes the temp; the
    /// precondition path is a new one, and a leaked `.flint-upload.*`
    /// is both a visible file in the user's project and a file the tier
    /// would eventually publish to the bucket.
    #[tokio::test]
    async fn a_refused_conditional_upload_leaves_no_temp_behind() {
        let (api, _fs, temp) = harness().await;
        assert_eq!(put(&api, "/doc.txt", b"one", None, None).await.status(), 201);

        // A tag for an object that is not there any more.
        let bogus = hubfs::render_etag(999_999, 12_345);
        let res = put(&api, "/doc.txt", b"two", Some(&bogus), None).await;
        assert_eq!(res.status(), 412);

        let leftovers: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(UPLOAD_TMP_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "temp survived a refused upload: {leftovers:?}");
    }

    /// `If-Match` against a file that does not exist is a FAILED
    /// CONDITION, not a missing resource. Answering 404 would tell a
    /// caller its file vanished when someone actually replaced it.
    #[tokio::test]
    async fn if_match_on_an_absent_file_is_412_not_404() {
        let (api, _fs, _t) = harness().await;
        let tag = hubfs::render_etag(1, 1);
        assert_eq!(put(&api, "/nope.txt", b"x", Some(&tag), None).await.status(), 412);

        // Without a precondition the same path is an ordinary create.
        assert_eq!(put(&api, "/nope.txt", b"x", None, None).await.status(), 201);
    }

    /// `If-None-Match: *` is create-if-absent.
    #[tokio::test]
    async fn if_none_match_star_creates_only_when_absent() {
        let (api, _fs, temp) = harness().await;

        assert_eq!(put(&api, "/new.txt", b"one", None, Some("*")).await.status(), 201);
        let res = put(&api, "/new.txt", b"two", None, Some("*")).await;
        assert_eq!(res.status(), 412, "created over an existing file");
        assert_eq!(std::fs::read(temp.path().join("new.txt")).unwrap(), b"one".to_vec());
    }

    /// A conditional GET revalidates without moving bytes — which on an
    /// evicted file is the difference between a 304 and billed S3 egress.
    #[tokio::test]
    async fn a_conditional_get_revalidates_with_304() {
        let (api, _fs, _t) = harness().await;
        assert_eq!(put(&api, "/doc.txt", b"hello", None, None).await.status(), 201);

        let got = warp::test::request()
            .method("GET")
            .path("/files/content?path=/doc.txt")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(got.status(), 200);
        let tag = etag_of(&got);
        assert_eq!(got.body().as_ref(), b"hello");

        let again = warp::test::request()
            .method("GET")
            .path("/files/content?path=/doc.txt")
            .header("authorization", bearer())
            .header("if-none-match", tag.clone())
            .reply(&api)
            .await;
        assert_eq!(again.status(), 304);
        assert!(again.body().is_empty(), "a 304 must carry no body");
        assert_eq!(etag_of(&again), tag, "a 304 must still name the version");

        // After a write the same tag no longer matches, so the caller
        // gets the new bytes rather than a stale 304.
        assert_eq!(put(&api, "/doc.txt", b"world!", None, None).await.status(), 201);
        let third = warp::test::request()
            .method("GET")
            .path("/files/content?path=/doc.txt")
            .header("authorization", bearer())
            .header("if-none-match", tag)
            .reply(&api)
            .await;
        assert_eq!(third.status(), 200);
        assert_eq!(third.body().as_ref(), b"world!");
    }

    /// A listing's tag is the same tag a download issues, so a UI can
    /// browse and then write conditionally without re-reading each file.
    #[tokio::test]
    async fn a_listing_carries_the_tag_a_download_would_issue() {
        let (api, _fs, _t) = harness().await;
        assert_eq!(put(&api, "/doc.txt", b"hello", None, None).await.status(), 201);

        let list = warp::test::request()
            .method("GET")
            .path("/files?path=/")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(list.status(), 200);
        let doc: serde_json::Value = serde_json::from_slice(list.body()).unwrap();
        let from_listing = doc["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "doc.txt")
            .expect("doc.txt missing from the listing")["etag"]
            .as_str()
            .unwrap()
            .to_string();

        let got = warp::test::request()
            .method("GET")
            .path("/files/content?path=/doc.txt")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(from_listing, etag_of(&got));

        // And it is good enough to write with.
        assert_eq!(
            put(&api, "/doc.txt", b"edited", Some(&from_listing), None).await.status(),
            201
        );
    }

    /// An entity-tag this server never issued is a client bug. Answering
    /// 412 would let it retry forever without ever learning that.
    #[tokio::test]
    async fn a_forged_entity_tag_is_a_400() {
        let (api, _fs, _t) = harness().await;
        assert_eq!(put(&api, "/doc.txt", b"one", None, None).await.status(), 201);

        for bad in ["\"nonsense\"", "nonsense", "\"zz-zz\""] {
            let res = put(&api, "/doc.txt", b"two", Some(bad), None).await;
            assert_eq!(res.status(), 400, "{bad} was not refused as malformed");
        }

        // Weak validators are refused on writes: If-Match is defined on
        // strong comparison, and weak means "equivalent", not "the same
        // bytes".
        let tag = etag_of(&put(&api, "/w.txt", b"one", None, None).await);
        let res = put(&api, "/w.txt", b"two", Some(&format!("W/{tag}")), None).await;
        assert_eq!(res.status(), 400);
    }

    /// DELETE and move condition too, or the contract is half a contract:
    /// a UI could protect an edit and still lose the file to a stale
    /// delete issued from another tab.
    #[tokio::test]
    async fn delete_and_move_honour_if_match() {
        let (api, _fs, temp) = harness().await;

        let first = put(&api, "/doc.txt", b"one", None, None).await;
        let stale = etag_of(&first);
        let fresh = etag_of(&put(&api, "/doc.txt", b"two", Some(&stale), None).await);

        let del = warp::test::request()
            .method("DELETE")
            .path("/files/content?path=/doc.txt")
            .header("authorization", bearer())
            .header("if-match", stale.clone())
            .reply(&api)
            .await;
        assert_eq!(del.status(), 412, "a stale tag deleted the file");
        assert!(temp.path().join("doc.txt").exists());

        let mv = warp::test::request()
            .method("POST")
            .path("/files/move")
            .header("authorization", bearer())
            .header("if-match", stale)
            .json(&serde_json::json!({"from": "/doc.txt", "to": "/moved.txt"}))
            .reply(&api)
            .await;
        assert_eq!(mv.status(), 412, "a stale tag moved the file");
        assert!(temp.path().join("doc.txt").exists());

        // The current tag works for both.
        let mv = warp::test::request()
            .method("POST")
            .path("/files/move")
            .header("authorization", bearer())
            .header("if-match", fresh)
            .json(&serde_json::json!({"from": "/doc.txt", "to": "/moved.txt"}))
            .reply(&api)
            .await;
        assert_eq!(mv.status(), 200, "{:?}", String::from_utf8_lossy(mv.body()));
        assert!(temp.path().join("moved.txt").exists());

        let tag = etag_of(
            &warp::test::request()
                .method("GET")
                .path("/files/content?path=/moved.txt")
                .header("authorization", bearer())
                .reply(&api)
                .await,
        );
        let del = warp::test::request()
            .method("DELETE")
            .path("/files/content?path=/moved.txt")
            .header("authorization", bearer())
            .header("if-match", tag)
            .reply(&api)
            .await;
        assert_eq!(del.status(), 200);
        assert!(!temp.path().join("moved.txt").exists());
    }

    /// Unconditional requests must behave exactly as they did before
    /// preconditions existed — every existing caller sends no headers.
    #[tokio::test]
    async fn requests_without_preconditions_are_unchanged() {
        let (api, _fs, temp) = harness().await;
        assert_eq!(put(&api, "/doc.txt", b"one", None, None).await.status(), 201);
        assert_eq!(put(&api, "/doc.txt", b"two", None, None).await.status(), 201);
        assert_eq!(std::fs::read(temp.path().join("doc.txt")).unwrap(), b"two".to_vec());

        let del = warp::test::request()
            .method("DELETE")
            .path("/files/content?path=/doc.txt")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(del.status(), 200);

        // A missing file with no precondition is still a 404, not a 412.
        let del = warp::test::request()
            .method("DELETE")
            .path("/files/content?path=/doc.txt")
            .header("authorization", bearer())
            .reply(&api)
            .await;
        assert_eq!(del.status(), 404);
    }

    // ── the guarantee, tested where it actually lives ────────────────
    //
    // Every test above drives HTTP, and the upload handler pre-checks a
    // precondition with a stat before it writes anything. That
    // shortcut would satisfy all of them ON ITS OWN — delete the VERIFY
    // from the mutating compound and they still pass, while the
    // guarantee they claim to prove is gone. These call the fs layer
    // directly so the compound is the only thing that can refuse.

    /// The condition rides INSIDE the compound that renames.
    #[tokio::test]
    async fn rename_checked_refuses_a_stale_tag_with_no_handler_involved() {
        let (_api, fs, temp) = harness().await;
        std::fs::write(temp.path().join("doc.txt"), b"one").unwrap();
        let doc = FsPath::parse("/doc.txt").unwrap();

        let before = fs.stat(&doc).await.unwrap();
        let stale = Precondition::Is { fileid: before.fileid, change: before.change };

        // Mutate it so the recorded tag goes stale.
        std::fs::write(temp.path().join("src.bin"), b"replacement").unwrap();
        let src = FsPath::parse("/src.bin").unwrap();
        fs.rename(&src, &doc).await.unwrap();

        // Now the swap the handler would perform, straight at the fs.
        std::fs::write(temp.path().join("src2.bin"), b"third").unwrap();
        let src2 = FsPath::parse("/src2.bin").unwrap();
        let err = fs.rename_checked(&src2, &doc, None, Some(stale)).await.unwrap_err();
        assert_eq!(
            err,
            FsError::Nfs(Nfs4Status::NotSame),
            "the VERIFY is not in the compound: a stale precondition renamed anyway"
        );
        assert_eq!(std::fs::read(temp.path().join("doc.txt")).unwrap(), b"replacement".to_vec());

        // The current tag still works, so the refusal was the condition
        // and not the mechanism being broken outright.
        let now = fs.stat(&doc).await.unwrap();
        fs.rename_checked(
            &src2,
            &doc,
            None,
            Some(Precondition::Is { fileid: now.fileid, change: now.change }),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(temp.path().join("doc.txt")).unwrap(), b"third".to_vec());
    }

    /// Same, for REMOVE.
    #[tokio::test]
    async fn remove_checked_refuses_a_stale_tag_with_no_handler_involved() {
        let (_api, fs, temp) = harness().await;
        std::fs::write(temp.path().join("doc.txt"), b"one").unwrap();
        let doc = FsPath::parse("/doc.txt").unwrap();

        let before = fs.stat(&doc).await.unwrap();
        std::fs::write(temp.path().join("src.bin"), b"two").unwrap();
        fs.rename(&FsPath::parse("/src.bin").unwrap(), &doc).await.unwrap();

        let err = fs
            .remove_checked(
                &doc,
                Some(Precondition::Is { fileid: before.fileid, change: before.change }),
            )
            .await
            .unwrap_err();
        assert_eq!(err, FsError::Nfs(Nfs4Status::NotSame));
        assert!(temp.path().join("doc.txt").exists(), "a stale tag deleted the file");

        let now = fs.stat(&doc).await.unwrap();
        fs.remove_checked(
            &doc,
            Some(Precondition::Is { fileid: now.fileid, change: now.change }),
        )
        .await
        .unwrap();
        assert!(!temp.path().join("doc.txt").exists());
    }

    /// BOTH halves of the entity-tag are verified.
    ///
    /// The fileid half is what catches a rename-over: the name is
    /// rebound to a different inode whose own change counter starts
    /// fresh and can hold any value, including the one the caller
    /// remembers. If only the change value were compared, that swap
    /// would pass. This also pins the VERIFY payload's wire shape —
    /// bitmap, length, and CHANGE-before-FILEID ordering — because a
    /// malformed blob would fail the matching case too.
    #[tokio::test]
    async fn the_tag_verifies_identity_as_well_as_content() {
        let (_api, fs, temp) = harness().await;
        std::fs::write(temp.path().join("doc.txt"), b"one").unwrap();
        let doc = FsPath::parse("/doc.txt").unwrap();
        let e = fs.stat(&doc).await.unwrap();

        for (fileid, change, why) in [
            (e.fileid ^ 1, e.change, "a different inode passed the check"),
            (e.fileid, e.change ^ 1, "a different change value passed the check"),
            (e.fileid ^ 1, e.change ^ 1, "a wholly different tag passed the check"),
        ] {
            let err = fs
                .remove_checked(&doc, Some(Precondition::Is { fileid, change }))
                .await
                .unwrap_err();
            assert_eq!(err, FsError::Nfs(Nfs4Status::NotSame), "{why}");
        }

        // And the true pair passes, so the refusals above are the
        // comparison working rather than the blob never matching.
        fs.remove_checked(
            &doc,
            Some(Precondition::Is { fileid: e.fileid, change: e.change }),
        )
        .await
        .unwrap();
        assert!(!temp.path().join("doc.txt").exists());
    }

    /// `Precondition::Exists` is carried by the LOOKUP that resolves the
    /// name, so a compound with it never reaches its mutation when the
    /// object is gone.
    #[tokio::test]
    async fn exists_precondition_refuses_when_the_object_is_gone() {
        let (_api, fs, temp) = harness().await;
        std::fs::write(temp.path().join("src.bin"), b"x").unwrap();
        let src = FsPath::parse("/src.bin").unwrap();
        let absent = FsPath::parse("/absent.txt").unwrap();

        let err = fs
            .rename_checked(&src, &absent, None, Some(Precondition::Exists))
            .await
            .unwrap_err();
        assert_eq!(err, FsError::Nfs(Nfs4Status::NoEnt));
        assert!(temp.path().join("src.bin").exists(), "the source moved anyway");

        // Present ⇒ the same call goes through.
        std::fs::write(temp.path().join("there.txt"), b"y").unwrap();
        let there = FsPath::parse("/there.txt").unwrap();
        fs.rename_checked(&src, &there, None, Some(Precondition::Exists)).await.unwrap();
        assert_eq!(std::fs::read(temp.path().join("there.txt")).unwrap(), b"x".to_vec());
    }

    // ── the rest of the HTTP surface ─────────────────────────────────

    /// `If-Match: *` is "must exist", which is a different question from
    /// "must be this version".
    #[tokio::test]
    async fn if_match_star_means_the_file_must_exist() {
        let (api, _fs, temp) = harness().await;
        assert_eq!(put(&api, "/doc.txt", b"one", None, None).await.status(), 201);

        assert_eq!(
            put(&api, "/doc.txt", b"two", Some("*"), None).await.status(),
            201,
            "If-Match: * refused an existing file"
        );
        assert_eq!(std::fs::read(temp.path().join("doc.txt")).unwrap(), b"two".to_vec());

        assert_eq!(
            put(&api, "/gone.txt", b"x", Some("*"), None).await.status(),
            412,
            "If-Match: * created a file that did not exist"
        );

        let del = warp::test::request()
            .method("DELETE")
            .path("/files/content?path=/gone.txt")
            .header("authorization", bearer())
            .header("if-match", "*")
            .reply(&api)
            .await;
        assert_eq!(del.status(), 412);
    }

    /// A tag list is legal HTTP. Evaluating one honestly needs a
    /// compound per tag, so it is refused rather than silently reduced
    /// to its first element — which would report a check that never ran.
    #[tokio::test]
    async fn a_multi_tag_if_match_is_refused_rather_than_half_honoured() {
        let (api, _fs, _t) = harness().await;
        let tag = etag_of(&put(&api, "/doc.txt", b"one", None, None).await);
        let other = hubfs::render_etag(7, 7);

        let res = put(&api, "/doc.txt", b"two", Some(&format!("{tag}, {other}")), None).await;
        assert_eq!(res.status(), 400, "{:?}", String::from_utf8_lossy(res.body()));
    }

    /// Revalidation accepts what a caching client actually sends: a
    /// list, and weak tags (weak comparison is correct for GET even
    /// though it is refused on a write).
    #[tokio::test]
    async fn a_conditional_get_accepts_tag_lists_and_weak_tags() {
        let (api, _fs, _t) = harness().await;
        let tag = etag_of(&put(&api, "/doc.txt", b"hello", None, None).await);

        for header in [
            format!("{}, {tag}", hubfs::render_etag(1, 1)),
            format!("W/{tag}"),
            "*".to_string(),
        ] {
            let res = warp::test::request()
                .method("GET")
                .path("/files/content?path=/doc.txt")
                .header("authorization", bearer())
                .header("if-none-match", header.clone())
                .reply(&api)
                .await;
            assert_eq!(res.status(), 304, "If-None-Match: {header} did not revalidate");
        }

        // A list that does not contain the current tag still serves.
        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/doc.txt")
            .header("authorization", bearer())
            .header(
                "if-none-match",
                format!("{}, {}", hubfs::render_etag(1, 1), hubfs::render_etag(2, 2)),
            )
            .reply(&api)
            .await;
        assert_eq!(res.status(), 200);
        assert_eq!(res.body().as_ref(), b"hello");
    }

    /// Contradictory preconditions are a caller bug, not a race to
    /// resolve at runtime.
    #[tokio::test]
    async fn contradictory_preconditions_are_refused() {
        let (api, _fs, _t) = harness().await;
        let tag = etag_of(&put(&api, "/doc.txt", b"one", None, None).await);

        let res = put(&api, "/doc.txt", b"two", Some(&tag), Some("*")).await;
        assert_eq!(res.status(), 400);

        // `If-None-Match: "<tag>"` on a write is not offered; it must
        // not be silently treated as `*`.
        let res = put(&api, "/doc.txt", b"two", None, Some(&tag)).await;
        assert_eq!(res.status(), 400, "a tagged If-None-Match was quietly accepted");
    }

    /// A ranged response names the version its bytes came from, or a
    /// caller assembling a file from pieces cannot tell that the file
    /// changed under it halfway through.
    #[tokio::test]
    async fn a_partial_response_carries_the_same_tag_as_the_whole() {
        let (api, _fs, _t) = harness().await;
        let whole = etag_of(&put(&api, "/doc.txt", b"0123456789", None, None).await);

        let res = warp::test::request()
            .method("GET")
            .path("/files/content?path=/doc.txt")
            .header("authorization", bearer())
            .header("range", "bytes=2-5")
            .reply(&api)
            .await;
        assert_eq!(res.status(), 206);
        assert_eq!(res.body().as_ref(), b"2345");
        assert_eq!(etag_of(&res), whole);
    }

    /// A tag must move when the file moves and hold still when it does
    /// not. A validator that churned on every read would 412 every
    /// conditional write; one that never moved would authorise a lost
    /// update.
    #[tokio::test]
    async fn the_tag_is_stable_across_reads_and_moves_on_a_write() {
        let (api, _fs, _t) = harness().await;
        assert_eq!(put(&api, "/doc.txt", b"one", None, None).await.status(), 201);

        let read_tag = || async {
            etag_of(
                &warp::test::request()
                    .method("GET")
                    .path("/files/content?path=/doc.txt")
                    .header("authorization", bearer())
                    .reply(&api)
                    .await,
            )
        };
        let first = read_tag().await;
        assert_eq!(first, read_tag().await, "the tag churned across two plain reads");
        assert_eq!(first, read_tag().await);

        assert_eq!(put(&api, "/doc.txt", b"two", None, None).await.status(), 201);
        assert_ne!(first, read_tag().await, "the tag survived a write that changed the file");
    }

    // ── the drill ────────────────────────────────────────────────────
    //
    // Unit tests prove the mechanism answers correctly when asked. A
    // drill proves it survives the thing it was built for: many callers
    // doing read-modify-write against one path at once, which is the
    // front door's ordinary case (two tabs, a retried upload, an agent).
    //
    // Leg 1 is the ANTI-VACUITY CONTROL and it runs first on purpose.
    // It shows the disease is reachable at these timings with these
    // task counts. Without it leg 2 proves nothing: a storm that never
    // actually races would "pass" against a completely broken guard.

    /// How many writers, and how many appends each. Sized to race
    /// reliably while keeping the drill inside a normal test run.
    const DRILL_WRITERS: usize = 8;
    const DRILL_ROUNDS: usize = 25;

    /// GET, retrying the 409 the server answers when the object moved
    /// under the read.
    ///
    /// That retry IS the caller contract, not a workaround: a reader
    /// racing a writer is told to come back rather than handed bytes
    /// and a validator that describe different files. The drill follows
    /// the contract it documents. `None` means the file is gone.
    async fn read_body(
        api: &(impl Filter<Extract = (warp::reply::Response,), Error = warp::Rejection> + Clone
                  + 'static),
        path: &str,
    ) -> Option<(Vec<u8>, String)> {
        for _ in 0..5_000 {
            let res = warp::test::request()
                .method("GET")
                .path(&format!("/files/content?path={path}"))
                .header("authorization", bearer())
                .reply(api)
                .await;
            match res.status().as_u16() {
                200 => {
                    let tag = res
                        .headers()
                        .get("etag")
                        .expect("a 200 must carry a validator")
                        .to_str()
                        .unwrap()
                        .to_string();
                    return Some((res.body().to_vec(), tag));
                }
                409 | 503 => continue,
                404 => return None,
                other => panic!("unexpected {other} from a read: {:?}",
                                String::from_utf8_lossy(res.body())),
            }
        }
        panic!("a read never settled in 5000 attempts")
    }

    /// LEG 1 — the control. Unconditional read-modify-write from many
    /// writers MUST lose data, or the drill is measuring nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn drill_leg1_unconditional_writers_lose_updates() {
        let (api, _fs, _t) = harness().await;
        assert_eq!(put(&api, "/log.bin", b"", None, None).await.status(), 201);

        let mut tasks = Vec::new();
        for _ in 0..DRILL_WRITERS {
            let api = api.clone();
            tasks.push(async move {
                for _ in 0..DRILL_ROUNDS {
                    let (body, _) = read_body(&api, "/log.bin").await.expect("file vanished");
                    let mut next = body;
                    next.push(b'x');
                    let _ = put(&api, "/log.bin", &next, None, None).await;
                }
            });
        }
        futures::future::join_all(tasks).await;

        let (final_body, _) = read_body(&api, "/log.bin").await.expect("file vanished");
        let total = DRILL_WRITERS * DRILL_ROUNDS;
        assert!(
            final_body.len() < total,
            "ANTI-VACUITY FAILURE: {} unconditional writers × {} appends lost nothing \
             ({} bytes survived of {}). The storm is not racing, so leg 2 would prove \
             nothing about If-Match. Raise DRILL_WRITERS/DRILL_ROUNDS before trusting it.",
            DRILL_WRITERS,
            DRILL_ROUNDS,
            final_body.len(),
            total
        );
    }

    /// LEG 2 — the cure, measured rather than assumed.
    ///
    /// The same storm, each writer following the documented contract:
    /// send `If-Match`, and on 412 re-read and retry rather than
    /// retrying the write.
    ///
    /// **This does NOT assert that nothing is lost, and that is the
    /// finding.** A COMPOUND is not atomic, so two writers can both
    /// pass their VERIFY before either lands its RENAME, and one update
    /// dies with both callers seeing 201. An earlier version of this
    /// leg asserted zero loss and failed — not because the guard is
    /// broken, but because the leg asserted a property the design
    /// explicitly does not provide. What `If-Match` buys is a window
    /// narrowed from a whole client round trip to one operation inside
    /// one compound. So the oracle is comparative: conditional writers
    /// must lose dramatically less than unconditional ones, measured
    /// side by side under identical contention.
    ///
    /// Closing the gap entirely needs an exclusion primitive this
    /// surface deliberately does not take (holding one across an API
    /// request would let a caller stall the mount).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn drill_leg2_conditional_writers_lose_far_less() {
        let (api, _fs, _t) = harness().await;
        let total = DRILL_WRITERS * DRILL_ROUNDS;

        let unconditional = storm(&api, "/plain.bin", false).await;
        let conditional = storm(&api, "/guarded.bin", true).await;

        let lost_plain = total - unconditional;
        let lost_guarded = total - conditional;
        println!(
            "drill leg 2: {total} appends — unconditional kept {unconditional} \
             (lost {lost_plain}), conditional kept {conditional} (lost {lost_guarded})"
        );

        assert!(
            lost_plain > 0,
            "the control lost nothing, so this comparison measures nothing"
        );
        assert!(
            lost_guarded < lost_plain,
            "conditional writes lost {lost_guarded} of {total}, unconditional lost \
             {lost_plain} — the guard bought nothing under contention"
        );
        // DIRECTIONAL ONLY, and the reason is the finding.
        //
        // Every fixed ratio tried here was flaky, because the benefit is
        // not a constant. Measured on Linux: idle machine, the control
        // loses 168-174 of 200 and the guard loses 32-66 (2.6x-5.4x);
        // under full-suite CPU load the control is unchanged but the
        // guard loses 90-102 (1.7x-1.9x).
        //
        // CPU contention widens the server-internal VERIFY→RENAME gap by
        // descheduling a task inside it — so the guard is weakest
        // exactly when concurrent writers are most likely, which is the
        // opposite of the comforting assumption. A threshold encoding
        // any single load's ratio is a lie the suite tells at 1-in-5.
        // What is stable, and what a caller can rely on, is the
        // direction.
        assert!(
            lost_guarded < lost_plain,
            "conditional writes lost {lost_guarded} of {total} against {lost_plain} \
             unconditional — the guard bought nothing"
        );
    }

    /// One storm. `conditional` writers send `If-Match` and honour 412
    /// by re-reading; the others just write. Returns the surviving
    /// length, which with append-only writers is the number of updates
    /// that were not lost.
    async fn storm(
        api: &(impl Filter<Extract = (warp::reply::Response,), Error = warp::Rejection> + Clone
                  + 'static),
        path: &str,
        conditional: bool,
    ) -> usize {
        assert_eq!(put(api, path, b"", None, None).await.status(), 201);

        // A ceiling, not an expectation: under contention a writer may
        // retry many times, but it must not spin forever.
        const MAX_ATTEMPTS: usize = 5_000;

        // Monotonic-length oracle, and it applies to the CONDITIONAL
        // arm only.
        //
        // Under `If-Match` the file's length can never go down: a writer
        // that read N bytes writes N+1 only if its VERIFY still names
        // the N-byte version. So a read issued after N bytes were
        // observed must see at least N — anything less means a write
        // went backwards.
        //
        // It does NOT check the body against its validator, whatever an
        // earlier message here claimed: that property is enforced by the
        // download's terminal change-check, which refuses a mid-read
        // change rather than returning a mismatched 200, and it is
        // pinned by `drill_leg4_a_tag_never_names_two_contents` and by
        // `a_streamed_download_whose_file_changes_fails_the_stream`.
        //
        // In the UNCONDITIONAL arm shrinking is not a bug, it is the
        // whole point: a writer that read a stale short body writes it
        // forward over a longer file, and that IS the lost update being
        // measured. Asserting monotonic length there fails on a correct
        // run, which is how this oracle first showed up — as a flaky
        // drill rather than as a finding.
        let high_water = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for w in 0..DRILL_WRITERS {
            let api = api.clone();
            let high_water = Arc::clone(&high_water);
            let path = path.to_string();
            tasks.push(async move {
                for r in 0..DRILL_ROUNDS {
                    let mut attempts = 0usize;
                    loop {
                        attempts += 1;
                        assert!(
                            attempts < MAX_ATTEMPTS,
                            "writer {w} round {r} never landed in {MAX_ATTEMPTS} attempts"
                        );
                        // SAMPLE BEFORE THE READ, publish after. The
                        // order is the whole correctness of this oracle.
                        //
                        // Publishing and comparing in one `fetch_max`
                        // after the read compares THIS read's body
                        // against a mark another task may have published
                        // in between — and a task can be descheduled
                        // between its body landing and its publish, so
                        // the two are not the same instant. That version
                        // failed ~1-in-8 under an 8-way contended VM
                        // (and ~1-in-4 inside a loaded full suite) on a
                        // correct server: the read had simply linearized
                        // earlier than the mark it was judged against.
                        //
                        // Sampled first, the bound is sound in the only
                        // direction it needs: a value published before
                        // this read was ISSUED describes a version the
                        // file already had, and an append-only file
                        // guarded by If-Match never shrinks, so this
                        // read must see at least that much. A publish
                        // landing between the load and the read only
                        // weakens the bound — it can never invent a
                        // failure.
                        let seen_before =
                            high_water.load(std::sync::atomic::Ordering::SeqCst);
                        let (body, tag) =
                            read_body(&api, &path).await.expect("file vanished");
                        assert!(
                            !conditional || body.len() >= seen_before,
                            "writer {w} round {r}: read {} bytes under tag {tag}, but {} \
                             were already visible BEFORE this read was issued — an \
                             If-Match-guarded append-only file must never shrink",
                            body.len(),
                            seen_before
                        );
                        high_water
                            .fetch_max(body.len(), std::sync::atomic::Ordering::SeqCst);
                        let mut next = body;
                        next.push(b'x');
                        let tag = conditional.then_some(tag);
                        let res = put(&api, &path, &next, tag.as_deref(), None).await;
                        match res.status().as_u16() {
                            201 => break,
                            // The contract: re-read, do not retry the
                            // body we already built.
                            412 | 503 | 409 => continue,
                            other => panic!(
                                "writer {w} round {r}: unexpected {other}: {:?}",
                                String::from_utf8_lossy(res.body())
                            ),
                        }
                    }
                }
            });
        }
        futures::future::join_all(tasks).await;

        let (final_body, _) = read_body(api, path).await.expect("file vanished");
        assert!(
            final_body.iter().all(|b| *b == b'x'),
            "the file holds bytes nobody wrote — a torn or interleaved swap"
        );
        final_body.len()
    }

    /// LEG 3 — exactly one winner.
    ///
    /// **Linux only, and the reason is worth recording.** On macOS this
    /// leg reports three to six winners out of eight, and it is the
    /// HARNESS, not the server: racing `remove_file` on one path there
    /// returns success to several callers. The identical drill on Linux
    /// yields exactly one winner every round, conditional and
    /// unconditional alike, which was confirmed before writing this
    /// gate. Left ungated it would be a permanently red test on the
    /// development machine, and a red test nobody can fix is a test
    /// somebody deletes. Many callers race to delete the file
    /// they just read. Conditional delete must let precisely one
    /// through per incarnation; the rest are 412 (someone else won) or
    /// 404 (they won and it is gone).
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn drill_leg3_conditional_delete_has_exactly_one_winner() {
        let (api, _fs, temp) = harness().await;

        for round in 0..12 {
            assert_eq!(put(&api, "/victim.bin", b"here", None, None).await.status(), 201);

            let mut tasks = Vec::new();
            for _ in 0..DRILL_WRITERS {
                let api = api.clone();
                tasks.push(async move {
                    let Some((_, tag)) = read_body(&api, "/victim.bin").await else {
                        return (String::from("gone"), 0u16);
                    };
                    let res = warp::test::request()
                        .method("DELETE")
                        .path("/files/content?path=/victim.bin")
                        .header("authorization", bearer())
                        .header("if-match", tag.clone())
                        .reply(&api)
                        .await;
                    (tag, res.status().as_u16())
                });
            }
            let seen = futures::future::join_all(tasks).await;
            let winners = seen.iter().filter(|(_, st)| *st == 200).count();
            assert_eq!(
                winners, 1,
                "round {round}: {winners} deletes succeeded, expected exactly 1 — saw {seen:?}"
            );
            assert!(!temp.path().join("victim.bin").exists());
        }
    }

    /// LEG 4 — no tag is ever reused for different content.
    ///
    /// The tag is what a caller stores and later conditions on. If two
    /// distinct contents could ever carry one tag, every guarantee
    /// above is void — a stale write would pass its VERIFY.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn drill_leg4_a_tag_never_names_two_contents() {
        use std::collections::HashMap;
        use std::sync::Mutex;

        let (api, _fs, _t) = harness().await;
        assert_eq!(put(&api, "/churn.bin", b"seed", None, None).await.status(), 201);
        let seen: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));

        let mut tasks = Vec::new();
        for w in 0..DRILL_WRITERS {
            let api = api.clone();
            let seen = Arc::clone(&seen);
            tasks.push(async move {
                for r in 0..DRILL_ROUNDS {
                    // A body unique to this (writer, round).
                    let body = format!("w{w}-r{r}").into_bytes();
                    loop {
                        let res = put(&api, "/churn.bin", &body, None, None).await;
                        match res.status().as_u16() {
                            201 => break,
                            503 => continue,
                            other => panic!("unexpected {other} from a write"),
                        }
                    }

                    // Read back; whatever we get, its tag must be
                    // consistent with its content everywhere it appears.
                    let Some((got, tag)) = read_body(&api, "/churn.bin").await else {
                        panic!("the file vanished; nobody deletes in this leg")
                    };
                    let mut map = seen.lock().unwrap();
                    if let Some(prev) = map.get(&tag) {
                        assert_eq!(
                            prev, &got,
                            "tag {tag} named two different contents — a stale \
                             conditional write would pass its VERIFY"
                        );
                    } else {
                        map.insert(tag, got);
                    }
                }
            });
        }
        futures::future::join_all(tasks).await;
        let distinct = seen.lock().unwrap().len();
        assert!(distinct > 1, "the file never changed identity; the drill did not churn");
    }


}
