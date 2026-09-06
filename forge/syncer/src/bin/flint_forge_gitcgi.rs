//! `flint-forge-gitcgi` — the git container's whole HTTP surface, in
//! one process: `git http-backend` spawned per request as a CGI child,
//! both directions streamed as they arrive, and the one LFS route
//! relayed to the syncer's listener. It replaces nginx + fcgiwrap
//! (A3 of docs/plans/flint-forge-simplification-2026-09-05.md).
//!
//! What the two processes it replaces cost, on the wire: fcgiwrap held
//! the CGI's output until the request ended unless a patch-specific
//! parameter was set, so `receive-pack`'s 5 s sideband keepalives
//! reached the client in one burst with the final report and the door
//! cut every push whose hook ran longer than its inactivity bound
//! (runbx, 2026-09-05: a 40 GiB push, 311 s into an 8-minute hook
//! wait); nginx's `client_body_timeout` and `send_timeout` defaulted to
//! 60 s beside backend bounds set to an hour; and fcgiwrap's four
//! workers queued a fifth request in silence until the door cut it.
//! None of those knobs exist here. This process has exactly the knobs
//! it declares:
//!
//!   - NO timeouts of its own. The door owns the client-facing
//!     inactivity bound; a `repack` behind a fetch or a clone as large
//!     as the repository legitimately runs for as long as it runs.
//!   - NO buffering. Every chunk the child writes is a chunk on the
//!     wire; every chunk the client sends is a write to the child's
//!     stdin. A push is a streamed pack of unknown length and
//!     `http-backend` reads to EOF when `CONTENT_LENGTH` is unset.
//!   - A concurrency ceiling that ANSWERS (503) rather than queues, so
//!     an overloaded repository is visible instead of silent.
//!   - A client that goes away kills its child: the child is owned by
//!     the response body, and dropping the body drops the child.
//!
//! THE TRUST BOUNDARY is unchanged: `REMOTE_USER` comes from the
//! door's `X-Remote-User`, which the door sets from a verified
//! TokenReview and builds from an allowlist. Anything that can reach
//! this port can set the header itself, which is why the operator
//! renders a NetworkPolicy admitting only the gateway's pods (design
//! §6). `GIT_PROJECT_ROOT` and `GIT_HTTP_EXPORT_ALL` are the
//! container's environment, inherited.
//!
//! Environment: `FLINT_FORGE_GIT_LISTEN` (default `0.0.0.0:8080`),
//! `FLINT_FORGE_LFS_UPSTREAM` (default `127.0.0.1:9848`),
//! `FLINT_FORGE_GIT_CONCURRENCY` (default 64), `FLINT_FORGE_GIT_BIN`
//! (default `git`).

use std::net::SocketAddr;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::Stream;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdout, Command};
use tokio_util::io::ReaderStream;

type Body = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

struct Config {
    git: String,
    lfs_upstream: String,
    slots: tokio::sync::Semaphore,
    concurrency: usize,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() {
    let listen = env_or("FLINT_FORGE_GIT_LISTEN", "0.0.0.0:8080");
    let concurrency: usize = env_or("FLINT_FORGE_GIT_CONCURRENCY", "64").parse().unwrap_or(64);
    let cfg = Arc::new(Config {
        git: env_or("FLINT_FORGE_GIT_BIN", "git"),
        lfs_upstream: env_or("FLINT_FORGE_LFS_UPSTREAM", "127.0.0.1:9848"),
        slots: tokio::sync::Semaphore::new(concurrency),
        concurrency,
    });
    let listener = match TcpListener::bind(&listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("flint-forge-gitcgi: cannot listen on {listen}: {e}");
            std::process::exit(2);
        }
    };
    eprintln!(
        "flint-forge-gitcgi: serving git http-backend on {listen} (concurrency {}, lfs -> {})",
        cfg.concurrency, cfg.lfs_upstream
    );
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("flint-forge-gitcgi: accept: {e}");
                continue;
            }
        };
        let cfg = cfg.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = hyper::service::service_fn(move |req| handle(req, cfg.clone(), peer));
            // HTTP/1.1 only, keep-alive on: git opens one connection
            // per request pair and the door does the same.
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .keep_alive(true)
                .serve_connection(io, svc)
                .await
            {
                // A client that hung up mid-transfer is the ordinary
                // case here, not an error worth a line per occurrence.
                let s = e.to_string();
                if !s.contains("connection closed") && !s.contains("broken pipe") {
                    eprintln!("flint-forge-gitcgi: {peer}: {s}");
                }
            }
        });
    }
}

