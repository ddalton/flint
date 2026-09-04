//! The hook-to-syncer channel.
//!
//! A hook is a short-lived process that git spawns per push; the
//! syncer is the long-lived one that owns the writer lock. They meet
//! on a Unix socket inside the pod, which is the smallest surface that
//! gives the syncer what it must have — every push in one process, in
//! an order it chooses — and gives the hook what it must have: a
//! report it can relay verbatim.
//!
//! One JSON document per line in each direction. Not pkt-line: the
//! hook already translates git's pkt-line conversation, and a second
//! implementation of it here would be a second place to get the
//! framing wrong for no gain. JSON strings escape newlines, so the
//! framing is unambiguous.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use tokio::sync::{mpsc, oneshot};

use super::batch::{CommandResult, PushRequest};
use super::gitcmd::RefUpdate;
use super::{ForgeError, ForgeResult};

/// The socket, beside the repository so it shares the pod's lifetime.
pub const SOCKET_NAME: &str = "syncer.sock";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HookRequest {
    /// The principal the door authenticated, as `REMOTE_USER`. Empty
    /// when a hook was reached without one, which the syncer records
    /// but does not use for authorization — that is `pre-receive`'s
    /// job, and it runs before the pack is even accepted.
    #[serde(default)]
    pub principal: String,
    #[serde(default)]
    pub options: Vec<String>,
    pub commands: Vec<RefUpdate>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HookResponse {
    pub results: Vec<CommandResult>,
}

/// What the serving loop receives: a push and the channel its report
/// must come back on.
pub struct Incoming {
    pub request: HookRequest,
    pub reply: oneshot::Sender<HookResponse>,
}

/// Accept hook connections forever, forwarding each push to the
/// serving loop and relaying its report.
///
/// A connection that dies before its report is delivered is not an
/// error here: the client's push has already failed, and the batch
/// that was running for it still completed or still fenced. The
/// syncer's correctness never depends on anyone hearing the answer.
pub async fn serve(socket: &Path, tx: mpsc::Sender<Incoming>) -> ForgeResult<()> {
    if socket.exists() {
        std::fs::remove_file(socket)?;
    }
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = tokio::net::UnixListener::bind(socket)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, tx).await {
                eprintln!("flint-forge: hook connection: {e}");
            }
        });
    }
}

async fn handle(stream: tokio::net::UnixStream, tx: mpsc::Sender<Incoming>) -> ForgeResult<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
    let (r, mut w) = stream.into_split();
    let mut lines = TokioBufReader::new(r).lines();
    let Some(line) = lines.next_line().await? else { return Ok(()) };
    let request: HookRequest = serde_json::from_str(&line)
        .map_err(|e| ForgeError::State(format!("hook sent an unparseable request: {e}")))?;
    let (reply, wait) = oneshot::channel();
    tx.send(Incoming { request, reply })
        .await
        .map_err(|_| ForgeError::State("the serving loop is gone".into()))?;
    let response = wait
        .await
        .map_err(|_| ForgeError::State("the serving loop dropped this push".into()))?;
    let mut body = serde_json::to_vec(&response)
        .map_err(|e| ForgeError::State(format!("report will not serialise: {e}")))?;
    body.push(b'\n');
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// The hook side: blocking, because a hook is a short-lived process
/// whose only job is this one round trip.
pub fn ask(socket: &Path, request: &HookRequest) -> std::io::Result<HookResponse> {
    let mut stream = UnixStream::connect(socket)?;
    let mut body = serde_json::to_vec(request)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    body.push(b'\n');
    stream.write_all(&body)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the syncer closed the connection without a report",
        ));
    }
    serde_json::from_str(&line)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Turn a hook request into the batch's shape.
pub fn to_push(id: u64, req: HookRequest) -> PushRequest {
    PushRequest {
        id,
        principal: req.principal,
        options: req.options,
        commands: req.commands,
    }
}
