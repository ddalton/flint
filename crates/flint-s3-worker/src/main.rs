//! `flint-s3-worker` — PID 1 of a flint-s3 worker pod.
//!
//! One worker pod per published volume (design §3.1). The node plugin
//! creates the pod, waits for it to run, then connects to the socket
//! this process listens on inside the pod's memory-backed `comm`
//! emptyDir and sends ONE launch message:
//!
//! ```json
//! {"mode":"passthrough","args":["bucket","{FUSE_FD}","--foreground",…],"env":{…}}
//! ```
//!
//! For `passthrough` the message carries a `/dev/fuse` fd via
//! `SCM_RIGHTS`: the plugin already performed the `mount(2)` as root,
//! so the mounter this process spawns needs no capability, no
//! `/dev/fuse` and no fusermount — it reads FUSE requests from an fd it
//! inherited and is told its "mount point" is `/dev/fd/3` (the
//! Mountpoint fd mode; AWS's CSI v2 and Google's gcsfuse driver do the
//! same). For `lean` there is no fd: the child is `flint-sync run` over
//! a tree the plugin bind-mounted at `/workspace`.
//!
//! The message is persisted to `comm/launch.json` before the child
//! starts. A lean worker restarted by kubelet (`restartPolicy:
//! OnFailure`) finds it and relaunches WITHOUT the plugin — the plugin's
//! `NodePublishVolume` returned long ago and nobody will connect again.
//! A passthrough worker never restarts (`Never`): a restarted mounter
//! cannot re-acquire an fd that was passed once, and pretending
//! otherwise would hide a dead mount.
//!
//! This process also serves the loopback credentials door
//! (`http://127.0.0.1:9911/v1/creds`): the AWS container-credentials
//! provider that both mount-s3 (CRT) and the Rust SDK consume unchanged
//! — plain `http` is accepted only to loopback, and the request must
//! carry the contents of `comm/auth.token`. The node plugin writes
//! `comm/creds.json` host-side (`{AccessKeyId, SecretAccessKey, Token,
//! Expiration}`) and rewrites it on republish; this door only serves the
//! file. No credential is ever placed in the pod spec.
//!
//! PID 1 duties: forward SIGTERM/SIGINT to the child, reap orphans,
//! exit with the child's status. On child exit the tail of its stderr
//! is written to `comm/mount.error` so the plugin can name the failure
//! in an Event without `pods/log` RBAC.

use std::collections::BTreeMap;
use std::io::{IoSliceMut, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};

/// The launch message (the contract with the node plugin's
/// `s3csi::fuse`/`s3csi::worker`). Field names are the protocol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Launch {
    /// `passthrough` | `lean`.
    pub mode: String,
    /// Arguments AFTER the binary. `{FUSE_FD}` is replaced by
    /// `/dev/fd/3` in passthrough mode.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the child, layered over this process's.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// What we answer on the same socket, once.
#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub const FUSE_FD_PLACEHOLDER: &str = "{FUSE_FD}";
/// The fd number the child inherits the FUSE device on.
pub const CHILD_FUSE_FD: RawFd = 3;
pub const SOCK_NAME: &str = "mount.sock";
pub const LAUNCH_NAME: &str = "launch.json";
pub const ERROR_NAME: &str = "mount.error";
pub const CREDS_NAME: &str = "creds.json";
pub const AUTH_TOKEN_NAME: &str = "auth.token";

static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    PENDING_SIGNAL.store(sig, Ordering::SeqCst);
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

