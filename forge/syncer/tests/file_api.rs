//! The file API, end to end: HTTP → the listener → the serving loop →
//! the batch → the store.
//!
//! The unit battery decides the rules by calling `fileapi` directly. It
//! cannot decide whether the wire carries them: whether the query
//! parses, whether `If-Match` survives a header round trip, whether the
//! verdict a browser sees is the verdict the batch reached — and, above
//! all, **what happens when several people save at once**. That last is
//! the thing this file exists for. Everything here runs against a real
//! git, a real TCP listener and the real serving loop; only the bucket
//! is a double.

use std::sync::Arc;

use flint_forge::policy::Policy;
use flint_forge::server::{run, FileApiOpts, ServerOpts};
use flint_forge::{ForgeConfig, Syncer};
use flint_store::memory::MemoryStore;
use flint_store::ObjectStore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PREFIX: &str = "tenant/repo";
const BRANCH: &str = "refs/heads/main";
struct Rig {
    _dir: tempfile::TempDir,
    addr: String,
    #[allow(dead_code)]
    store: Arc<MemoryStore>,
    repo: std::path::PathBuf,
    socket: std::path::PathBuf,
    /// A working clone, present only when the rig was started with the
    /// hook installed. The two-door legs need it; the rest do not pay
    /// for a clone they never push.
    client: Option<std::path::PathBuf>,
    token: String,
}

fn git_in(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", "/nonexistent")
        .env("GIT_AUTHOR_NAME", "pusher")
        .env("GIT_AUTHOR_EMAIL", "pusher@example.invalid")
        .env("GIT_COMMITTER_NAME", "pusher")
        .env("GIT_COMMITTER_EMAIL", "pusher@example.invalid")
        // What the door sets, and what the hooks read.
        .env("REMOTE_USER", "system:serviceaccount:apps:agent")
        .output()
        .expect("git")
}

fn must_git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = git_in(dir, args);
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn install_hook(repo: &std::path::Path) {
    let hooks = repo.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    for name in ["proc-receive", "pre-receive"] {
        let target = hooks.join(name);
        let _ = std::fs::remove_file(&target);
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_flint-forge-hook"), &target).unwrap();
    }
}

/// A whole HTTP response, parsed enough to assert on.
#[derive(Debug)]
struct Res {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Res {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
    fn etag(&self) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("etag"))
            .map(|(_, v)| v.trim_matches('"').to_string())
    }
    fn reason(&self) -> String {
        self.json().get("reason").and_then(|v| v.as_str()).unwrap_or("").to_string()
    }
}

/// A minimal HTTP/1.1 client. Deliberately hand-rolled: adding a client
/// dependency to this crate to test its own server would put a second
/// implementation of the thing under test into the build.
async fn request(
    addr: &str,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Res {
    let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: x\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    s.write_all(head.as_bytes()).await.expect("write head");
    // A server that refuses on Content-Length answers and closes
    // WITHOUT draining the body, which is correct — it is how an
    // oversized upload is refused before it is allocated. The client
    // then sees a broken pipe, so the write is best-effort and the
    // response is what matters.
    let _ = s.write_all(body).await;
    let _ = s.flush().await;

    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.expect("read");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a response with headers");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = raw[split + 4..].to_vec();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect();
    Res { status, headers, body }
}

/// The authenticated caller the door would produce: a verified
/// principal, and an end user in the author header.
fn auth<'a>(bearer: &'a str, user: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("Authorization", bearer),
        ("X-Remote-User", "system:serviceaccount:apps:browser"),
        ("X-Flint-Author", user),
    ]
}

/// A bearer unique to this rig.
///
/// It is the rig's identity check: if a rig ever reached a sibling's
/// listener the token would not match and the probe would answer 401
/// rather than the test quietly measuring the wrong process.
///
/// An earlier version derived it from the PORT — so two rigs that
/// collided on a port also shared a token and the check could never
/// fire. That, with a port allocator whose retry range overlapped its
/// own stride, made a connection reset appear about one run in three:
/// 0/15 alone, 0/10 under `--test-threads=1`, 6/15 in parallel. The
/// port is no longer guessed at all (the server reports what it bound),
/// and this no longer depends on it.
fn rig_token() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "rig-{}-{}-at-least-16-bytes",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

impl Rig {
    async fn start() -> Rig {
        Rig::start_with_git(false).await
    }

