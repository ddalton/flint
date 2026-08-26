//! The UDS door (§2.5, Phase 5) — `.flint-sync/ctl.sock`.
//!
//! Three verbs: `POST /v1/boundary`, `POST /v1/sync`, `GET /v1/status`.
//! It is deliberately **sugar over the file protocol**, not a second
//! protocol: a request lands in the same pending record a
//! `.flint/publish` touch would, so min-interval, coalescing, the
//! work-metered budget, the covered-nonce ack and every crash rule
//! apply to it without a second implementation that could disagree
//! with the first. What the socket buys is a SYNCHRONOUS answer —
//! the caller gets the ack in the response instead of polling
//! `.flint/publish.ack`.
//!
//! Why it is pod-internal only, and stays that way: the socket lives in
//! the state directory, which is outside the scan by the shipped
//! exclusion and inside the emptyDir every container in the pod shares.
//! There is no TCP listener and no authentication, because the trust
//! boundary is the pod — exactly as it is for the file sentinels, which
//! any process in the pod can already touch (§3 residual 6). A TCP
//! listener would be a new remote surface, which is a stated non-goal.
//!
//! The listener owns NO sidecar state. It forwards requests over a
//! channel to the run loop, which is the single thread that holds the
//! lease and the state directory — the same reason `flint-sync status`
//! reads files instead of claiming anything.

use std::path::{Path, PathBuf};

use tokio::sync::{mpsc, oneshot};

/// What the socket asks the run loop to do.
#[derive(Debug)]
pub enum CtlRequest {
    /// Honor a boundary now (subject to min-interval and budget).
    Boundary { note: Option<String>, reply: oneshot::Sender<serde_json::Value> },
    /// Run the sync verb. Unlike the GATEWAY's sync request — which is
    /// carried, never performed (D14) — this one executes, because the
    /// caller is inside the pod: it is the agent asking for its own
    /// tree to be updated, which is the agent's own decision to make.
    Sync { reply: oneshot::Sender<serde_json::Value> },
    /// The same report `flint-sync status` renders.
    Status { reply: oneshot::Sender<serde_json::Value> },
}

/// Bind the socket, replacing a stale one left by a crashed container.
///
/// A leftover socket file is the normal case after a container restart
/// (the emptyDir survives, the process does not), and `bind` fails with
/// EADDRINUSE on it. Refusing to start on that would make the door
/// work exactly once per pod.
pub fn bind(path: &Path) -> std::io::Result<tokio::net::UnixListener> {
    if let Ok(m) = std::fs::symlink_metadata(path) {
        // Only ever unlink a SOCKET. A regular file or a symlink here is
        // not ours to remove — it is either someone else's data or an
        // attempt to make us delete something.
        use std::os::unix::fs::FileTypeExt;
        if !m.file_type().is_socket() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "control socket path is occupied by a non-socket",
            ));
        }
        let _ = std::fs::remove_file(path);
    }
    tokio::net::UnixListener::bind(path)
}

/// Serve until the listener dies. One connection, one request, one
/// response — HTTP/1.1 with no keep-alive, which is all `curl
/// --unix-socket` and every agent library needs.
pub async fn serve(listener: tokio::net::UnixListener, tx: mpsc::Sender<CtlRequest>) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("flint-sync: ctl.sock accept failed: {e}");
                return;
            }
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, tx).await {
                eprintln!("flint-sync: ctl.sock connection: {e}");
            }
        });
    }
}

const MAX_REQUEST_BYTES: usize = 16 * 1024;

async fn handle_conn(
    mut stream: tokio::net::UnixStream,
    tx: mpsc::Sender<CtlRequest>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Read only the request line + headers. Bounded, because a hung or
    // hostile in-pod caller must not be able to grow this task without
    // limit — the same rule the sentinel body reader follows.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX_REQUEST_BYTES {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    let (code, body) = match (method, path) {
        ("POST", "/v1/boundary") => {
            let (reply, rx) = oneshot::channel();
            match tx.send(CtlRequest::Boundary { note: None, reply }).await {
                Ok(()) => match rx.await {
                    Ok(v) => (200, v),
                    Err(_) => (503, json_err("the sidecar did not answer")),
                },
                Err(_) => (503, json_err("the sidecar run loop is gone")),
            }
        }
        ("POST", "/v1/sync") => {
            let (reply, rx) = oneshot::channel();
            match tx.send(CtlRequest::Sync { reply }).await {
                Ok(()) => match rx.await {
                    Ok(v) => (200, v),
                    Err(_) => (503, json_err("the sidecar did not answer")),
                },
                Err(_) => (503, json_err("the sidecar run loop is gone")),
            }
        }
        ("GET", "/v1/status") => {
            let (reply, rx) = oneshot::channel();
            match tx.send(CtlRequest::Status { reply }).await {
                Ok(()) => match rx.await {
                    Ok(v) => (200, v),
                    Err(_) => (503, json_err("the sidecar did not answer")),
                },
                Err(_) => (503, json_err("the sidecar run loop is gone")),
            }
        }
        _ => (
            404,
            json_err("unknown verb (POST /v1/boundary, POST /v1/sync, GET /v1/status)"),
        ),
    };
    let body = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    let resp = format!(
        "HTTP/1.1 {code} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n",
        match code {
            200 => "OK",
            404 => "Not Found",
            _ => "Service Unavailable",
        },
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

fn json_err(msg: &str) -> serde_json::Value {
    serde_json::json!({ "status": "error", "message": msg })
}

/// Where the socket lives: the state directory, which is outside the
/// scan and shared by every container in the pod.
pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ctl.sock")
}