/// `await-release` — the container's preStop hook.
///
/// WHY THIS EXISTS. A worker holds the mount (passthrough) or the
/// syncer (lean) for a tenant pod in another namespace. Nothing in
/// Kubernetes orders the two deaths: `kubectl drain` evicts both at
/// once, and kubelet's graceful node shutdown terminates by priority.
/// A worker that dies first leaves its tenant on a dead mount —
/// `ENOTCONN` for passthrough, and for lean a final publish that never
/// runs, so everything written since the last one stays on the node.
///
/// A PodDisruptionBudget was the first answer and was wrong: it covers
/// the eviction path ONLY, stalls autoscaler scale-down, blocks a drain
/// for as long as any tenant refuses to die, and is bypassed by
/// `--disable-eviction` exactly when someone is in a hurry. The
/// upstream drivers with this same architecture hit exactly this and
/// answered it with ordering rather than a budget:
/// awslabs/mountpoint-s3-csi-driver#607 ("Draining nodes breaks
/// applications depending on the mountpoint volume while draining")
/// was answered by graceful eviction — a mount pod stays alive until
/// every workload pod using its volume has terminated — and
/// juicedata/juicefs-csi-driver#856 ("mount pod terminates before my
/// application pod when draining a node") is the same failure, reported
/// from a node-termination handler draining an AWS spot node. When the
/// order of two shutdowns matters, a preStop hook is the tool
/// Kubernetes offers.
///
/// So: kubelet runs this BEFORE it sends SIGTERM to the worker, on
/// every path that terminates a pod. It returns as soon as the plugin
/// has released the volume, which on the ordinary path has already
/// happened — NodeUnpublish writes the marker before it deletes the
/// worker, so the happy path costs one stat. On a drain it holds the
/// worker open until the tenant is gone. It NEVER blocks forever: the
/// budget is its own, well inside terminationGracePeriodSeconds, and it
/// exits 0 on timeout so termination proceeds exactly as it would have.
/// Blocking a shutdown is not on the table; the worst case here is the
/// behaviour we already had.
const RELEASED_MARKER: &str = "released";

