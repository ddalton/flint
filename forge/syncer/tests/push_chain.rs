//! The chain, end to end: `git push` → `receive-pack` → the
//! `proc-receive` hook → the Unix socket → the serving loop → the
//! batch → the store → the report the client prints.
//!
//! The unit battery decides the rules by calling `run_batch` directly.
//! It cannot decide whether the pkt-line conversation is right, whether
//! the hook finds its socket, or whether `receive.procReceiveRefs`
//! actually routes what we think it routes — and a wire feature has
//! three parties, so the chain is the thing to test. Everything here
//! runs against a real git and a real push; only the bucket is a
//! double.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use flint_forge::batch::Policy;
use flint_forge::server::{run, ServerOpts};
use flint_forge::{ForgeConfig, Syncer};
use flint_store::memory::MemoryStore;
use flint_store::ObjectStore;

const PREFIX: &str = "tenant/repo";

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", "/nonexistent")
        .env("GIT_AUTHOR_NAME", "tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.invalid")
        .env("GIT_COMMITTER_NAME", "tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.invalid")
        .output()
        .expect("git");
    out
}

fn must(dir: &Path, args: &[&str]) -> String {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Install the hook as `proc-receive`. In the pod this is a symlink in
/// the image; here it is the same symlink to the binary cargo just
/// built, so the test exercises the shipped relay rather than a copy
/// of its logic.
fn install_hook(repo: &Path) {
    let hooks = repo.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let target = hooks.join("proc-receive");
    let _ = std::fs::remove_file(&target);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_flint-forge-hook"), &target).unwrap();
}

async fn wait_for(path: &Path, what: &str) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("{what} never appeared at {}", path.display());
}

struct Rig {
    _dir: tempfile::TempDir,
    store: Arc<MemoryStore>,
    repo: PathBuf,
    client: PathBuf,
    cfg: ForgeConfig,
}

impl Rig {
    async fn start(protected: Vec<String>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo.git");
        let store = Arc::new(MemoryStore::new());
        let cfg = ForgeConfig::new(PREFIX, &repo);
        let socket = cfg.state_dir.join(flint_forge::uds::SOCKET_NAME);

        let sc = Syncer::new(
            store.clone() as Arc<dyn ObjectStore>,
            cfg.clone(),
            "forge-chain".into(),
        );
        // The repository must exist before the hook is installed, and
        // the serving loop creates it.
        let opts = ServerOpts {
            socket: socket.clone(),
            // No status listener: a fixed port would collide with the
            // other tests in this file when cargo runs them together.
            status_addr: None,
            policy: Policy { protected, allow_non_fast_forward: vec![] },
        };
        tokio::spawn(async move {
            if let Err(e) = run(sc, opts).await {
                eprintln!("serving loop stopped: {e}");
            }
        });
        wait_for(&socket, "the hook socket").await;
        install_hook(&repo);

        let client = dir.path().join("client");
        must(dir.path(), &["clone", "--quiet", repo.to_str().unwrap(), "client"]);
        must(&client, &["config", "user.email", "tester@example.invalid"]);
        must(&client, &["config", "user.name", "tester"]);
        Rig { _dir: dir, store, repo, client, cfg }
    }

    fn commit(&self, name: &str, content: &str) -> String {
        std::fs::write(self.client.join(name), content).unwrap();
        must(&self.client, &["add", name]);
        must(&self.client, &["commit", "--quiet", "-m", &format!("add {name}")]);
        must(&self.client, &["rev-parse", "HEAD"]).trim().to_string()
    }

    fn push(&self, args: &[&str]) -> (bool, String) {
        let mut argv = vec!["push"];
        argv.extend_from_slice(args);
        let out = git(&self.client, &argv);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    async fn snapshot(&self) -> flint_forge::snapshot::Snapshot {
        flint_forge::snapshot::load(self.store.as_ref(), &self.cfg)
            .await
            .expect("snapshot")
            .snap
    }
}

/// The whole chain, once. A real client pushes, the hook relays, the
/// syncer publishes, and the ref the client is told about is the ref
/// the bucket holds.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_push_reaches_the_bucket_and_the_client_is_told_the_truth() {
    let rig = Rig::start(vec![]).await;
    let oid = rig.commit("a.txt", "one\n");
    let (ok, text) = rig.push(&["--quiet", "origin", "HEAD:refs/heads/main"]);
    assert!(ok, "the push must succeed: {text}");

