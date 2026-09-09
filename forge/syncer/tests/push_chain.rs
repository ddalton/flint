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

use flint_forge::policy::Policy;
use flint_forge::server::{run, ServerOpts};
use flint_forge::{ForgeConfig, Syncer};
use flint_store::memory::MemoryStore;
use flint_store::ObjectStore;

const PREFIX: &str = "tenant/repo";

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    git_as(dir, args, None)
}

/// `REMOTE_USER` is what the door sets on the upstream request, and the
/// hooks read it from their environment. A local push inherits the
/// pusher's environment, so setting it here is the same thing the door
/// does — which is what lets the policy legs run without a door.
fn git_as(dir: &Path, args: &[&str], principal: Option<&str>) -> std::process::Output {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", "/nonexistent")
        .env("GIT_AUTHOR_NAME", "tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.invalid")
        .env("GIT_COMMITTER_NAME", "tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.invalid");
    match principal {
        Some(p) => cmd.env("REMOTE_USER", p),
        None => cmd.env_remove("REMOTE_USER"),
    };
    cmd.output().expect("git")
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
    // Both hooks are the same binary, dispatching on the name it was
    // invoked as — so the symlink is also the test of that dispatch.
    for name in ["proc-receive", "pre-receive"] {
        let target = hooks.join(name);
        let _ = std::fs::remove_file(&target);
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_flint-forge-hook"), &target).unwrap();
    }
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
        Rig::start_with(Policy { protected, ..Policy::default() }, false).await
    }

    /// `render` writes the policy document the hooks read, as the
    /// operator would. Without it only the syncer enforces, which is
    /// the misconfigured-hooks case worth being able to run.
    async fn start_with(policy: Policy, render: bool) -> Rig {
        Rig::start_tuned(policy, render, |_| {}).await
    }

    /// As `start_with`, but the caller may move the compaction knobs
    /// first. The defaults (`fold_min_bytes` 256 MiB, `base_min_bytes`
    /// 64 MiB) mean no fold and no base rebuild ever runs on a
    /// test-sized repository, so a measurement of what compaction
    /// LEAVES BEHIND cannot be taken without this.
    async fn start_tuned(
        policy: Policy,
        render: bool,
        tune: impl FnOnce(&mut ForgeConfig),
    ) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo.git");
        let store = Arc::new(MemoryStore::new());
        let mut cfg = ForgeConfig::new(PREFIX, &repo);
        tune(&mut cfg);
        let cfg = cfg;
        let socket = cfg.state_dir.join(flint_forge::uds::SOCKET_NAME);

        let sc = Syncer::new(
            store.clone() as Arc<dyn ObjectStore>,
            cfg.clone(),
            "forge-chain".into(),
        );
        // The repository must exist before the hook is installed, and
        // the serving loop creates it.
        if render {
            std::fs::create_dir_all(&cfg.state_dir).unwrap();
            std::fs::write(
                cfg.state_dir.join(flint_forge::policy::POLICY_FILE),
                serde_json::to_vec_pretty(&policy).unwrap(),
            )
            .unwrap();
        }
        let opts = ServerOpts {
            socket: socket.clone(),
            // The rendered document lives in the state directory here;
            // in the pod it is a ConfigMap mount, and the re-read is
            // what makes an edit take effect without a roll.
            policy_dir: render.then(|| cfg.state_dir.clone()),
            // No status listener: a fixed port would collide with the
            // other tests in this file when cargo runs them together.
            status_addr: None,
            policy,
            // The chain tests drive pushes, not the export; §9's own
            // legs run in the unit battery against a real git and a
            // real tree.
            export: None,
            bundle: None,
            prune: None,
            lfs: None,
            file_api: None,
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
        self.push_as(None, args)
    }

    fn push_as(&self, principal: Option<&str>, args: &[&str]) -> (bool, String) {
        let mut argv = vec!["push"];
        argv.extend_from_slice(args);
        let out = git_as(&self.client, &argv, principal);
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


/// Falsifier 6, through the wire: an agent's push to `main` is refused
/// by `pre-receive` naming the rule; its push to `agent/<pod>` lands;
/// its push to `refs/for/main` merges because `mergeInto` lists it; and
/// a principal that is not listed is refused and moves no ref.
#[tokio::test(flavor = "multi_thread")]
async fn the_policy_decides_who_moves_main() {
    let policy: Policy = serde_json::from_str(
        r#"{
            "protected": ["main"],
            "pushers": { "main": ["release-bot"] },
            "mergeInto": { "main": ["agent-runner"] },
            "agentPattern": "agent/*"
        }"#,
    )
    .unwrap();
    let rig = Rig::start_with(policy, true).await;

    // The release bot seeds main; the agent cannot.
    rig.commit("a.txt", "base\n");
    let (ok, text) = rig.push_as(Some("agent-runner"), &["origin", "HEAD:refs/heads/main"]);
    assert!(!ok, "an agent must not push main directly: {text}");
    assert!(text.contains("release-bot"), "the refusal names the rule: {text}");

    let (ok, text) = rig.push_as(Some("release-bot"), &["--quiet", "origin", "HEAD:refs/heads/main"]);
    assert!(ok, "the listed pusher moves main: {text}");

    // The agent's own branch lands…
    let (ok, text) =
        rig.push_as(Some("agent-runner"), &["--quiet", "origin", "HEAD:refs/heads/agent/pod-7"]);
    assert!(ok, "an agent pushes its own shape: {text}");

    // …and a branch outside its shape does not.
    let (ok, text) =
        rig.push_as(Some("agent-runner"), &["origin", "HEAD:refs/heads/sneaky"]);
    assert!(!ok, "agentPattern bounds what an agent creates: {text}");
    assert!(text.contains("agent/*"), "{text}");

    // A merge proposal from the listed principal lands…
    let ahead = rig.commit("b.txt", "more\n");
    let (ok, text) = rig.push_as(Some("agent-runner"), &["--quiet", "origin", "HEAD:refs/for/main"]);
    assert!(ok, "the listed merger proposes into main: {text}");
    assert_eq!(rig.snapshot().await.refs.get("refs/heads/main"), Some(&ahead));

    // …and one from an unlisted principal does not, and moves no ref.
    let after = rig.commit("c.txt", "unwanted\n");
    let (ok, text) = rig.push_as(Some("someone-else"), &["origin", "HEAD:refs/for/main"]);
    assert!(!ok, "an unlisted principal may not merge: {text}");
    assert!(text.contains("agent-runner"), "the refusal names who may: {text}");
    let refs = rig.snapshot().await.refs;
    assert_eq!(refs.get("refs/heads/main"), Some(&ahead), "no ref moved");
    assert!(!refs.values().any(|v| v == &after));
}