fn await_release(comm: &Path, budget: Duration) -> ! {
    let marker = comm.join(RELEASED_MARKER);
    let start = std::time::Instant::now();
    if marker.exists() {
        // The ordinary path: the plugin released the volume and then
        // deleted us. Nothing to wait for.
        std::process::exit(0);
    }
    eprintln!(
        "flint-s3-worker: preStop — the volume is not released yet; holding the mount open for up to {}s \
         so the tenant is not left on a dead mount",
        budget.as_secs()
    );
    while start.elapsed() < budget {
        if marker.exists() {
            eprintln!("flint-s3-worker: preStop — released after {:?}; terminating", start.elapsed());
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // Timed out. Exit 0 on purpose: a non-zero preStop is logged as a
    // FailedPreStopHook event and changes nothing about the outcome,
    // and refusing to return would only spend the grace period the
    // lean syncer still needs for its final publish.
    eprintln!(
        "flint-s3-worker: preStop — still unreleased after {}s; terminating anyway (the tenant may see \
         a dead mount, and for lean the last writes are recoverable with recover-staged)",
        budget.as_secs()
    );
    std::process::exit(0);
}

fn main() {
    let comm = PathBuf::from(env_or("FLINT_S3W_COMM", "/comm"));
    if std::env::args().nth(1).as_deref() == Some("await-release") {
        let budget = env_or("FLINT_S3W_PRESTOP_SECS", "60").parse().unwrap_or(60);
        await_release(&comm, Duration::from_secs(budget));
    }
    let accept_secs: u64 = env_or("FLINT_S3W_ACCEPT_SECS", "300").parse().unwrap_or(300);
    let door = env_or("FLINT_S3W_DOOR", "127.0.0.1:9911");

    if let Err(e) = std::fs::create_dir_all(&comm) {
        eprintln!("flint-s3-worker: cannot create {}: {e}", comm.display());
        std::process::exit(2);
    }

    // The door first, and BOUND HERE, in the main thread: a child that
    // reaches the door before the listener exists gets a connection
    // refused, and an AWS SDK reports that as a bare "dispatch
    // failure" and gives up — measured on kind, where the lean syncer
    // exited 1 on its first request while the door thread was still
    // starting. Binding before the spawn makes the socket exist for
    // every child, always; only the accept loop runs in the thread.
    if door != "off" {
        match std::net::TcpListener::bind(&door) {
            Ok(listener) => {
                eprintln!("flint-s3-worker: credentials door on http://{door}/v1/creds");
                let comm_for_door = comm.clone();
                std::thread::Builder::new()
                    .name("creds-door".into())
                    .spawn(move || serve_door(listener, &comm_for_door))
                    .expect("spawn door thread");
            }
            Err(e) => {
                // Fatal: a lean worker whose credentials are unreachable
                // fails every request, and failing here names the reason
                // instead of leaving a "dispatch failure" in the log.
                eprintln!("flint-s3-worker: door bind {door}: {e}");
                std::process::exit(4);
            }
        }
    }

    // Relaunch path: the message from a previous incarnation of this
    // container. Only meaningful without an fd, i.e. lean.
    let persisted = comm.join(LAUNCH_NAME);
    let (launch, fuse_fd): (Launch, Option<OwnedFd>) = match std::fs::read(&persisted) {
        Ok(bytes) if !bytes.is_empty() => match serde_json::from_slice::<Launch>(&bytes) {
            Ok(l) if l.mode == "lean" => {
                eprintln!("flint-s3-worker: relaunching from {}", persisted.display());
                (l, None)
            }
            Ok(l) => {
                eprintln!(
                    "flint-s3-worker: {} holds a {} launch, which cannot be relaunched (the \
                     FUSE fd was passed once) — exiting so the pod reports Failed",
                    persisted.display(),
                    l.mode
                );
                std::process::exit(3);
            }
            Err(e) => {
                eprintln!("flint-s3-worker: {} unreadable ({e}); waiting for the plugin", persisted.display());
                wait_for_launch(&comm, accept_secs)
            }
        },
        _ => wait_for_launch(&comm, accept_secs),
    };

    let code = supervise(&comm, &launch, fuse_fd);
    std::process::exit(code);
}

/// Bind the socket and wait for the plugin's one message.
fn wait_for_launch(comm: &Path, accept_secs: u64) -> (Launch, Option<OwnedFd>) {
    let sock = comm.join(SOCK_NAME);
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("flint-s3-worker: bind {}: {e}", sock.display());
            std::process::exit(2);
        }
    };
    let _ = std::fs::set_permissions(&sock, std::os::unix::fs::PermissionsExt::from_mode(0o600));
    listener.set_nonblocking(true).expect("nonblocking listener");
    eprintln!("flint-s3-worker: listening on {} (accept budget {accept_secs}s)", sock.display());

    let deadline = Instant::now() + Duration::from_secs(accept_secs);
    loop {
        if PENDING_SIGNAL.load(Ordering::SeqCst) != 0 {
            eprintln!("flint-s3-worker: signalled before launch; exiting");
            std::process::exit(0);
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).ok();
                match receive_launch(&stream) {
                    Ok((launch, fd)) => {
                        // The answer goes back on this same connection
                        // once the child is up (or failed to start).
                        *REPLY_STREAM.lock().unwrap() = Some(stream);
                        if launch.mode == "lean" {
                            // Persist BEFORE the child starts, so a restart
                            // never races a half-written file.
                            let tmp = comm.join(format!("{LAUNCH_NAME}.tmp"));
                            if let Err(e) = std::fs::write(&tmp, serde_json::to_vec(&launch).unwrap())
                                .and_then(|_| std::fs::rename(&tmp, comm.join(LAUNCH_NAME)))
                            {
                                eprintln!("flint-s3-worker: persist launch: {e}");
                            }
                        }
                        return (launch, fd);
                    }
                    Err(e) => {
                        eprintln!("flint-s3-worker: bad launch message: {e}");
                        let _ = write_reply(&stream, &Reply { ok: false, pid: None, error: Some(e) });
                        // Keep listening: the plugin retries.
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    eprintln!("flint-s3-worker: no launch message within {accept_secs}s — exiting");
                    std::process::exit(4);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("flint-s3-worker: accept: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// One `recvmsg` with a control buffer sized for a single fd, then the
/// rest of the length-prefixed JSON with plain reads.
fn receive_launch(stream: &UnixStream) -> Result<(Launch, Option<OwnedFd>), String> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut cmsg = nix::cmsg_space!([RawFd; 1]);
    let (n, fd) = {
        let mut iov = [IoSliceMut::new(&mut buf)];
        let msg = recvmsg::<()>(stream.as_raw_fd(), &mut iov, Some(&mut cmsg), MsgFlags::empty())
            .map_err(|e| format!("recvmsg: {e}"))?;
        let mut fd = None;
        for c in msg.cmsgs() {
            if let ControlMessageOwned::ScmRights(fds) = c {
                for f in fds {
                    if fd.is_none() {
                        // SAFETY: the kernel just installed this fd in our table.
                        fd = Some(unsafe { OwnedFd::from_raw_fd(f) });
                    } else {
                        let _ = nix::unistd::close(f);
                    }
                }
            }
        }
        (msg.bytes, fd)
    };
    if n < 4 {
        return Err(format!("short message ({n} bytes)"));
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > 1 << 20 {
        return Err(format!("launch message of {len} bytes refused"));
    }
    let mut body = buf[4..n].to_vec();
    let mut s = stream;
    while body.len() < len {
        let mut chunk = vec![0u8; len - body.len()];
        let got = s.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
        if got == 0 {
            return Err("connection closed mid-message".into());
        }
        body.extend_from_slice(&chunk[..got]);
    }
    let launch: Launch = serde_json::from_slice(&body[..len]).map_err(|e| format!("json: {e}"))?;
    match launch.mode.as_str() {
        "passthrough" if fd.is_none() => Err("passthrough launch carried no fd".into()),
        "passthrough" | "lean" => Ok((launch, fd)),
        other => Err(format!("unknown mode {other:?}")),
    }
}

fn write_reply(mut stream: &UnixStream, reply: &Reply) -> std::io::Result<()> {
    let body = serde_json::to_vec(reply).unwrap();
    stream.write_all(&(body.len() as u32).to_be_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

/// Spawn the child, answer the plugin, forward signals, reap, exit code.
fn supervise(comm: &Path, launch: &Launch, fuse_fd: Option<OwnedFd>) -> i32 {
    // Install handlers before the child exists so a SIGTERM racing the
    // spawn is not lost.
    for sig in [Signal::SIGTERM, Signal::SIGINT] {
        // SAFETY: the handler only stores an int.
        unsafe { signal::signal(sig, SigHandler::Handler(on_signal)) }.expect("install handler");
    }

    let (bin, args): (String, Vec<String>) = match launch.mode.as_str() {
        "passthrough" => {
            let bin = env_or("FLINT_S3W_MOUNTER", "/usr/bin/mount-s3");
            let args = launch
                .args
                .iter()
                .map(|a| {
                    if a == FUSE_FD_PLACEHOLDER {
                        format!("/dev/fd/{CHILD_FUSE_FD}")
                    } else {
                        a.clone()
                    }
                })
                .collect();
            (bin, args)
        }
        _ => (env_or("FLINT_S3W_SYNC", "/usr/local/bin/flint-sync"), launch.args.clone()),
    };

    let mut cmd = Command::new(&bin);
    cmd.args(&args);
    for (k, v) in &launch.env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::piped());
    if let Some(fd) = fuse_fd.as_ref() {
        let raw = fd.as_raw_fd();
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if raw != CHILD_FUSE_FD {
                    if libc::dup2(raw, CHILD_FUSE_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                // dup2 clears FD_CLOEXEC on the new fd; if raw == 3 the
                // OwnedFd's CLOEXEC would strip it at exec, so clear it.
                let flags = libc::fcntl(CHILD_FUSE_FD, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(CHILD_FUSE_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                }
                Ok(())
            });
        }
    }

    eprintln!("flint-s3-worker: launching {bin} {}", redact(&args));
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("spawn {bin}: {e}");
            eprintln!("flint-s3-worker: {msg}");
            let _ = std::fs::write(comm.join(ERROR_NAME), &msg);
            answer_plugin(comm, &Reply { ok: false, pid: None, error: Some(msg) });
            return 5;
        }
    };
    // The parent's copy of the FUSE fd goes now: the child owns the
    // connection, and holding a second reference would keep a dead
    // mount's connection alive after the mounter exits.
    drop(fuse_fd);

    let pid = child.id() as i32;
    answer_plugin(comm, &Reply { ok: true, pid: Some(pid), error: None });

    // Tee stderr: container log + a bounded tail for mount.error.
    let tail: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    if let Some(mut err) = child.stderr.take() {
        let tail = tail.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match err.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let _ = std::io::stderr().write_all(&buf[..n]);
                        let mut t = tail.lock().unwrap();
                        t.extend_from_slice(&buf[..n]);
                        if t.len() > 8192 {
                            let cut = t.len() - 8192;
                            t.drain(..cut);
                        }
                    }
                }
            }
        });
    }

    let mut forwarded = false;
    let status: ExitStatus = loop {
        let sig = PENDING_SIGNAL.swap(0, Ordering::SeqCst);
        if sig != 0 && !forwarded {
            eprintln!("flint-s3-worker: forwarding signal {sig} to pid {pid}");
            let _ = signal::kill(Pid::from_raw(pid), Signal::try_from(sig).unwrap_or(Signal::SIGTERM));
            forwarded = true;
        }
        // Reap everything (we are PID 1); stop when OUR child is gone.
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => {
                eprintln!("flint-s3-worker: try_wait: {e}");
                break ExitStatus::from_raw_code(1);
            }
        }
        reap_orphans();
        std::thread::sleep(Duration::from_millis(100));
    };

    let code = exit_code(status);
    if !status.success() {
        let t = tail.lock().unwrap();
        let text = format!(
            "{bin} exited {status}\n{}",
            String::from_utf8_lossy(&t).trim_end()
        );
        let _ = std::fs::write(comm.join(ERROR_NAME), text);
    }
    eprintln!("flint-s3-worker: child exited {status}; exiting {code}");
    code
}