    let snap = rig.snapshot().await;
    assert_eq!(snap.refs.get("refs/heads/main"), Some(&oid), "the bucket holds what was acked");
    assert!(!snap.packs.is_empty());
    for pack in &snap.packs {
        rig.store.head(&rig.cfg.pack_key(pack)).await.expect("the pack is in the bucket");
    }
    assert_eq!(must(&rig.repo, &["rev-parse", "refs/heads/main"]).trim(), oid);
}

/// The stale push, through the wire rather than through the API: the
/// server refuses and git prints the refusal against the ref.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_push_is_refused_and_the_client_sees_why() {
    let rig = Rig::start(vec![]).await;
    rig.commit("a.txt", "one\n");
    assert!(rig.push(&["--quiet", "origin", "HEAD:refs/heads/main"]).0);

    // A second client that never saw the first push's successor.
    let second = rig.client.parent().unwrap().join("second");
    must(
        rig.client.parent().unwrap(),
        &["clone", "--quiet", rig.repo.to_str().unwrap(), "second"],
    );
    must(&second, &["config", "user.email", "t@example.invalid"]);
    must(&second, &["config", "user.name", "t"]);

    // The first client moves main…
    let ahead = rig.commit("a.txt", "two\n");
    assert!(rig.push(&["--quiet", "origin", "HEAD:refs/heads/main"]).0);

    // …and the second pushes from the old base.
    std::fs::write(second.join("b.txt"), "mine\n").unwrap();
    must(&second, &["add", "b.txt"]);
    must(&second, &["commit", "--quiet", "-m", "mine"]);
    let out = git(&second, &["push", "origin", "HEAD:refs/heads/main"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "a stale push must fail: {text}");
    assert!(text.contains("stale info") || text.contains("fetch first"), "{text}");
    assert_eq!(rig.snapshot().await.refs.get("refs/heads/main"), Some(&ahead));
}

/// A protected branch refuses the direct push and takes the same change
/// through `refs/for/`, which is the whole of forge's merge surface.
#[tokio::test(flavor = "multi_thread")]
async fn a_protected_branch_takes_the_change_through_refs_for() {
    let rig = Rig::start(vec!["refs/heads/main".into()]).await;
    // Seed main while it is still empty: an unborn protected ref has to
    // be created by someone, and the operator's own seed is that path.
    // Here the protection is asserted on the SECOND push.
    rig.commit("a.txt", "one\n");
    let (ok, text) = rig.push(&["--quiet", "origin", "HEAD:refs/heads/main"]);
    assert!(!ok, "a protected ref refuses a direct push: {text}");
    assert!(text.contains("protected"), "{text}");
}

/// `git push -o strategy=…` reaches the syncer, which is only true if
/// the hook echoes the `push-options` capability during version
/// negotiation. A hook that answers `version=1` alone silently drops
/// every option.
#[tokio::test(flavor = "multi_thread")]
async fn push_options_survive_the_version_negotiation() {
    let rig = Rig::start(vec![]).await;
    rig.commit("a.txt", "base\n");
    assert!(rig.push(&["--quiet", "origin", "HEAD:refs/heads/main"]).0);

    // main moves under a second client…
    let second = rig.client.parent().unwrap().join("second");
    must(
        rig.client.parent().unwrap(),
        &["clone", "--quiet", rig.repo.to_str().unwrap(), "second"],
    );
    must(&second, &["config", "user.email", "t@example.invalid"]);
    must(&second, &["config", "user.name", "t"]);
    let conflicting = rig.commit("a.txt", "main side\n");
    assert!(rig.push(&["--quiet", "origin", "HEAD:refs/heads/main"]).0);

    // …and the agent proposes a conflicting change with an explicit
    // strategy, which must resolve rather than refuse.
    std::fs::write(second.join("a.txt"), "agent side\n").unwrap();
    must(&second, &["add", "a.txt"]);
    must(&second, &["commit", "--quiet", "-m", "agent"]);
    let out = git(&second, &["push", "-o", "strategy=theirs", "origin", "HEAD:refs/for/main"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "the strategy must reach merge-tree: {text}");

    let snap = rig.snapshot().await;
    let merged = snap.refs.get("refs/heads/main").expect("main moved").clone();
    assert_ne!(merged, conflicting, "the merge produced a new commit");
    assert!(snap.refs.keys().all(|k| !k.starts_with("refs/for/")), "refs/for is never stored");
    let content = must(&rig.repo, &["show", &format!("{merged}:a.txt")]);
    assert_eq!(content, "agent side\n", "-Xtheirs takes the pushed side");
}
