//! `flint-forge-gitcgi` against real git over real HTTP: the smart
//! protocol round trip, and the property the two processes it replaces
//! did not have — a hook's sideband output reaches the client AS IT IS
//! WRITTEN. fcgiwrap held the CGI's output until the request ended
//! unless a patch-specific parameter was set, and the runbx drill paid
//! for that with a 40 GiB push cut 311 s into its hook wait. The timing
//! test here is that defect's oracle: six `remote:` lines a second
//! apart must arrive a second apart, not together at the end.
#![cfg(feature = "gitcgi")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    port: u16,
    root: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}{}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare repository under a fresh project root, and the runner on an
/// ephemeral port with the container's environment.
fn serve(extra_env: &[(&str, &str)]) -> Server {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo.git");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--bare", "-b", "main"]);
    let port = free_port();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_flint-forge-gitcgi"));
    cmd.env("GIT_PROJECT_ROOT", root.path())
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("FLINT_FORGE_GIT_LISTEN", format!("127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("spawn gitcgi");
    let t0 = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(t0.elapsed() < Duration::from_secs(10), "the runner never listened");
        std::thread::sleep(Duration::from_millis(50));
    }
    Server { child, port, root }
}

fn url(s: &Server) -> String {
    format!("http://127.0.0.1:{}/repo.git", s.port)
}

/// A working clone with an identity and the door's header, as the
/// agent image's git would send it.
fn clone_into(s: &Server, name: &str) -> PathBuf {
    let dir = s.root.path().join(name);
    git(
        s.root.path(),
        &["-c", "http.extraHeader=X-Remote-User: tester", "-c", "protocol.version=2", "clone", "-q", &url(s), dir.to_str().unwrap()],
    );
    git(&dir, &["config", "user.email", "t@example"]);
    git(&dir, &["config", "user.name", "tester"]);
    git(&dir, &["config", "http.extraHeader", "X-Remote-User: tester"]);
    git(&dir, &["config", "protocol.version", "2"]);
    dir
}

#[test]
fn clone_push_and_fetch_round_trip_over_the_runner() {
    let s = serve(&[]);
    let a = clone_into(&s, "a");
    std::fs::write(a.join("f.txt"), "one\n").unwrap();
    git(&a, &["add", "f.txt"]);
    git(&a, &["commit", "-q", "-m", "first"]);
    git(&a, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    let tip = git(&a, &["rev-parse", "HEAD"]);

    // A second clone sees it; a second push from it lands too.
    let b = clone_into(&s, "b");
    assert_eq!(git(&b, &["rev-parse", "HEAD"]), tip);
    std::fs::write(b.join("g.txt"), "two\n").unwrap();
    git(&b, &["add", "g.txt"]);
    git(&b, &["commit", "-q", "-m", "second"]);
    git(&b, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    git(&a, &["fetch", "-q", "origin"]);
    assert_eq!(git(&a, &["rev-parse", "origin/main"]), git(&b, &["rev-parse", "HEAD"]));
    // The bare repository holds both.
    assert_eq!(git(&s.root.path().join("repo.git"), &["rev-parse", "main"]), git(&b, &["rev-parse", "HEAD"]));
}

/// The defect's oracle. A pre-receive hook writes one line a second
/// for six seconds; the client must see the first within three seconds
/// of starting the push, and the lines must be spread over at least
/// four seconds — buffered through fcgiwrap they arrived together with
/// the report, six seconds in.
#[test]
fn a_hooks_sideband_output_reaches_the_client_as_it_is_written() {
    let s = serve(&[]);
    let hooks = s.root.path().join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-receive");
    std::fs::write(&hook, "#!/bin/sh\ncat >/dev/null\nfor i in 1 2 3 4 5 6; do echo \"tick $i\" >&2; sleep 1; done\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(&s.root.path().join("repo.git"), &["config", "core.hooksPath", hooks.to_str().unwrap()]);

    let a = clone_into(&s, "a");
    std::fs::write(a.join("f.txt"), "one\n").unwrap();
    git(&a, &["add", "f.txt"]);
    git(&a, &["commit", "-q", "-m", "first"]);

    let t0 = Instant::now();
    let mut push = Command::new("git")
        .args(["push", "origin", "HEAD:refs/heads/main"])
        .current_dir(&a)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", &a)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = push.stderr.take().unwrap();
    let mut arrivals: Vec<(Duration, String)> = Vec::new();
    for line in BufReader::new(stderr).lines() {
        let line = line.unwrap();
        if line.contains("tick") {
            arrivals.push((t0.elapsed(), line));
        }
    }
    assert!(push.wait().unwrap().success(), "the push must succeed");
    assert_eq!(arrivals.len(), 6, "six ticks: {arrivals:?}");
    let first = arrivals[0].0;
    let last = arrivals[5].0;
    assert!(first < Duration::from_secs(3), "the first tick took {first:?} to arrive: buffered? {arrivals:?}");
    assert!(last - first >= Duration::from_secs(4), "ticks arrived in a burst: {arrivals:?}");
}

/// The LFS batch and verify calls go to the syncer's listener with the
/// door's headers, and the answer comes back with its status.
#[test]
fn lfs_batch_and_verify_are_relayed_to_the_upstream() {
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let up_addr = upstream.local_addr().unwrap();
    let s = serve(&[("FLINT_FORGE_LFS_UPSTREAM", &up_addr.to_string())]);
    let seen = std::thread::spawn(move || {
        let (mut c, _) = upstream.accept().unwrap();
        // The whole request: headers, then Content-Length bytes of body,
        // however the segments fall.
        let mut raw = Vec::new();
        let mut buf = vec![0u8; 4096];
        loop {
            let n = c.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
            if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&raw[..i]).to_string();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if raw.len() >= i + 4 + len {
                    break;
                }
            }
        }
        let req = String::from_utf8_lossy(&raw).to_string();
        let body = r#"{"objects":[]}"#;
        write!(c, "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.git-lfs+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        req
    });
    let mut c = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
    let body = r#"{"operation":"download","objects":[]}"#;
    write!(
        c,
        "POST /repo.git/info/lfs/objects/batch HTTP/1.1\r\nHost: x\r\nX-Remote-User: tester\r\nX-Forge-Lfs-Verify: http://door/verify\r\nContent-Type: application/vnd.git-lfs+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
    let mut resp = String::new();
    c.read_to_string(&mut resp).unwrap();
    let req = seen.join().unwrap();
    assert!(req.starts_with("POST /lfs/objects/batch HTTP/1.1"), "{req}");
    assert!(req.contains("X-Remote-User: tester"), "{req}");
    assert!(req.contains("X-Forge-Lfs-Verify: http://door/verify"), "{req}");
    assert!(req.ends_with(body), "{req}");
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.contains("application/vnd.git-lfs+json"), "{resp}");
    assert!(resp.ends_with(r#"{"objects":[]}"#), "{resp}");
}

/// The ceiling answers 503 at once; it never queues in silence.
#[test]
fn the_concurrency_ceiling_answers_instead_of_queueing() {
    let s = serve(&[("FLINT_FORGE_GIT_CONCURRENCY", "0")]);
    let mut c = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
    write!(c, "GET /repo.git/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut resp = String::new();
    c.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 503"), "{resp}");
}
