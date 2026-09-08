//! The file API's HTTP listener (`docs/plans/forge-file-api-design.md`).
//!
//! On its OWN port, never the status listener's: `/status` is served
//! unauthenticated and the door must never be able to reach it, so the
//! two surfaces are separated by a port and a NetworkPolicy rule rather
//! than by a route table nobody can regress.
//!
//! Deliberately buffered rather than streaming. Every request and every
//! response is bounded by [`FileApiOpts::cap`], and the size of a read
//! is known from the tree before a byte is fetched — so the memory
//! bound is the cap, and the syncer's small container is safe. The day
//! a consumer needs unbounded objects, this is the decision to undo.
//!
//! Identity, per §4.4a: the **principal** is `X-Remote-User`, which the
//! door sets from a verified TokenReview and no caller can smuggle; the
//! **author** is `X-Flint-Author`, which the application supplies for
//! the end user it authenticated. Trust and authorship are separate,
//! which is what git's own author/committer split is for.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use super::fileapi::{self, FileError, FileWrite, Mutation};
use super::gitcmd::Git;
use super::server::FileApiOpts;
use super::status::Shared;
use super::ForgeResult;

/// Percent-decode one query value. Written here rather than pulled in
/// because the crate has no URL dependency and this is the whole of
/// what is needed — but it is caller-facing, so it is tested.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    // A stray `%` is data, not an error: refusing here
                    // would turn a legal filename into a 400.
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Split a request target into its path and its decoded parameters.
pub fn split_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let mut params = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(percent_decode(k), percent_decode(v));
    }
    (percent_decode(path), params)
}

/// Trim the leading and trailing slashes a UI will send. `/` and `""`
/// both mean the root, as they do in lite's API.
fn norm_path(raw: &str) -> String {
    raw.trim_matches('/').to_string()
}

struct Ctx {
    git: Git,
    opts: FileApiOpts,
    writes: tokio::sync::mpsc::Sender<FileWrite>,
    shared: Shared,
}

fn json_error(status: u16, reason: &str, message: &str) -> (u16, String, Vec<u8>) {
    let body = serde_json::json!({
        "error": reason, "reason": reason, "message": message
    });
    (status, "application/json".into(), serde_json::to_vec(&body).unwrap_or_default())
}

fn from_err(e: &FileError) -> (u16, String, Vec<u8>) {
    json_error(e.status(), e.reason(), &e.message())
}

pub async fn serve(
    opts: &FileApiOpts,
    git: Git,
    writes: tokio::sync::mpsc::Sender<FileWrite>,
    shared: Shared,
) -> ForgeResult<()> {
    // Bind with a bounded retry. A pod restarting onto the same port
    // can meet the old socket still closing, and a door that gave up on
    // that would need a human to restart it for a condition that clears
    // in seconds.
    let listener = {
        let mut last = None;
        let mut bound = None;
        for attempt in 0..40 {
            match TcpListener::bind(&opts.addr).await {
                Ok(l) => {
                    bound = Some(l);
                    break;
                }
                Err(e) => {
                    if attempt == 0 {
                        eprintln!("flint-forge: file API waiting to bind {} ({e})", opts.addr);
                    }
                    last = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
        }
        match bound {
            Some(l) => l,
            None => return Err(super::ForgeError::Io(last.expect("a bind error"))),
        }
    };
    let actual = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| opts.addr.clone());
    eprintln!("flint-forge: file API serving {actual} on branch {}", opts.branch);
    if let Some(slot) = opts.bound.as_ref() {
        if let Ok(mut g) = slot.lock() {
            *g = Some(actual.clone());
        }
    }
    let ctx = Arc::new(Ctx { git, opts: opts.clone(), writes, shared });
    loop {
        // An accept error is transient — a descriptor limit, a client
        // that vanished between SYN and accept. Returning here would
        // take the door down permanently for a condition that clears
        // on its own, and nothing would restart it.
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                eprintln!("flint-forge: file API accept failed ({e}); continuing");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                continue;
            }
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let mut reader = BufReader::new(r);
            let (code, ctype, body) = match handle(&ctx, &mut reader).await {
                Ok(v) => v,
                Err(e) => json_error(500, "internal", &e.to_string()),
            };
            let head = format!(
                "HTTP/1.1 {code} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                if (200..300).contains(&code) { "OK" } else { "Error" },
                body.len()
            );
            let _ = w.write_all(head.as_bytes()).await;
            let _ = w.write_all(&body).await;
            let _ = w.flush().await;
            // FIN rather than a bare drop: the client reads to EOF.
            let _ = w.shutdown().await;
        });
    }
}