/// The hooks are not the guarantee. With `pre-receive` removed — a
/// wrong `core.hooksPath`, a missing binary, an image rolled without it
/// — the syncer still refuses, because it applies the same document at
/// the writer.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_pre_receive_does_not_open_the_repository() {
    let policy: Policy = serde_json::from_str(
        r#"{"protected": ["main"], "pushers": {"main": ["release-bot"]}}"#,
    )
    .unwrap();
    let rig = Rig::start_with(policy, true).await;
    std::fs::remove_file(rig.repo.join("hooks/pre-receive")).unwrap();

    rig.commit("a.txt", "base\n");
    let (ok, text) = rig.push_as(Some("agent-runner"), &["origin", "HEAD:refs/heads/main"]);
    assert!(!ok, "the writer refuses what the edge no longer sees: {text}");
    assert!(text.contains("release-bot"), "{text}");
    assert!(rig.snapshot().await.refs.is_empty(), "nothing was published");
}

/// A branch-policy edit takes effect on the next push, with no restart
/// and no roll. In the pod the document arrives on a ConfigMap mount
/// that updates in place; rolling the server to change who may push
/// would drop every clone in flight.
///
/// `pre-receive` is removed first, so what is measured is the SYNCER's
/// re-read rather than the hook's per-push read.
#[tokio::test(flavor = "multi_thread")]
async fn a_policy_edit_takes_effect_without_a_restart() {
    let rig = Rig::start_with(Policy::default(), true).await;
    std::fs::remove_file(rig.repo.join("hooks/pre-receive")).unwrap();

    rig.commit("a.txt", "one\n");
    let (ok, text) = rig.push_as(Some("agent-runner"), &["--quiet", "origin", "HEAD:refs/heads/main"]);
    assert!(ok, "the permissive policy admits this: {text}");

    // The operator re-renders the ConfigMap; the mount updates in place.
    let tightened: Policy = serde_json::from_str(
        r#"{"protected": ["main"], "pushers": {"main": ["release-bot"]}}"#,
    )
    .unwrap();
    std::fs::write(
        rig.repo.join("flint-forge").join(flint_forge::policy::POLICY_FILE),
        serde_json::to_vec_pretty(&tightened).unwrap(),
    )
    .unwrap();

    let ahead = rig.commit("a.txt", "two\n");
    let (ok, text) = rig.push_as(Some("agent-runner"), &["origin", "HEAD:refs/heads/main"]);
    assert!(!ok, "the edited policy must be in force for the very next push: {text}");
    assert!(text.contains("release-bot"), "{text}");
    assert_ne!(rig.snapshot().await.refs.get("refs/heads/main"), Some(&ahead));
}

// ── the residue measurement ──────────────────────────────────────────
//
// Not a pass/fail leg: a MEASUREMENT, because every choice about the
// pack-pinning finding turns on numbers that existed only as prose.
// `docs/plans/forge-pack-pinning-2026-09-08.md` quotes "58% of named
// bytes" from one ~80 KB repository on runcl, and
// `forge-pack-residue-plan-2026-09-08.md` was rejected partly because
// that repository's pushes do not DELTIFY — the unit rig's
// `stage_commit` packs with no `--fix-thin` pass, so it can only make
// all-dead residue packs, while a real `receive-pack` completes a thin
// pack with delta bases that ARE reachable.
//
// This runs a real `git push` through a real `receive-pack` over a
// corpus that deltifies, in three arms that differ ONLY in why the push
// is refused, and then classifies every pack the snapshot names by
// dropping it and asking git.

/// The direction-5 RECORDER, in the only place it can run.
///
/// `pre-receive` is the last moment a push's pack is still identifiable
/// as *this push's* pack: it sits in `$GIT_QUARANTINE_PATH/pack/`, and
/// the probe against real git established that `index-pack --fix-thin`
/// has already completed by then — so the name read here is the name
/// the pack keeps after git migrates it. (That probe also found the
/// map is MANY-TO-ONE: two pushes of identical content produce the same
/// pack name, which is why the classification below ANDs over producers
/// instead of treating a pack as one push's property.)
///
/// This WRAPS the shipped hook rather than replacing it — same stdin,
/// same environment, same exit status, invoked through the binary's own
/// explicit-role argument — so the policy arm still runs through the
/// real refusal and its control still means something.
fn install_pre_receive_recorder(repo: &Path, log: &Path) {
    let target = repo.join("hooks/pre-receive");
    let _ = std::fs::remove_file(&target);
    let script = format!(
        r#"#!/bin/sh
tmp="{log}.$$"
cat > "$tmp"
{{
  printf 'push\n'
  if [ -n "$GIT_QUARANTINE_PATH" ]; then
    for p in "$GIT_QUARANTINE_PATH"/pack/pack-*.pack; do
      [ -e "$p" ] || continue
      printf 'pack %s\n' "$(basename "$p")"
    done
  fi
  while read -r _old new ref; do printf 'ref %s %s\n' "$new" "$ref"; done < "$tmp"
}} >> "{log}"
exec "{bin}" pre-receive < "$tmp"
"#,
        log = log.display(),
        bin = env!("CARGO_BIN_EXE_flint-forge-hook"),
    );
    std::fs::write(&target, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(&target).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&target, perm).unwrap();
}

/// One push, as the recorder saw it.
struct Recorded {
    packs: Vec<String>,
    news: Vec<String>,
    refs: Vec<String>,
}