    /// `with_git` installs the hooks and makes a clone, so the same
    /// repository can be driven through BOTH doors at once — which is
    /// the interaction neither door's own tests can reach.
    async fn start_with_git(with_git: bool) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo.git");
        let store = Arc::new(MemoryStore::new());
        let cfg = ForgeConfig::new(PREFIX, &repo);
        let socket = cfg.state_dir.join(flint_forge::uds::SOCKET_NAME);
        // Port 0: the kernel picks, the server reports, nothing races.
        let token = rig_token();
        let bearer = format!("Bearer {token}");
        let bound: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));

        let sc = Syncer::new(
            store.clone() as Arc<dyn ObjectStore>,
            cfg.clone(),
            "forge-fileapi".into(),
        );
        let opts = ServerOpts {
            socket: socket.clone(),
            policy_dir: None,
            status_addr: None,
            policy: Policy::default(),
            export: None,
            bundle: None,
            prune: None,
            lfs: None,
            file_api: Some(FileApiOpts {
                addr: "127.0.0.1:0".into(),
                bound: Some(bound.clone()),
                branch: BRANCH.into(),
                cap: 1 << 20,
                token: Some(token.clone()),
            }),
        };
        tokio::spawn(async move {
            if let Err(e) = run(sc, opts).await {
                eprintln!("serving loop stopped: {e}");
            }
        });

        // Two waits, and they are different things. First the server
        // has to report where it bound; then the readiness gate has to
        // stop answering 503, because it refuses until the loop is
        // Serving. A test that raced either would be flaky in the
        // direction that reads as a product bug.
        let mut addr = String::new();
        for _ in 0..600 {
            if let Some(a) = bound.lock().ok().and_then(|g| g.clone()) {
                addr = a;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(!addr.is_empty(), "the file API never reported a bound address");

        for _ in 0..600 {
            let r = request(&addr, "GET", "/files?path=/", &auth(&bearer, "probe"), b"").await;
            assert_ne!(
                r.status, 401,
                "this rig reached a listener that is not its own"
            );
            if r.status != 503 {
                let client = if with_git {
                    install_hook(&repo);
                    let c = dir.path().join("client");
                    must_git(dir.path(), &["clone", "--quiet", repo.to_str().unwrap(), "client"]);
                    must_git(&c, &["config", "user.email", "pusher@example.invalid"]);
                    must_git(&c, &["config", "user.name", "pusher"]);
                    Some(c)
                } else {
                    None
                };
                return Rig { _dir: dir, addr, store, repo, socket, client, token };
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the file API never began serving");
    }

    fn client(&self) -> &std::path::Path {
        self.client.as_deref().expect("this rig was started with git")
    }

    /// Commit a file in the clone and push it. Returns whether the push
    /// was accepted, with git's own output for the failure message.
    fn git_push(&self, name: &str, content: &str) -> (bool, String) {
        let c = self.client();
        std::fs::write(c.join(name), content).unwrap();
        must_git(c, &["add", name]);
        must_git(c, &["commit", "--quiet", "-m", &format!("add {name}")]);
        let out = git_in(c, &["push", "origin", "HEAD:main"]);
        (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    async fn put(&self, user: &str, path: &str, body: &[u8], if_match: Option<&str>) -> Res {
        let b = self.bearer();
        let mut h = auth(&b, user);
        if let Some(t) = if_match {
            h.push(("If-Match", t));
        }
        request(&self.addr, "PUT", &format!("/files/content?path={path}"), &h, body).await
    }

    async fn get(&self, path: &str) -> Res {
        let b = self.bearer();
        request(&self.addr, "GET", &format!("/files/content?path={path}"), &auth(&b, "reader"), b"")
            .await
    }

    async fn list(&self, path: &str) -> Res {
        let b = self.bearer();
        request(&self.addr, "GET", &format!("/files?path={path}"), &auth(&b, "reader"), b"").await
    }

    /// A request with this rig's bearer and arbitrary extra headers.
    async fn req(&self, method: &str, target: &str, extra: &[(&str, &str)], body: &[u8]) -> Res {
        let b = self.bearer();
        let mut h = auth(&b, "reader");
        h.extend_from_slice(extra);
        request(&self.addr, method, target, &h, body).await
    }

    /// What git itself says is on the branch — the oracle. Asserting
    /// against the API's own answers would let a write that never
    /// happened agree with a read that never looked.
    fn on_branch(&self) -> Vec<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["ls-tree", "-r", "--name-only", BRANCH])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", "/nonexistent")
            .output()
            .expect("git");
        String::from_utf8_lossy(&out.stdout).lines().map(|s| s.to_string()).collect()
    }

    fn commit_count(&self) -> usize {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["rev-list", "--count", BRANCH])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", "/nonexistent")
            .output()
            .expect("git");
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
    }
}

/// The whole surface once, over the wire.
#[tokio::test]
async fn the_verbs_work_end_to_end() {
    let rig = Rig::start().await;

    // Create.
    let r = rig.put("ada", "notes.md", b"# hello\n", None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let etag = r.etag().expect("an ETag on the write");

    // Read it back, byte for byte.
    let g = rig.get("notes.md").await;
    assert_eq!(g.status, 200);
    assert_eq!(g.body, b"# hello\n");
    assert_eq!(g.etag().as_deref(), Some(etag.as_str()), "the read agrees with the write");

    // List.
    let l = rig.list("/").await;
    assert_eq!(l.status, 200);
    let doc = l.json();
    let names: Vec<&str> = doc["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["path"].as_str().expect("path"))
        .collect();
    assert_eq!(names, vec!["notes.md"]);

    // Move.
    let m = rig
        .req("POST", "/files/move", &[], br#"{"from":"notes.md","to":"docs/notes.md"}"#)
        .await;
    assert_eq!(m.status, 200, "{}", m.text());
    assert_eq!(rig.on_branch(), vec!["docs/notes.md"]);

    // Delete.
    let cur = rig.get("docs/notes.md").await.etag().unwrap();
    let d = rig
        .req("DELETE", "/files/content?path=docs/notes.md", &[("If-Match", &cur)], b"")
        .await;
    assert_eq!(d.status, 200, "{}", d.text());
    assert!(rig.on_branch().is_empty(), "{:?}", rig.on_branch());

    // The verb git cannot have, refused with a reason a UI can render.
    let f = rig.req("POST", "/files/folder", &[], br#"{"path":"/x"}"#).await;
    assert_eq!(f.status, 501);
    assert_eq!(f.reason(), "no-empty-directories");
}

/// **The leg this file exists for.** Many people saving DIFFERENT files
/// at the same moment must all land — none lost, none refused for a
/// conflict that is not theirs.
///
/// The oracle is git, not the API: `ls-tree` on the branch. An
/// assertion against the API's own reads could pass while nothing was
/// ever written.
#[tokio::test]
async fn concurrent_writers_to_different_files_all_land() {
    let rig = Arc::new(Rig::start().await);
    const N: usize = 16;

    let mut tasks = Vec::new();
    for i in 0..N {
        let rig = rig.clone();
        tasks.push(tokio::spawn(async move {
            let user = format!("user{i}");
            let body = format!("body {i}\n");
            rig.put(&user, &format!("f{i:02}.txt"), body.as_bytes(), None).await
        }));
    }

    let mut ok = 0;
    for (i, t) in tasks.into_iter().enumerate() {
        let r = t.await.expect("join");
        assert_eq!(r.status, 200, "writer {i} was refused: {} {}", r.status, r.text());
        ok += 1;
    }
    assert_eq!(ok, N);

    let on_branch = rig.on_branch();
    assert_eq!(on_branch.len(), N, "every acknowledged write must be on the branch: {on_branch:?}");
    for i in 0..N {
        let want = format!("f{i:02}.txt");
        assert!(on_branch.contains(&want), "{want} was acknowledged and is not there");
    }

    // Each writer keeps their own commit and their own name: a burst of
    // saves must not be squashed into one commit with one author on it.
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&rig.repo)
        .args(["log", "--format=%an", BRANCH])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", "/nonexistent")
        .output()
        .expect("git");
    let mut authors: Vec<String> =
        String::from_utf8_lossy(&out.stdout).lines().map(|s| s.to_string()).collect();
    authors.sort();
    authors.dedup();
    assert_eq!(authors.len(), N, "each writer must be the author of their own commit: {authors:?}");
}

/// Many people saving the SAME file. Exactly one may win per version,
/// and the losers must be told their own file changed — never silently
/// overwritten, and never told about somebody else's contention.
#[tokio::test]
async fn concurrent_writers_to_one_file_lose_nothing_silently() {
    let rig = Arc::new(Rig::start().await);
    // The seed's content must differ from EVERY writer's, or one of
    // them writes bytes that are already there — a genuine no-op that
    // answers 200 without committing, which would read here as a second
    // winner. An earlier draft of this test seeded with `v0` and
    // writer 0 sent `v0`; it failed about one run in eight and the
    // failure looked exactly like a lost update.
    let first = rig.put("ada", "shared.txt", b"seed\n", None).await;
    assert_eq!(first.status, 200);
    let base = first.etag().unwrap();

    const N: usize = 12;
    let mut tasks = Vec::new();
    for i in 0..N {
        let rig = rig.clone();
        let tag = base.clone();
        tasks.push(tokio::spawn(async move {
            let body = format!("v{i}\n");
            rig.put(&format!("user{i}"), "shared.txt", body.as_bytes(), Some(&tag)).await
        }));
    }

    let mut winners = 0;
    let mut losers = 0;
    for t in tasks {
        let r = t.await.expect("join");
        match r.status {
            200 => winners += 1,
            412 => {
                assert_eq!(r.reason(), "file-changed", "the loser must be told about THEIR file");
                losers += 1;
            }
            other => panic!("unexpected {other}: {}", r.text()),
        }
    }
    assert_eq!(winners, 1, "exactly one write may win against one version");
    assert_eq!(losers, N - 1);

    // And the survivor is one of the bodies actually sent — not a
    // mixture, and not the base.
    let g = rig.get("shared.txt").await;
    let text = g.text();
    assert!(text.starts_with('v'), "the winner's bytes stand, whole: {text:?}");

    // One version, one commit past the seed. If two had landed against
    // one etag, this would be 3.
    assert_eq!(rig.commit_count(), 2, "exactly one write moved the branch");
}

/// An unconditioned overwrite is refused. Without this, two browser
/// tabs silently destroy each other's work and neither is told — which
/// is the failure a file manager cannot afford and cannot detect.
#[tokio::test]
async fn an_unconditioned_overwrite_is_refused_over_the_wire() {
    let rig = Rig::start().await;
    assert_eq!(rig.put("ada", "a.txt", b"one\n", None).await.status, 200);

    let bare = rig.put("bob", "a.txt", b"two\n", None).await;
    assert_eq!(bare.status, 428, "{}", bare.text());
    assert_eq!(bare.reason(), "precondition-required");
    assert_eq!(rig.get("a.txt").await.body, b"one\n", "and nothing was overwritten");

    let stale = rig.put("bob", "a.txt", b"two\n", Some("0000000000000000000000000000000000000000")).await;
    assert_eq!(stale.status, 412);
    assert_eq!(rig.get("a.txt").await.body, b"one\n");
}

/// Authentication runs BEFORE routing, so an unauthenticated caller
/// cannot map the surface or read the phase off a readiness answer.
#[tokio::test]
async fn authentication_precedes_routing() {
    let rig = Rig::start().await;
    let good = rig.bearer();
    let cases: Vec<(Vec<(&str, &str)>, u16)> = vec![
        (vec![], 401),
        (vec![("Authorization", "Bearer wrong-token-but-long-enough")], 401),
        // Authenticated, but with no door-verified identity.
        (vec![("Authorization", good.as_str())], 403),
    ];
    for (headers, want) in &cases {
        for target in ["/files?path=/", "/nonexistent-route", "/files/content?path=x"] {
            let r = request(&rig.addr, "GET", target, headers, b"").await;
            assert_eq!(
                r.status, *want,
                "{target} with {headers:?} answered {} — an unauthenticated caller must not \
                 be able to tell one route from another",
                r.status
            );
        }
    }
}

/// The size cap is a memory bound, and it holds over the wire in both
/// directions.
#[tokio::test]
async fn the_cap_holds_over_the_wire() {
    let rig = Rig::start().await;
    let big = vec![b'z'; (1 << 20) + 1];
    let r = rig.put("ada", "big.bin", &big, None).await;
    assert_eq!(r.status, 413, "{}", r.text());
    assert_eq!(r.reason(), "too-large");
    assert!(rig.on_branch().is_empty(), "nothing was written");
}

/// Saving a buffer that has not changed must not create history. The
/// write is answered 200 with the current ETag and no commit is made —
/// a file manager whose editor autosaves would otherwise fill the log
/// with empty commits.
///
/// This is also the shape that made an earlier version of
/// `concurrent_writers_to_one_file_lose_nothing_silently` fail
/// intermittently, so it is worth a test of its own rather than a
/// comment.
#[tokio::test]
async fn rewriting_identical_content_is_not_a_commit() {
    let rig = Rig::start().await;
    let a = rig.put("ada", "same.txt", b"unchanged\n", None).await;
    assert_eq!(a.status, 200);
    let before = rig.commit_count();
    let etag = a.etag().unwrap();

    let b = rig.put("ada", "same.txt", b"unchanged\n", Some(&etag)).await;
    assert_eq!(b.status, 200, "{}", b.text());
    assert_eq!(b.etag().as_deref(), Some(etag.as_str()), "the version did not move");
    assert_eq!(rig.commit_count(), before, "an unchanged save must add no commit");

    // A stale condition is still refused, no-op or not: the check runs
    // before the content is compared.
    let stale = rig
        .put("bob", "same.txt", b"unchanged\n", Some("0000000000000000000000000000000000000000"))
        .await;
    assert_eq!(stale.status, 412);
}

/// **Both doors at once.** An agent pushing with a real git client and
/// a browser writing through the file API, against one repository, at
/// the same moment.
///
/// This is the interaction neither door's own tests can reach, and it
/// is the one the deployment actually has: agents use git, people use
/// the app. Both paths end in the same batch and the same single CAS,
/// so what is under test is whether that shared path keeps its
/// promises when two different kinds of client arrive together.
///
/// What must hold, and what deliberately need not:
///
/// - **Nothing is lost or corrupted.** Whatever is acknowledged is on
///   the branch afterwards.
/// - **The HTTP write always lands.** It is planned inside the serving
///   loop against the tip it will commit onto, so it cannot go stale.
/// - **The push may be refused** — if the file API moved the branch
///   first, the client's commit is no longer a fast-forward. That is
///   ordinary git, not a fault, and the agent's answer is to fetch and
///   push again. The test does exactly that and requires it to work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_git_push_and_two_http_clients_share_one_repository() {
    let rig = Arc::new(Rig::start_with_git(true).await);

    // Seed through git so the branch exists for both doors.
    let (ok, out) = rig.git_push("seed.txt", "seed\n");
    assert!(ok, "the seed push must land: {out}");

    // Three clients, at once: two people in the browser and one agent
    // pushing. The two HTTP writes are the pair that must coalesce into
    // a single ref movement; the push is the one that may be refused.
    let ada = {
        let rig = rig.clone();
        tokio::spawn(async move { rig.put("ada", "from-ada.txt", b"ada typed\n", None).await })
    };
    let bob = {
        let rig = rig.clone();
        tokio::spawn(async move { rig.put("bob", "from-bob.txt", b"bob typed\n", None).await })
    };
    let push = {
        let rig = rig.clone();
        tokio::task::spawn_blocking(move || rig.git_push("from-git.txt", "committed\n"))
    };

    let ada = ada.await.expect("join");
    let bob = bob.await.expect("join");
    let (pushed, push_out) = push.await.expect("join");

    assert_eq!(ada.status, 200, "ada's write must land: {}", ada.text());
    assert_eq!(bob.status, 200, "bob's write must land: {}", bob.text());
    assert_ne!(ada.etag(), bob.etag(), "two different files, two different versions");

    eprintln!("RACE pushed={pushed}");
    if !pushed {
        // Refused because the file API moved the branch first. The
        // agent's recovery is a fetch and a re-push, and it must work
        // — a repository that cannot be pushed to after a UI write
        // would be unusable for the agents.
        assert!(
            push_out.contains("fetch first")
                || push_out.contains("non-fast-forward")
                || push_out.contains("stale info"),
            "a refused push must say why, and say something a git client understands: {push_out}"
        );
        let c = rig.client();
        must_git(c, &["fetch", "--quiet", "origin"]);
        must_git(c, &["rebase", "--quiet", "origin/main"]);
        let out = git_in(c, &["push", "origin", "HEAD:main"]);
        assert!(
            out.status.success(),
            "the retry after a fetch must land: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Whatever route each took, both writes are on the branch and
    // neither clobbered the other.
    let on_branch = rig.on_branch();
    for want in ["seed.txt", "from-git.txt", "from-ada.txt", "from-bob.txt"] {
        assert!(on_branch.contains(&want.to_string()), "{want} missing from {on_branch:?}");
    }

    // And each door's content is intact — read back through the OTHER
    // door than the one that wrote it, which is the point of having one
    // repository behind both.
    assert_eq!(rig.get("from-git.txt").await.body, b"committed\n", "git's write reads over HTTP");
    let via_git = must_git(rig.client(), &["show", "origin/main:from-ada.txt"]);
    assert_eq!(via_git, "ada typed\n", "an HTTP write is a real commit a git client can see");

    // Each browser user authored their own commit. Two people saving at
    // the same moment must not be collapsed into one name.
    let log = must_git(rig.client(), &["log", "--format=%an", "origin/main"]);
    for who in ["ada", "bob"] {
        assert!(log.lines().any(|l| l == who), "{who} is not an author in:\n{log}");
    }
}

/// Sustained traffic on both doors. Ten HTTP writes and three pushes
/// interleaved, then a full accounting: every acknowledged write is
/// present, and the branch is one connected history rather than two
/// that raced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_doors_under_sustained_traffic_keep_one_history() {
    let rig = Arc::new(Rig::start_with_git(true).await);
    let (ok, out) = rig.git_push("seed.txt", "seed\n");
    assert!(ok, "{out}");

    let mut http = Vec::new();
    for i in 0..10 {
        let rig = rig.clone();
        http.push(tokio::spawn(async move {
            let name = format!("h{i:02}.txt");
            let r = rig.put(&format!("user{i}"), &name, b"x\n", None).await;
            (name, r.status)
        }));
    }
    let pushes = {
        let rig = rig.clone();
        tokio::task::spawn_blocking(move || {
            let mut landed = Vec::new();
            for i in 0..3 {
                let name = format!("g{i}.txt");
                let (mut ok, mut why) = rig.git_push(&name, "y\n");
                // Retry until it wins. Not just for tidiness: a git
                // push is CUMULATIVE, so a file whose own push lost the
                // race still rides along with the next one — which made
                // per-file bookkeeping wrong in both directions. It is
                // also the property an agent needs: no amount of UI
                // traffic may permanently lock a pusher out.
                for _ in 0..8 {
                    if ok {
                        break;
                    }
                    let c = rig.client();
                    must_git(c, &["fetch", "--quiet", "origin"]);
                    must_git(c, &["rebase", "--quiet", "origin/main"]);
                    let out = git_in(c, &["push", "origin", "HEAD:main"]);
                    ok = out.status.success();
                    why = String::from_utf8_lossy(&out.stderr).into_owned();
                }
                assert!(ok, "a git client must eventually get through: {why}");
                landed.push(name);
            }
            landed
        })
    };

    let mut expected: Vec<String> = vec!["seed.txt".into()];
    for t in http {
        let (name, status) = t.await.expect("join");
        assert_eq!(status, 200, "{name} was refused");
        expected.push(name);
    }
    expected.extend(pushes.await.expect("join"));

    let on_branch = rig.on_branch();
    for want in &expected {
        assert!(on_branch.contains(want), "{want} was acknowledged and is not on the branch");
    }
    assert_eq!(
        on_branch.len(),
        expected.len(),
        "the branch holds exactly what was acknowledged: {on_branch:?} vs {expected:?}"
    );

    // One history: every commit reachable from the tip, no forks left
    // behind. `rev-list --count` walking to the root proves the chain
    // is connected rather than merely that the files are present.
    assert!(rig.commit_count() >= expected.len(), "one connected history, not two");
}

/// A condition on a move must be HONOURED, not accepted and dropped.
///
/// This test exists because it did not, for a while: an edit meant to
/// make `If-Match` optional on a rename deleted the check instead, and
/// every test in the suite still passed — because they all sent a
/// valid etag. The compiler's unused-variable warning is what caught
/// it. A stale condition is the only thing that can tell the
/// difference, so that is what this sends.
#[tokio::test]
async fn a_stale_condition_on_a_move_is_refused() {
    let rig = Rig::start().await;
    assert_eq!(rig.put("ada", "old.txt", b"body\n", None).await.status, 200);

    let stale = rig
        .req(
            "POST",
            "/files/move",
            &[("If-Match", "0000000000000000000000000000000000000000")],
            br#"{"from":"old.txt","to":"new.txt"}"#,
        )
        .await;
    assert_eq!(stale.status, 412, "a stale condition must refuse the move: {}", stale.text());
    assert_eq!(stale.reason(), "file-changed");
    assert_eq!(rig.on_branch(), vec!["old.txt"], "and nothing moved");

    // With no condition at all it is allowed: a rename destroys no
    // content, so it does not require one.
    let ok = rig.req("POST", "/files/move", &[], br#"{"from":"old.txt","to":"new.txt"}"#).await;
    assert_eq!(ok.status, 200, "{}", ok.text());
    assert_eq!(rig.on_branch(), vec!["new.txt"]);
}