async fn handle<R: tokio::io::AsyncRead + Unpin>(
    ctx: &Ctx,
    reader: &mut BufReader<R>,
) -> ForgeResult<(u16, String, Vec<u8>)> {
    // ── request line and headers ──────────────────────────────────
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(json_error(400, "bad-request", "empty request"));
    }
    let mut it = line.split_whitespace();
    let method = it.next().unwrap_or("").to_string();
    let target = it.next().unwrap_or("/").to_string();

    let mut headers: HashMap<String, String> = HashMap::new();
    let mut total = 0usize;
    loop {
        let mut h = String::new();
        let n = reader.read_line(&mut h).await?;
        if n == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        total += n;
        // A head with no bound is a way to hold a connection open for
        // free; the status listener has no cap and this one does.
        if total > 64 * 1024 || headers.len() > 100 {
            return Ok(json_error(431, "headers-too-large", "too many request headers"));
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    // The body is consumed BEFORE any answer, always. A server that
    // responds and closes with the client's bytes still unread gets a
    // TCP reset, and the reset discards the response the client needed
    // — so a refusal arrives as "connection reset by peer" instead of
    // as the reason it was refused. Measured under load.
    //
    // Reading it here does not weaken the auth-before-ROUTING property
    // below: it is bounded by the cap, and an over-cap body is drained
    // into a fixed buffer rather than allocated.
    let declared = headers.get("content-length").and_then(|v| v.parse::<usize>().ok());
    let mut over_cap = None;
    let body = match declared {
        Some(n) if n > ctx.opts.cap as usize => {
            let mut scratch = [0u8; 64 * 1024];
            let mut drained = 0usize;
            let bound = (ctx.opts.cap as usize).saturating_mul(2).min(8 * 1024 * 1024);
            while drained < n.min(bound) {
                match reader.read(&mut scratch).await {
                    Ok(0) | Err(_) => break,
                    Ok(k) => drained += k,
                }
            }
            over_cap = Some(n as u64);
            Vec::new()
        }
        Some(n) => {
            let mut buf = vec![0u8; n];
            // A short read is a client that hung up; answer rather than
            // failing the whole connection.
            if reader.read_exact(&mut buf).await.is_err() {
                return Ok(json_error(400, "bad-request", "the request body ended early"));
            }
            buf
        }
        None => Vec::new(),
    };

    let (path, params) = split_target(&target);
    if method == "GET" && path == "/healthz" {
        return Ok((200, "text/plain".into(), b"ok".to_vec()));
    }

    // ── authentication, BEFORE routing ────────────────────────────
    // Deliberately first: an unauthenticated request to any path is
    // 401, never 404, so the surface cannot be mapped and the readiness
    // answer below cannot be probed anonymously.
    if let Some(want) = ctx.opts.token.as_deref() {
        let given = headers.get("authorization").and_then(|h| h.strip_prefix("Bearer "));
        if given.map(|t| !constant_time_eq(t.as_bytes(), want.as_bytes())).unwrap_or(true) {
            return Ok(json_error(401, "unauthorized", "bearer token required"));
        }
    }
    // The door sets this from a verified TokenReview and strips any the
    // caller sent. No principal means no door, which is a deployment
    // error rather than an anonymous write.
    let principal = match headers.get("x-remote-user").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(p) => p.to_string(),
        None => {
            return Ok(json_error(
                403,
                "no-principal",
                "this request carried no verified identity",
            ))
        }
    };
    let author = headers
        .get("x-flint-author")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(&principal)
        .to_string();

    // ── readiness ─────────────────────────────────────────────────
    {
        let phase = ctx.shared.lock().map(|f| f.phase).unwrap_or(super::status::Phase::Starting);
        if !matches!(phase, super::status::Phase::Serving) {
            let mut r = json_error(
                503,
                "not-serving",
                &format!("the repository is not serving yet (phase: {phase:?})"),
            );
            r.1 = "application/json".into();
            return Ok(r);
        }
    }

    if let Some(n) = over_cap {
        return Ok(from_err(&FileError::TooLarge { size: n, cap: ctx.opts.cap }));
    }

    let branch = ctx.opts.branch.clone();
    let tree = match fileapi::tree_of(&ctx.git, &branch).await {
        Ok(t) => t,
        Err(e) => return Ok(from_err(&e)),
    };
    let qpath = norm_path(params.get("path").map(|s| s.as_str()).unwrap_or(""));
    let if_match = headers.get("if-match").cloned();

    Ok(match (method.as_str(), path.as_str()) {
        ("GET", "/files") => match fileapi::list(&ctx.git, tree.as_deref(), &qpath).await {
            Ok(l) => (
                200,
                "application/json".into(),
                serde_json::to_vec(&l).unwrap_or_default(),
            ),
            Err(e) => from_err(&e),
        },
        ("GET", "/files/content") => {
            match fileapi::read(&ctx.git, tree.as_deref(), &qpath, ctx.opts.cap).await {
                Ok((entry, bytes)) => {
                    return Ok((200, format!("application/octet-stream\r\nETag: \"{}\"", entry.etag), bytes))
                }
                Err(e) => from_err(&e),
            }
        }
        ("PUT", "/files/content") => {
            submit(ctx, Mutation::Put { path: qpath, body, if_match }, &author, &principal).await
        }
        ("DELETE", "/files/content") => {
            submit(ctx, Mutation::Delete { path: qpath, if_match }, &author, &principal).await
        }
        ("POST", "/files/move") => {
            #[derive(serde::Deserialize)]
            struct MoveBody {
                from: String,
                to: String,
            }
            match serde_json::from_slice::<MoveBody>(&body) {
                Ok(m) => {
                    submit(
                        ctx,
                        Mutation::Move {
                            from: norm_path(&m.from),
                            to: norm_path(&m.to),
                            if_match,
                        },
                        &author,
                        &principal,
                    )
                    .await
                }
                Err(e) => json_error(400, "bad-body", &format!("malformed body: {e}")),
            }
        }
        // git has no empty directories: a directory exists because a
        // file is under it. Fabricating a `.gitkeep` would make the
        // listing lie, so the verb says what is true instead.
        ("POST", "/files/folder") => json_error(
            501,
            "no-empty-directories",
            "this repository cannot hold an empty directory; create the first file in it \
             and the directory appears with it",
        ),
        ("GET", _) | ("PUT", _) | ("POST", _) | ("DELETE", _) => {
            json_error(404, "no-such-route", "no such route")
        }
        _ => json_error(405, "method-not-allowed", "method not allowed"),
    })
}

/// Hand a mutation to the serving loop and wait for its verdict.
async fn submit(
    ctx: &Ctx,
    mutation: Mutation,
    author: &str,
    principal: &str,
) -> (u16, String, Vec<u8>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let req = FileWrite {
        mutation,
        author: author.to_string(),
        principal: principal.to_string(),
        cap: ctx.opts.cap,
        reply: tx,
    };
    if ctx.writes.send(req).await.is_err() {
        return json_error(503, "not-serving", "the repository is not accepting writes");
    }
    match rx.await {
        Ok(Ok(etag)) => {
            let body = serde_json::json!({ "status": "written", "etag": etag });
            (
                200,
                format!("application/json\r\nETag: \"{etag}\""),
                serde_json::to_vec(&body).unwrap_or_default(),
            )
        }
        Ok(Err(e)) => from_err(&e),
        Err(_) => json_error(503, "not-serving", "the write was never answered"),
    }
}

/// Length-independent only in its comparison; a length mismatch still
/// returns early, which leaks the token's LENGTH and nothing else.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