trait FromRawCode {
    fn from_raw_code(code: i32) -> ExitStatus;
}
impl FromRawCode for ExitStatus {
    fn from_raw_code(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }
}

fn exit_code(st: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(1))
}

fn reap_orphans() {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(_) => break,
            Ok(_) => continue,
        }
    }
}

/// Answer on the connection the launch arrived on. On the relaunch path
/// there is no connection; leave a note for whoever looks.
fn answer_plugin(comm: &Path, reply: &Reply) {
    if let Some(stream) = REPLY_STREAM.lock().unwrap().take() {
        if let Err(e) = write_reply(&stream, reply) {
            eprintln!("flint-s3-worker: reply: {e}");
        }
    } else {
        // Relaunch path: nobody is listening; leave a note.
        let _ = std::fs::write(comm.join("relaunch.json"), serde_json::to_vec(reply).unwrap());
    }
}

static REPLY_STREAM: Mutex<Option<UnixStream>> = Mutex::new(None);

/// Argument redaction for logs: values after `--` flags whose names
/// suggest a secret are masked. mount-s3 takes none on argv today, but
/// `mountOptions` come from a tenant-writable CR.
fn redact(args: &[String]) -> String {
    let mut out = Vec::with_capacity(args.len());
    let mut mask_next = false;
    for a in args {
        if mask_next {
            out.push("***".to_string());
            mask_next = false;
            continue;
        }
        let l = a.to_ascii_lowercase();
        if l.contains("secret") || l.contains("token") || l.contains("password") {
            if let Some((k, _)) = a.split_once('=') {
                out.push(format!("{k}=***"));
            } else {
                out.push(a.clone());
                mask_next = true;
            }
        } else {
            out.push(a.clone());
        }
    }
    out.join(" ")
}