fn text(status: StatusCode, msg: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Full::new(Bytes::from(msg.to_string())).map_err(|e| match e {}).boxed())
        .unwrap()
}

fn is_lfs(path: &str) -> bool {
    path.ends_with("/info/lfs/objects/batch") || path.ends_with("/info/lfs/objects/verify")
}

async fn handle(
    req: Request<Incoming>,
    cfg: Arc<Config>,
    peer: SocketAddr,
) -> Result<Response<Body>, std::convert::Infallible> {
    let path = req.uri().path().to_string();
    if is_lfs(&path) && req.method() == Method::POST {
        return Ok(lfs_proxy(req, &cfg).await);
    }
    Ok(cgi(req, &cfg, peer).await)
}

/// The CGI child's stdout as a response body. Dropping the body drops
/// the child (`kill_on_drop`), which is what a client hangup does.
struct CgiBody {
    out: ReaderStream<BufReader<ChildStdout>>,
    _child: Child,
}

impl Stream for CgiBody {
    type Item = Result<Frame<Bytes>, std::io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.out).poll_next(cx).map(|o| o.map(|r| r.map(Frame::data)))
    }
}

async fn cgi(req: Request<Incoming>, cfg: &Config, peer: SocketAddr) -> Response<Body> {
    // The ceiling answers; it never queues (X4).
    let _slot = match cfg.slots.try_acquire() {
        Ok(p) => p,
        Err(_) => {
            eprintln!(
                "flint-forge-gitcgi: {} concurrent git requests, refusing one from {peer}",
                cfg.concurrency
            );
            return text(StatusCode::SERVICE_UNAVAILABLE, "too many git requests in flight\n");
        }
    };
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().unwrap_or("").to_string();
    let req_uri = parts.uri.to_string();
    let get = |n: &str| {
        parts.headers.get(n).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
    };

    let mut cmd = Command::new(&cfg.git);
    cmd.arg("http-backend");
    // What CGI promises a script, from the request. `GIT_PROJECT_ROOT`
    // and `GIT_HTTP_EXPORT_ALL` come with the container's environment.
    cmd.env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .env("SERVER_SOFTWARE", "flint-forge-gitcgi")
        .env("SERVER_NAME", "flint-forge")
        .env("SERVER_PORT", "8080")
        .env("SCRIPT_NAME", "")
        .env("REQUEST_METHOD", parts.method.as_str())
        .env("REQUEST_URI", &req_uri)
        .env("PATH_INFO", &path)
        .env("QUERY_STRING", &query)
        .env("REMOTE_ADDR", peer.ip().to_string())
        .env("REMOTE_PORT", peer.port().to_string());
    // Never inherited, always from this request: a stale value in the
    // environment would be a principal nobody verified.
    cmd.env_remove("REMOTE_USER").env_remove("GIT_PROTOCOL").env_remove("CONTENT_LENGTH");
    if let Some(v) = get("content-type") {
        cmd.env("CONTENT_TYPE", v);
    } else {
        cmd.env_remove("CONTENT_TYPE");
    }
    // Absent for a chunked push: http-backend then reads to EOF.
    if let Some(v) = get("content-length") {
        cmd.env("CONTENT_LENGTH", v);
    }
    // THE TRUST BOUNDARY: the door's verdict, and nothing else.
    if let Some(v) = get("x-remote-user") {
        cmd.env("REMOTE_USER", v);
    }
    // Protocol v2, or every session is v0.
    if let Some(v) = get("git-protocol") {
        cmd.env("GIT_PROTOCOL", v);
    }
    // http-backend inflates a gzip'd request body itself.
    if let Some(v) = get("content-encoding") {
        cmd.env("HTTP_CONTENT_ENCODING", v);
    }
    if let Some(v) = get("accept-encoding") {
        cmd.env("HTTP_ACCEPT_ENCODING", v);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("flint-forge-gitcgi: cannot spawn {} http-backend: {e}", cfg.git);
            return text(StatusCode::INTERNAL_SERVER_ERROR, "git http-backend did not start\n");
        }
    };
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");

    // The request body streams into the child as it arrives; the
    // child's stdin closes when the body ends, which is how
    // http-backend sees EOF on a chunked push. A body that fails
    // mid-way (the client left) closes stdin early and the child
    // fails its own read, which is the right outcome.
    tokio::spawn(async move {
        let mut body = body;
        while let Some(frame) = body.frame().await {
            match frame {
                Ok(f) => {
                    if let Ok(data) = f.into_data() {
                        if stdin.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let _ = stdin.shutdown().await;
        drop(stdin);
    });

    // The CGI header block: lines up to the first empty one. `Status:`
    // is the response status (200 when absent); everything else is a
    // response header. The body is whatever follows, streamed.
    let mut out = BufReader::new(stdout);
    let mut builder = Response::builder();
    let mut status = StatusCode::OK;
    let mut line = Vec::new();
    loop {
        line.clear();
        match out.read_until(b'\n', &mut line).await {
            Ok(0) => {
                // EOF before the blank line: the child died or wrote
                // nothing. Its stderr already went to ours.
                return text(StatusCode::BAD_GATEWAY, "git http-backend ended before its headers\n");
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("flint-forge-gitcgi: reading http-backend headers: {e}");
                return text(StatusCode::BAD_GATEWAY, "git http-backend header read failed\n");
            }
        }
        let l = String::from_utf8_lossy(&line);
        let l = l.trim_end_matches(['\r', '\n']);
        if l.is_empty() {
            break;
        }
        let Some((name, value)) = l.split_once(':') else { continue };
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("status") {
            if let Some(code) = value.split_whitespace().next().and_then(|c| c.parse::<u16>().ok()) {
                status = StatusCode::from_u16(code).unwrap_or(StatusCode::OK);
            }
            continue;
        }
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            builder = builder.header(n, v);
        }
    }
    let body = CgiBody { out: ReaderStream::with_capacity(out, 64 * 1024), _child: child };
    builder
        .status(status)
        .body(StreamBody::new(body).boxed())
        .unwrap_or_else(|_| text(StatusCode::BAD_GATEWAY, "git http-backend headers were not valid HTTP\n"))
}

/// The LFS batch and verify calls are answered by the SYNCER, which
/// holds the bucket credentials this container does not. The body is
/// small JSON; the upstream answers with a Content-Length and closes.
async fn lfs_proxy(req: Request<Incoming>, cfg: &Config) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path();
    let leaf = if path.ends_with("/batch") { "batch" } else { "verify" };
    let get = |n: &str| parts.headers.get(n).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let body = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => return text(StatusCode::BAD_REQUEST, &format!("lfs request body: {e}\n")),
    };
    let mut up = match TcpStream::connect(&cfg.lfs_upstream).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("flint-forge-gitcgi: lfs upstream {}: {e}", cfg.lfs_upstream);
            return text(StatusCode::BAD_GATEWAY, "the repository's LFS endpoint is not reachable\n");
        }
    };
    let mut head = format!(
        "POST /lfs/objects/{leaf} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
        cfg.lfs_upstream,
        body.len()
    );
    for (h, env) in [
        ("content-type", "Content-Type"),
        ("x-remote-user", "X-Remote-User"),
        ("x-forge-lfs-verify", "X-Forge-Lfs-Verify"),
    ] {
        if let Some(v) = get(h) {
            head.push_str(&format!("{env}: {v}\r\n"));
        }
    }
    head.push_str("\r\n");
    let mut wire = head.into_bytes();
    wire.extend_from_slice(&body);
    if up.write_all(&wire).await.is_err() {
        return text(StatusCode::BAD_GATEWAY, "the repository's LFS endpoint closed early\n");
    }
    let mut raw = Vec::new();
    if let Err(e) = up.read_to_end(&mut raw).await {
        return text(StatusCode::BAD_GATEWAY, &format!("lfs upstream read: {e}\n"));
    }
    // A minimal HTTP/1.1 response parse: status line, headers, then the
    // body to EOF (the upstream closes).
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return text(StatusCode::BAD_GATEWAY, "lfs upstream answered without headers\n");
    };
    let (head, rest) = raw.split_at(split);
    let rest = &rest[4..];
    let head = String::from_utf8_lossy(head);
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .and_then(|c| StatusCode::from_u16(c).ok())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    for l in lines {
        let Some((n, v)) = l.split_once(':') else { continue };
        let n = n.trim();
        if n.eq_ignore_ascii_case("connection")
            || n.eq_ignore_ascii_case("content-length")
            || n.eq_ignore_ascii_case("transfer-encoding")
        {
            continue;
        }
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(n.as_bytes()), HeaderValue::from_str(v.trim())) {
            builder = builder.header(n, v);
        }
    }
    builder
        .body(Full::new(Bytes::copy_from_slice(rest)).map_err(|e| match e {}).boxed())
        .unwrap_or_else(|_| text(StatusCode::BAD_GATEWAY, "lfs upstream headers were not valid HTTP\n"))
}
