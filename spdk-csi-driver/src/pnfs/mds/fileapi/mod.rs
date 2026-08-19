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
//! | PUT | `/files/content?path=` | OPEN(CREATE) + WRITE* + COMMIT + CLOSE + RENAME |
//! | DELETE | `/files/content?path=` | REMOVE |
//! | POST | `/files/folder` | CREATE(NF4DIR) |
//! | POST | `/files/move` | SAVEFH + RENAME |
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
//! **Keep the project awake by existing.** Every call here is real user
//! intent and counts as activity, which is correct for a person clicking
//! through files and fatal for a liveness poller — a UI refreshing on a
//! timer would pin every project in the fleet awake forever, and the
//! idle ladder would never fire. The front door polls `/status` for
//! liveness; `/status` is deliberately NOT activity.

pub mod hubfs;

use hubfs::{FsError, FsPath, HubFs};
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
    pub token: Option<String>,
    /// Largest single upload accepted, in bytes.
    pub max_upload_bytes: u64,
    /// Largest single download served in one request. A browse click can
    /// otherwise pull an arbitrarily large file out of S3 — real egress,
    /// billed, triggered by a UI.
    pub max_download_bytes: u64,
    /// How long a download waits for hydration before answering 503.
    pub hydrate_wait_secs: u64,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            token: None,
            max_upload_bytes: 5 * 1024 * 1024 * 1024,
            max_download_bytes: 5 * 1024 * 1024 * 1024,
            hydrate_wait_secs: 30,
        }
    }
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
            .then(move |_, q: PathQuery, range: Option<String>| {
                let fs = fs.clone();
                let cfg = cfg.clone();
                async move { handle_download(fs, cfg, q, range).await }
            })
    };

    let upload = {
        let fs = fs.clone();
        let cfg = cfg.clone();
        warp::path!("files" / "content")
            .and(warp::put())
            .and(auth.clone())
            .and(warp::query::<PathQuery>())
            .and(warp::body::content_length_limit(cfg.max_upload_bytes))
            .and(warp::body::bytes())
            .then(move |_, q: PathQuery, body: Bytes| {
                let fs = fs.clone();
                async move { handle_upload(fs, q, body).await }
            })
    };

    let delete = {
        let fs = fs.clone();
        warp::path!("files" / "content")
            .and(warp::delete())
            .and(auth.clone())
            .and(warp::query::<PathQuery>())
            .then(move |_, q: PathQuery| {
                let fs = fs.clone();
                async move { handle_delete(fs, q).await }
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
            .and(warp::body::json::<MoveBody>())
            .then(move |_, b: MoveBody| {
                let fs = fs.clone();
                async move { handle_move(fs, b).await }
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
    token: Option<String>,
) -> impl Filter<Extract = ((),), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("authorization").and_then(move |given: Option<String>| {
        let expected = token.clone();
        async move {
            let Some(expected) = expected else {
                // No token configured: the route table is not mounted in
                // that case, so this is unreachable — deny anyway rather
                // than depend on a caller elsewhere getting it right.
                return Err(warp::reject::custom(Unauthorized));
            };
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

async fn handle_download(
    fs: Arc<HubFs>,
    cfg: ApiConfig,
    q: PathQuery,
    range: Option<String>,
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

    // Read it all before answering. Buffering costs memory bounded by
    // the download cap, and it buys the property that matters: the
    // status code and Content-Length are decided when every byte is
    // already in hand, so a 200 can never be followed by a body that
    // stops early. Streaming while the file is concurrently evicted is
    // exactly how a caller ends up with a silently truncated file.
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

    let mut res = warp::reply::Response::new(buf.into());
    let h = res.headers_mut();
    h.insert(
        "content-type",
        warp::http::HeaderValue::from_static("application/octet-stream"),
    );
    h.insert("accept-ranges", warp::http::HeaderValue::from_static("bytes"));
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

/// Reserved prefix for in-progress uploads. Named so a crashed upload is
/// recognisable — and so the tier's own reserved names are not shadowed.
const UPLOAD_TMP_PREFIX: &str = ".flint-upload.";

async fn handle_upload(fs: Arc<HubFs>, q: PathQuery, body: Bytes) -> warp::reply::Response {
    let path = match FsPath::parse(&q.path) {
        Ok(p) => p,
        Err(e) => return err_reply(&e),
    };
    let Some((parent, leaf)) = path.split_leaf() else {
        return plain(StatusCode::BAD_REQUEST, "path names the export root, not a file");
    };

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
    let tmp_name = format!("{UPLOAD_TMP_PREFIX}{leaf}.{}", std::process::id());
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
    if let Err(e) = fs.rename(&tmp, &path).await {
        let _ = fs.remove(&tmp).await;
        return err_reply(&e);
    }

    plain(StatusCode::CREATED, "written")
}

async fn handle_delete(fs: Arc<HubFs>, q: PathQuery) -> warp::reply::Response {
    let path = match FsPath::parse(&q.path) {
        Ok(p) => p,
        Err(e) => return err_reply(&e),
    };
    match fs.remove(&path).await {
        Ok(()) => plain(StatusCode::OK, "removed"),
        Err(e) => err_reply(&e),
    }
}

async fn handle_folder(fs: Arc<HubFs>, b: FolderBody) -> warp::reply::Response {
    let path = match FsPath::parse(&b.path) {
        Ok(p) => p,
        Err(e) => return err_reply(&e),
    };
    match fs.mkdir(&path).await {
        Ok(()) => plain(StatusCode::CREATED, "created"),
        Err(e) => err_reply(&e),
    }
}

async fn handle_move(fs: Arc<HubFs>, b: MoveBody) -> warp::reply::Response {
    let from = match FsPath::parse(&b.from) {
        Ok(p) => p,
        Err(e) => return err_reply(&e),
    };
    let to = match FsPath::parse(&b.to) {
        Ok(p) => p,
        Err(e) => return err_reply(&e),
    };
    match fs.rename(&from, &to).await {
        Ok(()) => plain(StatusCode::OK, "moved"),
        Err(e) => err_reply(&e),
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
        harness_with(ApiConfig { token: Some(TOKEN.to_string()), ..Default::default() }, None).await
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
                token: Some(TOKEN.to_string()),
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
            ApiConfig { token: Some(TOKEN.to_string()), ..Default::default() },
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
            ApiConfig { token: Some(TOKEN.to_string()), ..Default::default() },
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
}