// ── the loopback credentials door ────────────────────────────────────

/// `GET /v1/creds` with `Authorization: <auth.token>` ⇒ `creds.json`.
/// Any other path ⇒ 404; missing files ⇒ 503; wrong token ⇒ 401.
/// The AWS container-credentials provider (CRT and Rust SDK) needs
/// exactly this: JSON `{AccessKeyId, SecretAccessKey, Token,
/// Expiration}` over plain http on loopback, re-fetched before
/// `Expiration`.
/// The listener is bound by `main` BEFORE the child is spawned (see
/// there); this is the accept loop only.
fn serve_door(listener: std::net::TcpListener, comm: &Path) {
    for conn in listener.incoming() {
        let Ok(mut stream) = conn else { continue };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = [0u8; 8192];
        let n = match stream.read(&mut buf) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let req = String::from_utf8_lossy(&buf[..n]);
        let (status, body) = door_response(&req, comm);
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.write_all(&body);
        let _ = stream.flush();
    }
}

fn door_response(req: &str, comm: &Path) -> (&'static str, Vec<u8>) {
    let mut lines = req.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    if method != "GET" || path != "/v1/creds" {
        return ("404 Not Found", b"{\"error\":\"not found\"}".to_vec());
    }
    let presented = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v.trim().to_string());
    let expected = match std::fs::read_to_string(comm.join(AUTH_TOKEN_NAME)) {
        Ok(t) => t.trim().to_string(),
        Err(_) => return ("503 Service Unavailable", b"{\"error\":\"no auth token yet\"}".to_vec()),
    };
    if expected.is_empty() || presented.as_deref() != Some(expected.as_str()) {
        return ("401 Unauthorized", b"{\"error\":\"unauthorized\"}".to_vec());
    }
    match std::fs::read(comm.join(CREDS_NAME)) {
        Ok(b) if !b.is_empty() => ("200 OK", b),
        _ => ("503 Service Unavailable", b"{\"error\":\"no credentials yet\"}".to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_round_trips_and_placeholder_is_literal() {
        let l = Launch {
            mode: "passthrough".into(),
            args: vec!["b".into(), FUSE_FD_PLACEHOLDER.into(), "--foreground".into()],
            env: BTreeMap::from([("AWS_REGION".into(), "us-east-1".into())]),
        };
        let j = serde_json::to_string(&l).unwrap();
        assert!(j.contains("{FUSE_FD}"));
        assert_eq!(serde_json::from_str::<Launch>(&j).unwrap(), l);
    }

    #[test]
    fn door_refuses_without_the_token_and_serves_with_it() {
        let dir = std::env::temp_dir().join(format!("s3w-door-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (s, _) = door_response("GET /v1/creds HTTP/1.1\r\n\r\n", &dir);
        assert!(s.starts_with("503"), "no token file yet: {s}");
        std::fs::write(dir.join(AUTH_TOKEN_NAME), "abc\n").unwrap();
        std::fs::write(dir.join(CREDS_NAME), "{\"AccessKeyId\":\"k\"}").unwrap();
        let (s, _) = door_response("GET /v1/creds HTTP/1.1\r\nAuthorization: nope\r\n\r\n", &dir);
        assert!(s.starts_with("401"), "{s}");
        let (s, b) = door_response("GET /v1/creds HTTP/1.1\r\nauthorization: abc\r\n\r\n", &dir);
        assert!(s.starts_with("200"), "{s}");
        assert_eq!(b, b"{\"AccessKeyId\":\"k\"}");
        let (s, _) = door_response("GET /other HTTP/1.1\r\nauthorization: abc\r\n\r\n", &dir);
        assert!(s.starts_with("404"), "{s}");
        let (s, _) = door_response("POST /v1/creds HTTP/1.1\r\nauthorization: abc\r\n\r\n", &dir);
        assert!(s.starts_with("404"), "{s}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redaction_masks_secret_shaped_arguments() {
        let r = redact(&["--endpoint-url".into(), "http://x".into(), "--session-token".into(), "abc".into(), "--k=SECRET1".into()]);
        assert_eq!(r, "--endpoint-url http://x --session-token *** --k=***");
    }

    /// The real socket path: a client sends a launch with an fd, the
    /// server receives both.
    #[test]
    fn receive_launch_gets_fd_and_body() {
        use nix::sys::socket::{sendmsg, ControlMessage};
        use std::io::IoSlice;
        let (a, b) = UnixStream::pair().unwrap();
        let launch = Launch { mode: "passthrough".into(), args: vec!["x".into()], env: BTreeMap::new() };
        let body = serde_json::to_vec(&launch).unwrap();
        let mut framed = (body.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(&body);
        let f = std::fs::File::open("/dev/null").unwrap();
        let fds = [f.as_raw_fd()];
        let cmsg = [ControlMessage::ScmRights(&fds)];
        sendmsg::<()>(a.as_raw_fd(), &[IoSlice::new(&framed)], &cmsg, MsgFlags::empty(), None).unwrap();
        let (got, fd) = receive_launch(&b).unwrap();
        assert_eq!(got, launch);
        assert!(fd.is_some(), "fd must arrive with the message");
        // A lean launch without an fd is fine; a passthrough one is not.
        let lean = Launch { mode: "lean".into(), args: vec!["run".into()], env: BTreeMap::new() };
        let body = serde_json::to_vec(&lean).unwrap();
        let mut framed = (body.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(&body);
        sendmsg::<()>(a.as_raw_fd(), &[IoSlice::new(&framed)], &[], MsgFlags::empty(), None).unwrap();
        let (got, fd) = receive_launch(&b).unwrap();
        assert_eq!(got, lean);
        assert!(fd.is_none());
        let pt = Launch { mode: "passthrough".into(), args: vec![], env: BTreeMap::new() };
        let body = serde_json::to_vec(&pt).unwrap();
        let mut framed = (body.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(&body);
        sendmsg::<()>(a.as_raw_fd(), &[IoSlice::new(&framed)], &[], MsgFlags::empty(), None).unwrap();
        assert!(receive_launch(&b).unwrap_err().contains("no fd"));
    }
}