fn recorded_pushes(log: &Path) -> Vec<Recorded> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut out: Vec<Recorded> = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        match it.next() {
            Some("push") => {
                out.push(Recorded { packs: vec![], news: vec![], refs: vec![] })
            }
            Some("pack") => {
                if let (Some(r), Some(v)) = (out.last_mut(), it.next()) {
                    r.packs.push(v.to_string());
                }
            }
            Some("ref") => {
                if let (Some(r), Some(v)) = (out.last_mut(), it.next()) {
                    r.news.push(v.to_string());
                    if let Some(name) = it.next() {
                        r.refs.push(name.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// What the snapshot's named bytes are worth, split the way the
/// direction-5 question needs: what is redundant, and of that, what the
/// rule would drop versus keep. `producers_all_refused` is the pack ->
/// "every push that produced it was refused" map.
#[derive(Default, Debug)]
struct Residue {
    named: u64,
    redundant: u64,
    removable: u64,
    kept_mixed: u64,
    kept_unattributed: u64,
}

fn classify_residue(
    repo: &Path,
    scratch: &Path,
    snap: &flint_forge::snapshot::Snapshot,
    producers_all_refused: &std::collections::BTreeMap<String, bool>,
) -> Residue {
    let dir = repo.join("objects/pack");
    let mut r = Residue::default();
    for p in &snap.packs {
        let bytes = std::fs::metadata(dir.join(p)).map(|m| m.len()).unwrap_or(0);
        r.named += bytes;
        if !is_redundant(scratch, &dir, &snap.packs, p, &snap.refs) {
            continue;
        }
        r.redundant += bytes;
        match producers_all_refused.get(p) {
            Some(true) => r.removable += bytes,
            Some(false) => r.kept_mixed += bytes,
            None => r.kept_unattributed += bytes,
        }
    }
    r
}

/// The pack -> "every producer refused" map, from the recorder's log and
/// the refs that ended up reachable. MANY-TO-ONE, so it ANDs.
fn producers_all_refused(
    records: &[Recorded],
    reach: &std::collections::HashSet<String>,
) -> std::collections::BTreeMap<String, bool> {
    let mut m: std::collections::BTreeMap<String, bool> = Default::default();
    for rec in records {
        let any_live = rec.news.iter().any(|n| reach.contains(n));
        for pk in &rec.packs {
            let e = m.entry(pk.clone()).or_insert(true);
            *e = *e && !any_live;
        }
    }
    m
}

/// Which named packs can be unnamed WITHOUT BUILDING ANYTHING?
///
/// The greedy an implementation would actually run. Invariant: the kept
/// set always holds every reachable object, so it always restores. Drop
/// a pack when every reachable object it holds is in another KEPT pack;
/// repeat until nothing moves. Order-dependent in which packs go, never
/// in whether the result is safe.
///
/// This is the generalisation of `covering_pack`: it does not need one
/// pack to cover everything, only the SET it keeps to.
fn zero_work_droppable(
    repo: &Path,
    named: &[String],
    reach: &std::collections::HashSet<String>,
) -> (usize, u64) {
    let dir = repo.join("objects/pack");
    let mut live: std::collections::BTreeMap<String, std::collections::HashSet<String>> =
        Default::default();
    let mut size: std::collections::BTreeMap<String, u64> = Default::default();
    for p in named {
        let stem = p.trim_end_matches(".pack");
        let objs = pack_objects(repo, &dir.join(format!("{stem}.idx")));
        live.insert(p.clone(), objs.into_iter().filter(|o| reach.contains(o)).collect());
        size.insert(p.clone(), std::fs::metadata(dir.join(p)).map(|m| m.len()).unwrap_or(0));
    }
    let mut kept: Vec<String> = named.to_vec();
    let (mut n, mut bytes) = (0usize, 0u64);
    loop {
        let victim = kept.iter().find(|p| {
            live[*p].iter().all(|o| kept.iter().any(|q| q != *p && live[q].contains(o)))
        });
        match victim.cloned() {
            Some(v) => {
                kept.retain(|x| x != &v);
                n += 1;
                bytes += size[&v];
            }
            None => break,
        }
    }
    (n, bytes)
}

/// Is there ALREADY a named pack that holds every reachable object?
///
/// The model's direction 4 plans a fresh pack (`holds[f] = {}` in
/// `FoldPlan`), so the reclaim it proves safe always pays a
/// `pack-objects --all --write-bitmap-index` over the whole repository
/// plus the upload of its output. But the rule it proves — drop an
/// input whose REACHABLE objects the new pack covers — never asks that
/// the coverer be new. If a pack the snapshot already names covers the
/// reachable set, every other named pack can be unnamed for the cost of
/// a snapshot CAS: no pack-objects, no upload, nothing on the wire.
///
/// That variant is NOT what TLC checked. This measures whether it would
/// ever apply, which is the question worth answering before modelling
/// it.
fn covering_pack(
    repo: &Path,
    packs: &[String],
    reach: &std::collections::HashSet<String>,
) -> Option<(String, u64)> {
    let dir = repo.join("objects/pack");
    for p in packs {
        let stem = p.trim_end_matches(".pack");
        let objs: std::collections::HashSet<String> =
            pack_objects(repo, &dir.join(format!("{stem}.idx"))).into_iter().collect();
        if reach.iter().all(|o| objs.contains(o)) {
            let bytes = std::fs::metadata(dir.join(p)).map(|m| m.len()).unwrap_or(0);
            return Some((p.clone(), bytes));
        }
    }
    None
}

/// Every object id a pack holds, from its index.
fn pack_objects(repo: &Path, idx: &Path) -> Vec<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("git show-index < {}", idx.display()))
        .current_dir(repo)
        .output()
        .expect("show-index");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(|s| s.to_string()))
        .collect()
}

/// Everything reachable from these tips, in this repository.
fn reachable_from(repo: &Path, tips: &[String]) -> std::collections::HashSet<String> {
    if tips.is_empty() {
        return Default::default();
    }
    let mut args = vec!["rev-list".to_string(), "--objects".to_string()];
    args.extend(tips.iter().cloned());
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    must(repo, &refs)
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(|s| s.to_string()))
        .collect()
}

/// THE ORACLE, and it is measured rather than reasoned: rebuild a bare
/// repository from every named pack EXCEPT this one, install the
/// snapshot's refs, and run the proof the syncer itself runs on every
/// start. If it passes, nothing needed that pack.
fn is_redundant(
    scratch: &Path,
    src: &Path,
    all: &[String],
    drop: &str,
    refs: &std::collections::BTreeMap<String, String>,
) -> bool {
    let _ = std::fs::remove_dir_all(scratch);
    std::fs::create_dir_all(scratch).unwrap();
    let out = Command::new("git")
        .args(["init", "--quiet", "--bare"])
        .arg(scratch)
        .output()
        .expect("init");
    assert!(out.status.success());
    let dst = scratch.join("objects/pack");
    std::fs::create_dir_all(&dst).unwrap();
    for p in all {
        if p == drop {
            continue;
        }
        let stem = p.trim_end_matches(".pack");
        for ext in ["pack", "idx"] {
            let from = src.join(format!("{stem}.{ext}"));
            if from.exists() {
                std::fs::copy(&from, dst.join(format!("{stem}.{ext}"))).unwrap();
            }
        }
    }
    for (name, oid) in refs {
        let out = git(scratch, &["update-ref", name, oid]);
        if !out.status.success() {
            return false; // the ref's own object is gone: not redundant
        }
    }
    git(scratch, &["fsck", "--connectivity-only", "--no-reflogs", "--no-progress"])
        .status
        .success()
}

#[tokio::test(flavor = "multi_thread")]
async fn measure_what_a_refused_push_leaves_in_the_snapshot() {
    let policy = Policy { protected: vec!["refs/heads/locked".into()], ..Policy::default() };
    let rig = Rig::start_tuned(policy, true, |c| {
        // The ladder, brought down to a test-sized repository so the
        // COLLECTOR (a base rebuild's `--all`) actually runs. Without
        // this nothing is ever dropped and there is no residue to see.
        c.fold_factor = 2;
        c.fold_min_bytes = 0;
        c.base_min_bytes = 0;
        c.base_rebuild_min_secs = 0;
    })
    .await;

    // The recorder goes in before ANY push, so every push in every arm
    // appears in it — including the rig's own accepted ones, which is
    // what makes "unattributed" mean "not from a push" rather than
    // "from a push we forgot to watch".
    let reclog = rig.repo.parent().unwrap().join("pushrec.log");
    install_pre_receive_recorder(&rig.repo, &reclog);

    // A corpus that DELTIFIES. This is the dimension the unit rig
    // cannot represent and the one that refuted the rejected plan.
    let big = |mark: &str| -> String {
        (0..4000)
            .map(|i| if i == 2000 { format!("line {i} {mark}\n") } else { format!("line {i}\n") })
            .collect::<String>()
    };
    rig.commit("big.txt", &big("base"));
    assert!(rig.push(&["--quiet", "origin", "HEAD:refs/heads/main"]).0);

    let base = must(&rig.client, &["rev-parse", "HEAD"]).trim().to_string();
    let parent = rig.client.parent().unwrap().to_path_buf();
    let mut refused = 0usize;

    // ── arm N: a non-fast-forward force push, refused by the syncer ──
    for i in 0..3 {
        let c = parent.join(format!("nff{i}"));
        must(&parent, &["clone", "--quiet", rig.repo.to_str().unwrap(), &format!("nff{i}")]);
        must(&c, &["config", "user.email", "t@example.invalid"]);
        must(&c, &["config", "user.name", "t"]);
        must(&c, &["reset", "--quiet", "--hard", &base]);
        std::fs::write(c.join("big.txt"), big(&format!("nff{i}"))).unwrap();
        must(&c, &["add", "big.txt"]);
        must(&c, &["commit", "--quiet", "-m", "nff"]);
        // Land a competing commit first so this one is genuinely non-ff.
        rig.commit("big.txt", &big(&format!("main{i}")));
        assert!(rig.push(&["--quiet", "origin", "HEAD:refs/heads/main"]).0);
        let out = git(&c, &["push", "--force", "--quiet", "origin", "HEAD:refs/heads/main"]);
        let text = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(!out.status.success(), "arm N push {i} must be refused: {text}");
        assert!(text.contains("non-fast-forward"), "arm N {i} wrong class: {text}");
        refused += 1;
    }

    // ── arm F: a refs/for proposal that CONFLICTS ────────────────────
    let mut conflicts = 0usize;
    for i in 0..3 {
        let c = parent.join(format!("prop{i}"));
        must(&parent, &["clone", "--quiet", rig.repo.to_str().unwrap(), &format!("prop{i}")]);
        must(&c, &["config", "user.email", "t@example.invalid"]);
        must(&c, &["config", "user.name", "t"]);
        std::fs::write(c.join("big.txt"), big(&format!("prop{i}"))).unwrap();
        must(&c, &["add", "big.txt"]);
        must(&c, &["commit", "--quiet", "-m", "proposal"]);
        // main moves the same line underneath it, so the merge conflicts.
        rig.commit("big.txt", &big(&format!("moved{i}")));
        assert!(rig.push(&["--quiet", "origin", "HEAD:refs/heads/main"]).0);
        let out = git(&c, &["push", "--quiet", "origin", "HEAD:refs/for/main"]);
        let text = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() && text.contains("conflict") {
            conflicts += 1;
            refused += 1;
        }
        eprintln!("  arm F {i}: ok={} {}", out.status.success(), text.trim().replace('\n', " | "));
    }

    // ── arm M: ONE push, one ref accepted and one refused ────────────
    //
    // The ceiling the git probe found (R7), now in forge's own chain.
    // `receive.procReceiveRefs = refs/` routes every ref through
    // proc-receive, so the syncer answers PER REF — but git built ONE
    // pack for the whole push, and that pack holds both verdicts'
    // objects. Direction 5 names it because something in it was
    // accepted, so the refused half's residue survives the fix. This
    // arm exists to price exactly that.
    let mut mixed = 0usize;
    let mut mixed_refs: Vec<String> = Vec::new();
    for i in 0..3 {
        let c = parent.join(format!("mix{i}"));
        must(&parent, &["clone", "--quiet", rig.repo.to_str().unwrap(), &format!("mix{i}")]);
        must(&c, &["config", "user.email", "t@example.invalid"]);
        must(&c, &["config", "user.name", "t"]);
        // TWO INDEPENDENT commits on the old base. Neither is an
        // ancestor of the other, so accepting one must not make the
        // other reachable — otherwise the arm measures nothing.
        must(&c, &["checkout", "--quiet", "-b", "good", &base]);
        std::fs::write(c.join("big.txt"), big(&format!("mixgood{i}"))).unwrap();
        must(&c, &["add", "big.txt"]);
        must(&c, &["commit", "--quiet", "-m", "good"]);
        must(&c, &["checkout", "--quiet", "-b", "bad", &base]);
        std::fs::write(c.join("big.txt"), big(&format!("mixbad{i}"))).unwrap();
        must(&c, &["add", "big.txt"]);
        must(&c, &["commit", "--quiet", "-m", "bad"]);
        let good_ref = format!("refs/heads/mixed{i}");
        let out = git(
            &c,
            &[
                "push",
                "--force",
                "--quiet",
                "origin",
                &format!("good:{good_ref}"),
                "bad:refs/heads/main",
            ],
        );
        let text = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(!out.status.success(), "arm M {i}: the bad half must be refused: {text}");
        assert!(text.contains("non-fast-forward"), "arm M {i} wrong class: {text}");
        mixed_refs.push(good_ref);
        mixed += 1;
        refused += 1;
    }

    // ── arm P: refused by POLICY, which pre-receive answers ──────────
    // The control. pre-receive runs BEFORE git migrates the quarantine,
    // so this arm must leave NOTHING — if it does, the whole "refuse
    // earlier" family is pointless and the measurement can say so.
    let packs_before_p = rig.snapshot().await.packs.len();
    let objs_before_p = must(&rig.repo, &["count-objects", "-v"]).to_string();
    for i in 0..3 {
        let c = parent.join(format!("pol{i}"));
        must(&parent, &["clone", "--quiet", rig.repo.to_str().unwrap(), &format!("pol{i}")]);
        must(&c, &["config", "user.email", "t@example.invalid"]);
        must(&c, &["config", "user.name", "t"]);
        std::fs::write(c.join("big.txt"), big(&format!("pol{i}"))).unwrap();
        must(&c, &["add", "big.txt"]);
        must(&c, &["commit", "--quiet", "-m", "policy"]);
        let out = git_as(&c, &["push", "--quiet", "origin", "HEAD:refs/heads/locked"], Some("nobody"));
        assert!(!out.status.success(), "arm P push {i} must be refused");
        refused += 1;
    }
    let objs_after_p = must(&rig.repo, &["count-objects", "-v"]).to_string();

    // Let the serving loop's compaction settle.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let snap = rig.snapshot().await;
    let dir = rig.repo.join("objects/pack");
    let tips: Vec<String> = snap.refs.values().cloned().collect();
    let reach = reachable_from(&rig.repo, &tips);

    eprintln!("\n=== residue measurement: {} refusals, {} conflicts ===", refused, conflicts);
    eprintln!("snapshot seq {} names {} pack(s), {} ref(s)", snap.seq, snap.packs.len(), snap.refs.len());

    // ── what direction 5 would name ──────────────────────────────────
    //
    // A recorded push was WHOLLY refused iff NONE of the object ids it
    // proposed ended up reachable. That is the same fact the syncer
    // holds at proc-receive time (which refs it said `ok` to), but it
    // is derived here from the FINAL STATE rather than from the
    // measurement's own bookkeeping, so a mislabelled arm cannot
    // produce a flattering answer.
    let records = recorded_pushes(&reclog);
    let mut producers_all_refused: std::collections::BTreeMap<String, bool> = Default::default();
    let mut wholly_refused = 0usize;
    let mut recorded_packs = 0usize;
    for r in &records {
        let any_live = r.news.iter().any(|n| reach.contains(n));
        if !any_live {
            wholly_refused += 1;
        }
        recorded_packs += r.packs.len();
        for pk in &r.packs {
            // MANY-TO-ONE: identical content gives identical checksums,
            // so one pack name can have several producers. It is
            // droppable only if EVERY producer was refused.
            let e = producers_all_refused.entry(pk.clone()).or_insert(true);
            *e = *e && !any_live;
        }
    }
    let never_migrated = producers_all_refused
        .keys()
        .filter(|pk| !dir.join(pk.as_str()).exists())
        .count();

    let scratch = rig.repo.parent().unwrap().join("scratch.git");
    let mut named_bytes = 0u64;
    let mut redundant_bytes = 0u64;
    let mut d5_removable = 0u64;
    let mut d5_kept_mixed = 0u64;
    let mut d5_kept_unattributed = 0u64;
    eprintln!(
        "{:<12} {:>9} {:>7} {:>9} {:>10}  {}",
        "pack", "bytes", "objs", "reachable", "redundant?", "direction 5"
    );
    for p in &snap.packs {
        let stem = p.trim_end_matches(".pack");
        let bytes = std::fs::metadata(dir.join(p)).map(|m| m.len()).unwrap_or(0);
        let objs = pack_objects(&rig.repo, &dir.join(format!("{stem}.idx")));
        let live = objs.iter().filter(|o| reach.contains(*o)).count();
        let red = is_redundant(&scratch, &dir, &snap.packs, p, &snap.refs);
        let verdict = match producers_all_refused.get(p) {
            Some(true) => "drops (push wholly refused)",
            Some(false) => "NAMES (mixed/accepted push)",
            None => "NAMES (not from a push)",
        };
        named_bytes += bytes;
        if red {
            redundant_bytes += bytes;
            match producers_all_refused.get(p) {
                Some(true) => d5_removable += bytes,
                Some(false) => d5_kept_mixed += bytes,
                None => d5_kept_unattributed += bytes,
            }
        }
        eprintln!(
            "{:<12} {:>9} {:>7} {:>9} {:>10}  {}",
            &stem[5..13.min(stem.len())],
            bytes,
            objs.len(),
            live,
            if red { "YES" } else { "no" },
            verdict
        );
    }
    let pct = if named_bytes > 0 { redundant_bytes * 100 / named_bytes } else { 0 };
    eprintln!("\nnamed {named_bytes} B, redundant {redundant_bytes} B  => {pct}%");
    eprintln!(
        "recorder: {} push(es), {recorded_packs} pack name(s), {wholly_refused} wholly refused, \
         {never_migrated} quarantine pack(s) never migrated (arm P)",
        records.len()
    );
    let share = |b: u64| if redundant_bytes > 0 { b * 100 / redundant_bytes } else { 0 };
    eprintln!(
        "direction 5 removes  {d5_removable} B of the {redundant_bytes} B residue => {}%",
        share(d5_removable)
    );
    eprintln!(
        "direction 5 KEEPS    {d5_kept_mixed} B ({}%) in mixed/accepted packs  \
         + {d5_kept_unattributed} B ({}%) not from a push",
        share(d5_kept_mixed),
        share(d5_kept_unattributed)
    );
    // ── arm P's control, and why it is no longer count-objects ───────
    //
    // With arm M added, the compaction ladder runs CONTINUOUSLY through
    // this rig, so a base rebuild's output can land inside arm P's
    // window: `count-objects` moved by exactly one pack and 30 objects
    // in every rep, which is the size of the base rebuild's own output.
    // A control a second writer can move is not a control, so the claim
    // is made from the recorder instead, where nothing else can write:
    // every pack an arm P push put in quarantine must be absent from
    // `objects/pack` AND unnamed by the snapshot.
    let mut p_packs = 0usize;
    for r in records.iter().filter(|r| r.refs.iter().any(|n| n == "refs/heads/locked")) {
        for pk in &r.packs {
            p_packs += 1;
            assert!(
                !dir.join(pk.as_str()).exists(),
                "arm P control: {pk} was migrated out of quarantine"
            );
            assert!(!snap.packs.contains(pk), "arm P control: {pk} is NAMED by the snapshot");
        }
    }
    assert_eq!(p_packs, 3, "arm P must have put three packs in quarantine");
    // THE OTHER ARM. `!exists` is only evidence if `exists` can be true
    // for the same predicate, read from the same log against the same
    // directory — so run it on arm N, where git DID migrate: a
    // single-ref push to main that nothing reachable came out of.
    let mut n_packs = 0usize;
    for r in records
        .iter()
        .filter(|r| r.refs == ["refs/heads/main"] && !r.news.iter().any(|n| reach.contains(n)))
    {
        for pk in &r.packs {
            n_packs += 1;
            assert!(
                dir.join(pk.as_str()).exists(),
                "arm N: {pk} should have been MIGRATED — the control's other arm is open"
            );
        }
    }
    assert_eq!(n_packs, 3, "arm N must have produced three migrated packs");
    eprintln!("arm P control: 3 quarantine pack(s) built, 0 migrated, 0 named — refusing at");
    eprintln!("  pre-receive really does leave nothing, measured through forge's own hook.");
    eprintln!("count-objects across arm P (CONFOUNDED by concurrent compaction, shown for");
    eprintln!("  the record) before:\n{objs_before_p}after:\n{objs_after_p}");
    eprintln!("packs named before arm P: {packs_before_p}, after: {}", snap.packs.len());

    // The in-chain replication of the git probe's R1: a quarantine pack
    // name survives migration unchanged. If it did not, every pack
    // below would classify as "not from a push" and direction 5 would
    // measure 0% — so the two asserts after the table are this claim's
    // positive control, not decoration.
    let migrated = recorded_packs - never_migrated;
    eprintln!(
        "R1 in forge's chain: {migrated} of {recorded_packs} recorded quarantine names exist in \
         objects/pack; the {never_migrated} that do not are arm P's."
    );

    // The hard assertions: the measurement must have MEASURED
    // something, and each arm must have done what it claims.
    assert!(refused >= 9, "the arms must actually have been refused: {refused}");
    assert!(!snap.packs.is_empty());
    assert_eq!(mixed, 3, "arm M must have run");
    // A mixed push whose accepted half did NOT land is just arm N
    // again, and the arm would be measuring nothing.
    for r in &mixed_refs {
        assert!(snap.refs.contains_key(r), "arm M: {r} must have LANDED, refs={:?}", snap.refs.keys());
    }
    // Two independent derivations of the same count must agree: the
    // arms constructed 3+3+3 wholly-refused pushes (N, F, P) and the
    // recorder derived its own answer from reachability alone.
    assert_eq!(
        wholly_refused,
        3 + conflicts + 3,
        "recorder disagrees with the arms about how many pushes were wholly refused"
    );
    // Both halves of the direction-5 answer must be non-zero, or the
    // measurement is reporting a mapping that silently failed: nothing
    // removable means the recorded names did not survive migration,
    // and nothing kept means arm M never shared a pack.
    assert!(d5_removable > 0, "no residue attributed to a wholly-refused push");
    assert!(d5_kept_mixed > 0, "arm M produced no shared pack — R7 is not being measured");
}

// ── does the residue direction 5 KEEPS grow without bound? ───────────
//
// Direction 5 removes 78-81% of the residue and keeps what came from
// MIXED pushes, because git built one pack for a push whose refs got
// different verdicts. That is a constant-factor win. The question this
// answers is whether it is a constant factor off a BOUNDED quantity or
// an unbounded one — because direction 5 does not touch the pinning
// rule at all, and if the kept share accumulates then direction 5 buys
// time while direction 4 (a reclaiming rebuild under quiescence) is the
// only thing that collects.
//
// So: run identical rounds, and read the curve rather than the ratio.
// Each round is one accepted push (live content grows too, which is the
// honest denominator), one mixed push, and one wholly-refused push.

#[tokio::test(flavor = "multi_thread")]
async fn measure_whether_the_residue_direction_5_keeps_grows() {
    let rig = Rig::start_tuned(Policy::default(), true, |c| {
        c.fold_factor = 2;
        c.fold_min_bytes = 0;
        c.base_min_bytes = 0;
        c.base_rebuild_min_secs = 0;
    })
    .await;
    let reclog = rig.repo.parent().unwrap().join("pushrec.log");
    install_pre_receive_recorder(&rig.repo, &reclog);

    let big = |mark: &str| -> String {
        (0..4000)
            .map(|i| if i == 2000 { format!("line {i} {mark}\n") } else { format!("line {i}\n") })
            .collect::<String>()
    };
    rig.commit("big.txt", &big("base"));
    assert!(rig.push(&["--quiet", "origin", "HEAD:refs/heads/main"]).0);
    // The fixed OLD base every refused push is built on, so every round
    // is genuinely non-fast-forward as main advances past it.
    let base = must(&rig.client, &["rev-parse", "HEAD"]).trim().to_string();
    let parent = rig.client.parent().unwrap().to_path_buf();
    let scratch = parent.join("scratch.git");

    const ROUNDS: usize = 8;
    let mut curve: Vec<(usize, Residue, usize, Option<(String, u64)>)> = Vec::new();
    for r in 0..ROUNDS {
        // 1. an ordinary ACCEPTED push: live content grows too.
        rig.commit("big.txt", &big(&format!("live{r}")));
        assert!(rig.push(&["--quiet", "origin", "HEAD:refs/heads/main"]).0);

        // 2. a MIXED push: one ref accepted, one refused, ONE pack.
        let c = parent.join(format!("mix{r}"));
        must(&parent, &["clone", "--quiet", rig.repo.to_str().unwrap(), &format!("mix{r}")]);
        must(&c, &["config", "user.email", "t@example.invalid"]);
        must(&c, &["config", "user.name", "t"]);
        must(&c, &["checkout", "--quiet", "-b", "good", &base]);
        std::fs::write(c.join("big.txt"), big(&format!("g{r}"))).unwrap();
        must(&c, &["add", "big.txt"]);
        must(&c, &["commit", "--quiet", "-m", "good"]);
        must(&c, &["checkout", "--quiet", "-b", "bad", &base]);
        std::fs::write(c.join("big.txt"), big(&format!("b{r}"))).unwrap();
        must(&c, &["add", "big.txt"]);
        must(&c, &["commit", "--quiet", "-m", "bad"]);
        let out = git(
            &c,
            &[
                "push",
                "--force",
                "--quiet",
                "origin",
                &format!("good:refs/heads/mixed{r}"),
                "bad:refs/heads/main",
            ],
        );
        assert!(!out.status.success(), "round {r}: the mixed push's bad half must be refused");

        // 3. a WHOLLY refused push, the class direction 5 does remove.
        let n = parent.join(format!("nff{r}"));
        must(&parent, &["clone", "--quiet", rig.repo.to_str().unwrap(), &format!("nff{r}")]);
        must(&n, &["config", "user.email", "t@example.invalid"]);
        must(&n, &["config", "user.name", "t"]);
        must(&n, &["reset", "--quiet", "--hard", &base]);
        std::fs::write(n.join("big.txt"), big(&format!("n{r}"))).unwrap();
        must(&n, &["add", "big.txt"]);
        must(&n, &["commit", "--quiet", "-m", "nff"]);
        let out = git(&n, &["push", "--force", "--quiet", "origin", "HEAD:refs/heads/main"]);
        assert!(!out.status.success(), "round {r}: the non-ff push must be refused");

        // Let the ladder settle: the fold and the base rebuild are where
        // pinning actually happens, so measuring before they run would
        // measure the wrong thing.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let snap = rig.snapshot().await;
        let tips: Vec<String> = snap.refs.values().cloned().collect();
        let reach = reachable_from(&rig.repo, &tips);
        let map = producers_all_refused(&recorded_pushes(&reclog), &reach);
        let res = classify_residue(&rig.repo, &scratch, &snap, &map);
        let cover = covering_pack(&rig.repo, &snap.packs, &reach);
        curve.push((r, res, snap.packs.len(), cover));
    }

    eprintln!("\n=== does direction 5's leftover grow? {ROUNDS} identical rounds ===");
    eprintln!(
        "{:>5} {:>6} {:>10} {:>10} {:>10} {:>10} {:>8}  {}",
        "round", "packs", "named B", "redundant", "d5 drops", "d5 KEEPS", "keeps %",
        "a named pack already covers the reachable set?"
    );
    for (r, res, packs, cover) in &curve {
        let pct = if res.redundant > 0 { res.kept_mixed * 100 / res.redundant } else { 0 };
        let c = match cover {
            Some((p, b)) => format!(
                "YES {} ({b} B) — {} B unnamable for a CAS",
                &p[5..13.min(p.len())],
                res.named - b
            ),
            None => "no — a reclaim here must BUILD one".to_string(),
        };
        eprintln!(
            "{r:>5} {packs:>6} {:>10} {:>10} {:>10} {:>10} {:>7}%  {c}",
            res.named, res.redundant, res.removable, res.kept_mixed, pct
        );
    }
    let covered = curve.iter().filter(|(_, _, _, c)| c.is_some()).count();
    eprintln!(
        "\nzero-work reclaim applies in {covered} of {ROUNDS} rounds: a pack the snapshot \
         ALREADY names covers everything reachable, so the other packs could be unnamed \
         without running pack-objects at all."
    );
    let first = &curve.first().unwrap().1;
    let last = &curve.last().unwrap().1;
    eprintln!(
        "\nd5 KEEPS: {} B after round 0 -> {} B after round {}  ({}x)",
        first.kept_mixed,
        last.kept_mixed,
        ROUNDS - 1,
        if first.kept_mixed > 0 { last.kept_mixed / first.kept_mixed } else { 0 }
    );
    eprintln!(
        "named:    {} B -> {} B  ({}x) — the denominator grows too",
        first.named,
        last.named,
        if first.named > 0 { last.named / first.named } else { 0 }
    );
    eprintln!("unattributed residue at the end: {} B", last.kept_unattributed);

    // The rounds are identical, so a flat curve and a rising one are
    // both real answers — but the measurement must have MEASURED.
    assert_eq!(curve.len(), ROUNDS);
    assert!(last.kept_mixed > 0, "no mixed-push residue: the arm is not measuring R7");
}

// ── does the zero-work reclaim survive a REALISTIC base cadence? ─────
//
// `measure_whether_the_residue_direction_5_keeps_grows` found a single
// named pack covering the whole reachable set in 24 of 24 rounds — but
// it ran at `base_rebuild_min_secs = 0`, so a base rebuild was always
// current. Shipped is 3600: at most one base rebuild an HOUR (the pack
// cap overrides it, the disk check never does). Every accepted push
// after a base rebuild adds reachable objects the base does not hold,
// so the SINGLE-pack coverer should degrade — and the question that
// actually decides whether direction 4 is free is the SET version:
// can the reclaim still collect the whole residue without building?
//
// Three arms, differing only in that cadence.

struct Row {
    packs: usize,
    named: u64,
    redundant: u64,
    greedy_n: usize,
    greedy_bytes: u64,
    single_coverer: bool,
}

async fn residue_at_cadence(base_rebuild_min_secs: u64, rounds: usize) -> Vec<Row> {
    let rig = Rig::start_tuned(Policy::default(), true, |c| {
        c.fold_factor = 2;
        c.fold_min_bytes = 0;
        c.base_min_bytes = 0;
        // THE ONLY DIMENSION UNDER TEST.
        c.base_rebuild_min_secs = base_rebuild_min_secs;
    })
    .await;
    let reclog = rig.repo.parent().unwrap().join("pushrec.log");
    install_pre_receive_recorder(&rig.repo, &reclog);
    let big = |mark: &str| -> String {
        (0..4000)
            .map(|i| if i == 2000 { format!("line {i} {mark}\n") } else { format!("line {i}\n") })
            .collect::<String>()
    };
    rig.commit("big.txt", &big("base"));
    assert!(rig.push(&["--quiet", "origin", "HEAD:refs/heads/main"]).0);
    let base = must(&rig.client, &["rev-parse", "HEAD"]).trim().to_string();
    let parent = rig.client.parent().unwrap().to_path_buf();
    let scratch = parent.join("scratch.git");

    let mut rows = Vec::new();
    for r in 0..rounds {
        rig.commit("big.txt", &big(&format!("live{r}")));
        assert!(rig.push(&["--quiet", "origin", "HEAD:refs/heads/main"]).0);
        let c = parent.join(format!("mix{r}"));
        must(&parent, &["clone", "--quiet", rig.repo.to_str().unwrap(), &format!("mix{r}")]);
        must(&c, &["config", "user.email", "t@example.invalid"]);
        must(&c, &["config", "user.name", "t"]);
        must(&c, &["checkout", "--quiet", "-b", "good", &base]);
        std::fs::write(c.join("big.txt"), big(&format!("g{r}"))).unwrap();
        must(&c, &["add", "big.txt"]);
        must(&c, &["commit", "--quiet", "-m", "good"]);
        must(&c, &["checkout", "--quiet", "-b", "bad", &base]);
        std::fs::write(c.join("big.txt"), big(&format!("b{r}"))).unwrap();
        must(&c, &["add", "big.txt"]);
        must(&c, &["commit", "--quiet", "-m", "bad"]);
        let out = git(
            &c,
            &[
                "push",
                "--force",
                "--quiet",
                "origin",
                &format!("good:refs/heads/mixed{r}"),
                "bad:refs/heads/main",
            ],
        );
        assert!(!out.status.success(), "round {r}: the mixed push's bad half must be refused");
        let n = parent.join(format!("nff{r}"));
        must(&parent, &["clone", "--quiet", rig.repo.to_str().unwrap(), &format!("nff{r}")]);
        must(&n, &["config", "user.email", "t@example.invalid"]);
        must(&n, &["config", "user.name", "t"]);
        must(&n, &["reset", "--quiet", "--hard", &base]);
        std::fs::write(n.join("big.txt"), big(&format!("n{r}"))).unwrap();
        must(&n, &["add", "big.txt"]);
        must(&n, &["commit", "--quiet", "-m", "nff"]);
        let out = git(&n, &["push", "--force", "--quiet", "origin", "HEAD:refs/heads/main"]);
        assert!(!out.status.success(), "round {r}: the non-ff push must be refused");

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let snap = rig.snapshot().await;
        let tips: Vec<String> = snap.refs.values().cloned().collect();
        let reach = reachable_from(&rig.repo, &tips);
        let map = producers_all_refused(&recorded_pushes(&reclog), &reach);
        let res = classify_residue(&rig.repo, &scratch, &snap, &map);
        let (greedy_n, greedy_bytes) = zero_work_droppable(&rig.repo, &snap.packs, &reach);
        rows.push(Row {
            packs: snap.packs.len(),
            named: res.named,
            redundant: res.redundant,
            greedy_n,
            greedy_bytes,
            single_coverer: covering_pack(&rig.repo, &snap.packs, &reach).is_some(),
        });
    }
    rows
}

#[tokio::test(flavor = "multi_thread")]
async fn measure_the_zero_work_reclaim_against_a_real_base_cadence() {
    const ROUNDS: usize = 6;
    // 0 = the earlier rig (a base rebuild is always current);
    // 6 = a base rebuild every few rounds;
    // 3600 = SHIPPED (one at the start, none after).
    for cadence in [0u64, 6, 3600] {
        let rows = residue_at_cadence(cadence, ROUNDS).await;
        eprintln!("\n=== base_rebuild_min_secs = {cadence} ===");
        eprintln!(
            "{:>5} {:>6} {:>10} {:>10} {:>8} {:>11} {:>9}  {}",
            "round", "packs", "named B", "redundant", "greedy n", "greedy B", "greedy %",
            "single coverer?"
        );
        for (r, row) in rows.iter().enumerate() {
            let pct =
                if row.redundant > 0 { row.greedy_bytes * 100 / row.redundant } else { 100 };
            eprintln!(
                "{r:>5} {:>6} {:>10} {:>10} {:>8} {:>11} {:>8}%  {}",
                row.packs,
                row.named,
                row.redundant,
                row.greedy_n,
                row.greedy_bytes,
                pct,
                if row.single_coverer { "YES" } else { "no — must BUILD one" }
            );
        }
        let singles = rows.iter().filter(|r| r.single_coverer).count();
        let last = rows.last().unwrap();
        eprintln!(
            "cadence {cadence}: single coverer in {singles}/{ROUNDS} rounds; the GREEDY SET \
             collects {} of {} redundant B at the last round, building nothing.",
            last.greedy_bytes, last.redundant
        );
        // The greedy must never claim MORE than the drop-and-fsck oracle
        // says is redundant — that would mean it is unsafe.
        assert!(
            last.greedy_bytes <= last.redundant,
            "cadence {cadence}: greedy claims {} B but only {} B is redundant",
            last.greedy_bytes,
            last.redundant
        );
    }
}
