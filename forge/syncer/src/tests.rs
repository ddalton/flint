//! The battery. Every test here is an instantiation of a rule the
//! design records a measurement or a mutation for, and several of them
//! are the falsifiers of §13 run against the memory store rather than
//! against a cluster: the cluster legs prove the same properties at
//! fleet scale, but the property itself is decided here, where a
//! control can be run in a second.
//!
//! The fixture drives real `git` against a real bare repository. A
//! test double for git would have been a test of the double: every
//! defect the review found in the first draft was a fact about git's
//! actual behaviour, not about a model of it.

use std::sync::Arc;

use flint_store::memory::MemoryStore;
use flint_store::ObjectStore;

use super::batch::{self, CommandResult, PushRequest};
use super::policy::{Policy, Verdict};
use super::gitcmd::RefUpdate;
use super::status::Phase;
use super::{lease, restore, snapshot, status, sweep, ForgeConfig, ForgeError, Syncer};

const PREFIX: &str = "tenant/repo";

struct Rig {
    #[allow(dead_code)]
    _dir: tempfile::TempDir,
    store: Arc<MemoryStore>,
    sc: Syncer,
}

impl Rig {
    async fn new() -> Rig {
        Rig::with_store(Arc::new(MemoryStore::new()), "a").await
    }

    async fn with_store(store: Arc<MemoryStore>, who: &str) -> Rig {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo.git");
        let cfg = ForgeConfig::new(PREFIX, &repo);
        let sc = Syncer::new(
            store.clone() as Arc<dyn ObjectStore>,
            cfg,
            format!("forge-test-{who}"),
        );
        let rig = Rig { _dir: dir, store, sc };
        rig.sc.git.init_bare("main", None).await.expect("init");
        rig
    }

    /// Claim the lease and restore, as start-up does.
    async fn start(&mut self) {
        for _ in 0..16 {
            match lease::claim_step(&mut self.sc).await.expect("claim") {
                lease::ClaimOutcome::Claimed(_) => break,
                lease::ClaimOutcome::Waiting { .. } => continue,
            }
        }
        assert!(self.sc.lease().is_ok(), "the rig must hold the lease");
        // As `server::run` does between the claim and the restore.
        sweep::abort_orphaned_uploads(&self.sc).await.expect("startup sweep");
        restore::restore(&mut self.sc).await.expect("restore");
    }

    async fn git(&self, args: &[&str], stdin: Option<&[u8]>) -> String {
        self.sc.git.must(args, stdin).await.expect("git")
    }

    /// Build a commit in the bare repository and pack it, which is
    /// what `receive-pack` leaves behind for a push
    /// (`receive.unpackLimit = 1` makes every push a pack).
    async fn stage_commit(
        &self,
        parent: Option<&str>,
        files: &[(&str, &str)],
        message: &str,
    ) -> String {
        let mut tree_spec = String::new();
        for (name, content) in files {
            let blob = self
                .git(&["hash-object", "-w", "--stdin"], Some(content.as_bytes()))
                .await
                .trim()
                .to_string();
            tree_spec.push_str(&format!("100644 blob {blob}\t{name}\n"));
        }
        let tree = self.git(&["mktree"], Some(tree_spec.as_bytes())).await.trim().to_string();
        let parents: Vec<String> = parent.map(|p| p.to_string()).into_iter().collect();
        let commit = self
            .sc
            .git
            .commit_tree(&tree, &parents, message, "tester")
            .await
            .expect("commit-tree");
        // Pack it exactly as a push would arrive.
        let refs = self.sc.git.refs().await.expect("refs");
        let excludes: Vec<String> = refs.values().cloned().collect();
        self.sc
            .git
            .pack_new_objects(std::slice::from_ref(&commit), &excludes)
            .await
            .expect("pack-objects");
        commit
    }

    async fn run(&mut self, pushes: Vec<PushRequest>) -> Vec<batch::PushReport> {
        batch::run_batch(&mut self.sc, pushes, &Policy::default()).await.expect("batch")
    }
}

/// A push as a hook would hand it over. A free function, not a method
/// on the rig, so a test can build one while the rig is borrowed for
/// the batch it is about to run.
fn push(id: u64, cmds: Vec<RefUpdate>) -> PushRequest {
    PushRequest { id, principal: "tester".into(), options: vec![], commands: cmds }
}

fn zero() -> String {
    "0".repeat(40)
}

fn is_ok(r: &CommandResult) -> bool {
    matches!(r, CommandResult::Ok { .. })
}

fn ng_reason(r: &CommandResult) -> String {
    match r {
        CommandResult::Ng { reason, .. } => reason.clone(),
        other => panic!("expected ng, got {other:?}"),
    }
}

// ── the acknowledgement rule ─────────────────────────────────────────

/// git migrates a push's quarantine in the order `.keep`, `.pack`,
/// `.rev`, `.idx` (`tmp-objdir.c`, `pack_copy_priority`), so for a
/// moment a neighbour's pack is on disk without its index. A batch
/// that lists it in that moment must not name it: the snapshot would
/// carry a pack with no index, the index would never be uploaded
/// (a named pack is skipped for good), and a restore of that snapshot
/// would install refs into objects git cannot see — a refusal, and
/// unrecoverable. The control is the listing before this rule, which
/// named every `pack-*.pack` it saw.
#[tokio::test]
async fn a_pack_without_its_index_is_neither_uploaded_nor_named() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;

    // The neighbour, mid-migration: `.keep` and `.pack` have landed,
    // `.idx` has not.
    let stem = "pack-0000000000000000000000000000000000000001";
    let dir = rig.sc.cfg.repo.join("objects/pack");
    std::fs::write(dir.join(format!("{stem}.keep")), b"receive-pack 1 on host\n").unwrap();
    std::fs::write(dir.join(format!("{stem}.pack")), b"PACK").unwrap();

    let reports = rig
        .run(vec![push(1, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: zero(),
            new_oid: c.clone(),
        }])])
        .await;
    assert!(is_ok(&reports[0].results[0]), "{:?}", reports[0].results[0]);

    let cell = snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await.expect("snapshot");
    assert_eq!(cell.snap.packs.len(), 1, "only the complete pack is named: {:?}", cell.snap.packs);
    assert!(!cell.snap.packs[0].starts_with(stem));
    let uploaded = rig.store.list(&rig.sc.cfg.pack_prefix()).await.expect("list");
    assert!(
        uploaded.iter().all(|o| !o.key.contains(stem)),
        "nothing of the index-less pack reaches the bucket: {:?}",
        uploaded.iter().map(|o| o.key.clone()).collect::<Vec<_>>()
    );

    // Once the index lands the pack is complete, and the next batch
    // uploads and names it.
    std::fs::write(dir.join(format!("{stem}.idx")), b"IDX").unwrap();
    let c2 = rig.stage_commit(Some(&c), &[("b.txt", "two\n")], "second").await;
    let reports = rig
        .run(vec![push(2, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: c.clone(),
            new_oid: c2,
        }])])
        .await;
    assert!(is_ok(&reports[0].results[0]), "{:?}", reports[0].results[0]);
    let cell = snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await.expect("snapshot");
    assert!(cell.snap.packs.iter().any(|p| p == &format!("{stem}.pack")), "{:?}", cell.snap.packs);
    rig.store.head(&rig.sc.cfg.pack_key(&format!("{stem}.idx"))).await.expect("its index is in the bucket");
}

/// Falsifier 1, decided here: what a push acknowledges, the bucket
/// already holds. The control is the shape the first draft would have
/// shipped — sync after the report — and it is not reachable in this
/// code at all, which is the point of doing the CAS before the ref
/// transaction rather than after it.
#[tokio::test]
async fn an_acknowledged_push_is_in_the_bucket_before_the_ref_moves() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    let reports = rig
        .run(vec![push(1, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: zero(),
            new_oid: c.clone(),
        }])])
        .await;
    assert!(is_ok(&reports[0].results[0]), "{:?}", reports[0].results[0]);

    let cell = snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await.expect("snapshot");
    assert_eq!(cell.snap.oid("refs/heads/main"), Some(c.as_str()));
    assert!(!cell.snap.packs.is_empty(), "the pack must be named by the snapshot");
    for pack in &cell.snap.packs {
        rig.store.head(&rig.sc.cfg.pack_key(pack)).await.expect("the pack must be in the bucket");
    }
    assert_eq!(rig.sc.git.ref_oid("refs/heads/main").await.unwrap(), Some(c));
}

/// Falsifier 5: the bucket alone rebuilds the repository. A fresh
/// directory, the same store, no local cache at all.
#[tokio::test]
async fn a_cold_restore_reproduces_the_refs_and_passes_fsck() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c1 = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: c1.clone(),
    }])])
    .await;
    let c2 = rig.stage_commit(Some(&c1), &[("a.txt", "two\n")], "second").await;
    rig.run(vec![push(2, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: c1.clone(),
        new_oid: c2.clone(),
    }])])
    .await;

    let mut cold = Rig::with_store(rig.store.clone(), "cold").await;
    restore::restore(&mut cold.sc).await.expect("cold restore");
    assert_eq!(cold.sc.git.ref_oid("refs/heads/main").await.unwrap(), Some(c2));
    cold.sc.git.fsck_connectivity().await.expect("a restored repository must be whole");
}

/// A snapshot naming a pack the bucket does not hold is refused, not
/// served. Half a repository serves clones that succeed and check out
/// nothing.
#[tokio::test]
async fn a_snapshot_naming_a_missing_pack_refuses_to_serve() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: c,
    }])])
    .await;
    let packs = rig.sc.cell().unwrap().snap.packs.clone();
    for p in &packs {
        rig.store.delete(&rig.sc.cfg.pack_key(p)).await.unwrap();
    }
    let mut cold = Rig::with_store(rig.store.clone(), "cold").await;
    match restore::restore(&mut cold.sc).await {
        Err(ForgeError::Refused(m)) => assert!(m.contains("does not hold"), "{m}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// ── step 2: staleness, in a batch ────────────────────────────────────

/// Falsifier 2. Both pushes name the same old-oid and arrive in ONE
/// batch, which is the case a check against the collection-time view
/// gets wrong: it would tell both clients `ok` and keep one.
#[tokio::test]
async fn two_pushes_to_one_ref_in_one_batch_and_exactly_one_wins() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let base = rig.stage_commit(None, &[("a.txt", "base\n")], "base").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: base.clone(),
    }])])
    .await;

    let n1 = rig.stage_commit(Some(&base), &[("a.txt", "one\n")], "one").await;
    let n2 = rig.stage_commit(Some(&base), &[("a.txt", "two\n")], "two").await;
    let reports = rig
        .run(vec![
            push(1, vec![RefUpdate {
                name: "refs/heads/main".into(),
                old_oid: base.clone(),
                new_oid: n1.clone(),
            }]),
            push(2, vec![RefUpdate {
                name: "refs/heads/main".into(),
                old_oid: base.clone(),
                new_oid: n2.clone(),
            }]),
        ])
        .await;
    assert!(is_ok(&reports[0].results[0]), "the first push wins");
    assert_eq!(ng_reason(&reports[1].results[0]), "stale info: fetch first");

    let cell = snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await.unwrap();
    assert_eq!(cell.snap.oid("refs/heads/main"), Some(n1.as_str()));
    assert_eq!(rig.sc.git.ref_oid("refs/heads/main").await.unwrap(), Some(n1));
}

/// The snapshot's half of the staleness test. The local ref agrees
/// with the push and the BUCKET does not: a syncer that checked only
/// the local ref would accept it.
#[tokio::test]
async fn a_push_that_matches_the_local_ref_but_not_the_bucket_is_refused() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let base = rig.stage_commit(None, &[("a.txt", "base\n")], "base").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: base.clone(),
    }])])
    .await;

    // The bucket moved on without us — the shape a lost CAS leaves.
    let mut cell = rig.sc.cell().unwrap().clone();
    let other = rig.stage_commit(Some(&base), &[("a.txt", "elsewhere\n")], "elsewhere").await;
    let mut next = cell.snap.clone();
    next.refs.insert("refs/heads/main".into(), other);
    let writer = rig.sc.holder_id.clone();
    cell = snapshot::cas(rig.store.as_ref(), &rig.sc.cfg, &cell, next, 1, &writer).await.unwrap();
    // The syncer still believes what it last read, but re-reads the
    // snapshot's refs at batch time through its own cell — so plant
    // the disagreement in the cell the way a restart would find it.
    rig.sc.cell = Some(cell);

    let n = rig.stage_commit(Some(&base), &[("a.txt", "mine\n")], "mine").await;
    let reports = rig
        .run(vec![push(9, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: base,
            new_oid: n,
        }])])
        .await;
    let why = ng_reason(&reports[0].results[0]);
    assert!(why.contains("differs between this server and the bucket"), "{why}");
}

#[tokio::test]
async fn a_non_fast_forward_is_refused_unless_the_policy_allows_it() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let base = rig.stage_commit(None, &[("a.txt", "base\n")], "base").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: base.clone(),
    }])])
    .await;
    let sideways = rig.stage_commit(None, &[("a.txt", "unrelated\n")], "unrelated").await;
    let cmd = RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: base.clone(),
        new_oid: sideways.clone(),
    };

    let reports = rig.run(vec![push(1, vec![cmd.clone()])]).await;
    assert!(ng_reason(&reports[0].results[0]).contains("non-fast-forward"));

    let policy = Policy {
        allow_non_fast_forward: vec!["refs/heads/*".into()],
        ..Policy::default()
    };
    let reports = batch::run_batch(&mut rig.sc, vec![push(2, vec![cmd])], &policy)
        .await
        .expect("batch");
    assert!(is_ok(&reports[0].results[0]), "{:?}", reports[0].results[0]);
    assert_eq!(rig.sc.git.ref_oid("refs/heads/main").await.unwrap(), Some(sideways));
}

/// Falsifier 6's direct-push half.
#[tokio::test]
async fn a_protected_ref_refuses_a_direct_push() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let base = rig.stage_commit(None, &[("a.txt", "base\n")], "base").await;
    let policy = Policy { protected: vec!["main".into()], ..Policy::default() };
    let reports = batch::run_batch(
        &mut rig.sc,
        vec![push(1, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: zero(),
            new_oid: base,
        }])],
        &policy,
    )
    .await
    .expect("batch");
    assert!(ng_reason(&reports[0].results[0]).contains("protected"));
    assert!(rig.sc.git.ref_oid("refs/heads/main").await.unwrap().is_none());
}

// ── merge is a push ──────────────────────────────────────────────────

/// Falsifier 3. A `refs/for/main` push merges, the target moves, and
/// the objects the SERVER created are in a pack the bucket holds — the
/// control is skipping that packing, after which the cold restore
/// cannot find the merge commit at all.
#[tokio::test]
async fn a_refs_for_push_merges_and_the_merge_survives_a_cold_restore() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let base = rig.stage_commit(None, &[("a.txt", "base\n"), ("b.txt", "b\n")], "base").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: base.clone(),
    }])])
    .await;
    // main moves…
    let main2 = rig.stage_commit(Some(&base), &[("a.txt", "main\n"), ("b.txt", "b\n")], "on main").await;
    rig.run(vec![push(2, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: base.clone(),
        new_oid: main2.clone(),
    }])])
    .await;
    // …and an agent branched from the old base and touched another file.
    let side = rig.stage_commit(Some(&base), &[("a.txt", "base\n"), ("b.txt", "side\n")], "on side").await;

    let reports = rig
        .run(vec![push(3, vec![RefUpdate {
            name: "refs/for/main".into(),
            old_oid: zero(),
            new_oid: side.clone(),
        }])])
        .await;
    let merged = match &reports[0].results[0] {
        CommandResult::Ok { alt_ref, new_oid, .. } => {
            assert_eq!(alt_ref.as_deref(), Some("refs/heads/main"));
            new_oid.clone().expect("the merge names a commit")
        }
        other => panic!("expected a merge, got {other:?}"),
    };
    assert_ne!(merged, main2, "a real merge commit, not a fast-forward");
    assert_eq!(rig.sc.git.ref_oid("refs/heads/main").await.unwrap(), Some(merged.clone()));
    assert!(
        rig.sc.git.ref_oid("refs/for/main").await.unwrap().is_none(),
        "refs/for is a request, never a ref"
    );

    let mut cold = Rig::with_store(rig.store.clone(), "cold").await;
    restore::restore(&mut cold.sc).await.expect("cold restore");
    assert_eq!(cold.sc.git.ref_oid("refs/heads/main").await.unwrap(), Some(merged));
    cold.sc.git.fsck_connectivity().await.expect("the merge must be in the bucket");
}

#[tokio::test]
async fn a_conflicting_merge_moves_no_ref_and_names_the_paths() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let base = rig.stage_commit(None, &[("a.txt", "base\n")], "base").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: base.clone(),
    }])])
    .await;
    let main2 = rig.stage_commit(Some(&base), &[("a.txt", "main side\n")], "main").await;
    rig.run(vec![push(2, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: base.clone(),
        new_oid: main2.clone(),
    }])])
    .await;
    let side = rig.stage_commit(Some(&base), &[("a.txt", "agent side\n")], "agent").await;

    let reports = rig
        .run(vec![push(3, vec![RefUpdate {
            name: "refs/for/main".into(),
            old_oid: zero(),
            new_oid: side,
        }])])
        .await;
    let why = ng_reason(&reports[0].results[0]);
    assert!(why.starts_with("conflict:"), "{why}");
    assert!(why.contains("a.txt"), "{why}");
    assert_eq!(rig.sc.git.ref_oid("refs/heads/main").await.unwrap(), Some(main2));
}

#[tokio::test]
async fn a_refs_for_push_that_fast_forwards_moves_the_target_with_no_merge_commit() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let base = rig.stage_commit(None, &[("a.txt", "base\n")], "base").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: base.clone(),
    }])])
    .await;
    let ahead = rig.stage_commit(Some(&base), &[("a.txt", "ahead\n")], "ahead").await;
    let reports = rig
        .run(vec![push(2, vec![RefUpdate {
            name: "refs/for/main".into(),
            old_oid: zero(),
            new_oid: ahead.clone(),
        }])])
        .await;
    match &reports[0].results[0] {
        CommandResult::Ok { new_oid, .. } => assert_eq!(new_oid.as_deref(), Some(ahead.as_str())),
        other => panic!("expected a fast-forward, got {other:?}"),
    }
}

/// BOOTSTRAP, found on a real cluster. A new repository has no default
/// branch, and both ways to make one refused: a direct push because
/// `main` is protected, and a merge request because there was nothing
/// to merge into. Between them `main` could never be created and the
/// repository was unusable from birth.
///
/// Every merge test above seeds `main` by direct push first, which is
/// why none of them could see this.
#[tokio::test]
async fn a_merge_request_creates_the_default_branch_when_it_does_not_exist() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let first = rig.stage_commit(None, &[("a.txt", "first\n")], "first").await;
    let reports = rig
        .run(vec![push(1, vec![RefUpdate {
            name: "refs/for/main".into(),
            old_oid: zero(),
            new_oid: first.clone(),
        }])])
        .await;
    match &reports[0].results[0] {
        CommandResult::Ok { new_oid, .. } => {
            assert_eq!(new_oid.as_deref(), Some(first.as_str()), "the proposal IS the branch")
        }
        other => panic!("a merge request must be able to create the default branch: {other:?}"),
    }
    assert_eq!(rig.sc.git.ref_oid("refs/heads/main").await.unwrap(), Some(first));
}

/// …but only the DEFAULT branch, so a merge request cannot be used to
/// conjure arbitrary refs. Without this the bootstrap fix would be a
/// general "create any ref you name" hole.
#[tokio::test]
async fn a_merge_request_into_a_missing_non_default_branch_is_still_refused() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c = rig.stage_commit(None, &[("a.txt", "x\n")], "c").await;
    let reports = rig
        .run(vec![push(1, vec![RefUpdate {
            name: "refs/for/release".into(),
            old_oid: zero(),
            new_oid: c,
        }])])
        .await;
    match &reports[0].results[0] {
        CommandResult::Ng { reason, .. } => {
            assert!(reason.contains("no such merge target"), "{reason}")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(rig.sc.git.ref_oid("refs/heads/release").await.unwrap().is_none());
}

/// `-o strategy=theirs` reaches `merge-tree -Xtheirs`, and a value the
/// client invents does not reach git at all.
#[tokio::test]
async fn a_push_option_selects_the_strategy_and_an_invented_one_is_ignored() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let base = rig.stage_commit(None, &[("a.txt", "base\n")], "base").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: base.clone(),
    }])])
    .await;
    let main2 = rig.stage_commit(Some(&base), &[("a.txt", "main side\n")], "main").await;
    rig.run(vec![push(2, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: base.clone(),
        new_oid: main2.clone(),
    }])])
    .await;
    let side = rig.stage_commit(Some(&base), &[("a.txt", "agent side\n")], "agent").await;

    let mut theirs = push(3, vec![RefUpdate {
        name: "refs/for/main".into(),
        old_oid: zero(),
        new_oid: side.clone(),
    }]);
    theirs.options = vec!["strategy=theirs".into()];
    let reports = rig.run(vec![theirs]).await;
    let merged = match &reports[0].results[0] {
        CommandResult::Ok { new_oid, .. } => new_oid.clone().unwrap(),
        other => panic!("-Xtheirs must resolve this conflict, got {other:?}"),
    };
    let content = rig.git(&["show", &format!("{merged}:a.txt")], None).await;
    assert_eq!(content, "agent side\n", "theirs is the pushed side");

    let side2 = rig.stage_commit(Some(&base), &[("a.txt", "another agent\n")], "agent2").await;
    let mut invented = push(4, vec![RefUpdate {
        name: "refs/for/main".into(),
        old_oid: zero(),
        new_oid: side2,
    }]);
    invented.options = vec!["strategy=; rm -rf /".into()];
    let reports = rig.run(vec![invented]).await;
    // No strategy reaches git, so the conflict stands and nothing was
    // executed on its behalf.
    assert!(ng_reason(&reports[0].results[0]).starts_with("conflict:"));
}

// ── the fence ────────────────────────────────────────────────────────

/// Under the writer lock a snapshot 412 can only mean a second server.
/// It stops this one — reads included — rather than retrying into a
/// repository it no longer owns.
#[tokio::test]
async fn a_snapshot_cas_refusal_fences_the_syncer() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c1 = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: c1.clone(),
    }])])
    .await;

    // Someone else wrote the snapshot: the etag we hold is stale.
    rig.store.raw_put(
        &rig.sc.cfg.snapshot_key(),
        bytes::Bytes::from_static(b"{}"),
        vec![],
    );

    let c2 = rig.stage_commit(Some(&c1), &[("a.txt", "two\n")], "second").await;
    let err = batch::run_batch(
        &mut rig.sc,
        vec![push(2, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: c1.clone(),
            new_oid: c2,
        }])],
        &Policy::default(),
    )
    .await
    .expect_err("a stale etag must fence");
    assert!(matches!(err, ForgeError::Fenced(_)), "{err:?}");
    assert!(rig.sc.fenced().is_some(), "the fence is sticky");
    assert!(rig.sc.check_fence().is_err(), "a fenced syncer serves nothing");
    // The ref did not move: the fence happened before the transaction.
    assert_eq!(rig.sc.git.ref_oid("refs/heads/main").await.unwrap(), Some(c1));
}

/// Falsifier 4. A successor rotates the snapshot before serving, so
/// the straggler's next batch 412s. Without the rotation the
/// straggler's `If-Match` is still valid and its batch would land
/// after the successor restored (lean's `LeanNoRotate`).
#[tokio::test]
async fn a_successor_rotates_and_the_straggler_fences_on_its_next_batch() {
    let mut a = Rig::new().await;
    a.start().await;
    let c1 = a.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    a.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: c1.clone(),
    }])])
    .await;
    let seq_before = a.sc.cell().unwrap().snap.seq;

    // A replacement pod: a fresh state dir, so a fresh incarnation id,
    // so the takeover path rather than self-recognition.
    let mut b = Rig::with_store(a.store.clone(), "b").await;
    let mut claimed = false;
    for _ in 0..(lease::QUIET_POLLS + 4) {
        if let lease::ClaimOutcome::Claimed(_) = lease::claim_step(&mut b.sc).await.unwrap() {
            claimed = true;
            break;
        }
    }
    assert!(claimed, "an unrenewed lease must be supersedable after the quiet polls");
    assert!(
        b.sc.cell().unwrap().snap.seq > seq_before,
        "the successor must rotate before it serves"
    );

    let c2 = a.stage_commit(Some(&c1), &[("a.txt", "two\n")], "second").await;
    let err = batch::run_batch(
        &mut a.sc,
        vec![push(2, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: c1,
            new_oid: c2,
        }])],
        &Policy::default(),
    )
    .await
    .expect_err("the straggler must not land");
    assert!(matches!(err, ForgeError::Fenced(_)), "{err:?}");
}

/// Falsifier 4 on a repository nobody has published: the successor's
/// rotation CREATES the empty snapshot, so the straggler's first CAS
/// (`If-None-Match: *`, from a belief that no snapshot exists) 412s
/// into the fence. Before this the rotation returned early here, and
/// `formal/ForgeSync.tla`'s first strict run found the straggler's
/// create landing after the successor served — and the successor's own
/// first CAS then fencing the successor. The control is that early
/// return: with it, the straggler's batch below LANDS.
#[tokio::test]
async fn a_successor_of_an_unpublished_repository_creates_the_snapshot_it_rotates() {
    let mut a = Rig::new().await;
    a.start().await;
    assert!(a.sc.cell().unwrap().etag.is_none(), "nothing published yet");
    // The straggler's push is staged but its batch has not run.
    let c1 = a.stage_commit(None, &[("a.txt", "one\n")], "first").await;

    let mut b = Rig::with_store(a.store.clone(), "b").await;
    let mut claimed = false;
    for _ in 0..(lease::QUIET_POLLS + 4) {
        if let lease::ClaimOutcome::Claimed(_) = lease::claim_step(&mut b.sc).await.unwrap() {
            claimed = true;
            break;
        }
    }
    assert!(claimed);
    let created = b.sc.cell().unwrap().clone();
    assert!(created.etag.is_some(), "the takeover must create the snapshot it could not rotate");
    assert!(created.snap.refs.is_empty() && created.snap.packs.is_empty());
    restore::restore(&mut b.sc).await.expect("the successor restores an empty repository");

    let err = batch::run_batch(
        &mut a.sc,
        vec![push(1, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: zero(),
            new_oid: c1,
        }])],
        &Policy::default(),
    )
    .await
    .expect_err("the straggler's If-None-Match create must not land over the successor");
    assert!(matches!(err, ForgeError::Fenced(_)), "{err:?}");
    // And the successor's first push lands: its belief is the cell it created.
    let c2 = b.stage_commit(None, &[("b.txt", "two\n")], "successor").await;
    let reports = b
        .run(vec![push(2, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: zero(),
            new_oid: c2.clone(),
        }])])
        .await;
    assert!(is_ok(&reports[0].results[0]), "{:?}", reports[0].results[0]);
    let cell = snapshot::load(b.store.as_ref(), &b.sc.cfg).await.unwrap();
    assert_eq!(cell.snap.oid("refs/heads/main"), Some(c2.as_str()));
}

/// Falsifier 4 across a successor's own restart: b's takeover CAS
/// landed and b died before its rotation (the two are separate store
/// requests). b's restart self-recognizes — and must rotate, because
/// the straggler a still holds a valid `If-Match` from the epoch before
/// b's. Self-recognition once skipped the rotation ("our own previous
/// process died with its writes"), and `formal/ForgeSync.tla`'s second
/// strict run found this restart letting a's batch land after b served.
/// The control is that skip: with it, a's batch below lands.
#[tokio::test]
async fn a_restarted_successor_rotates_before_it_serves() {
    let mut a = Rig::new().await;
    a.start().await;
    let c1 = a.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    a.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: c1.clone(),
    }])])
    .await;
    let seq_before = a.sc.cell().unwrap().snap.seq;
    // a's next push is staged; its batch has not run.
    let c2 = a.stage_commit(Some(&c1), &[("a.txt", "two\n")], "second").await;

    // b's takeover CAS, by hand, WITHOUT the rotation that follows it
    // in claim_step: this is b dying between the two requests.
    let key = a.sc.cfg.epoch_key();
    let state = a.store.epoch_read(&key).await.unwrap().expect("a holds");
    a.store.epoch_acquire(&key, "forge-test-b", Some(&state)).await.expect("b's takeover");

    // b comes back with its persisted id and self-recognizes.
    let mut b = Rig::with_store(a.store.clone(), "b").await;
    let outcome = lease::claim_step(&mut b.sc).await.unwrap();
    assert!(matches!(outcome, lease::ClaimOutcome::Claimed(_)), "self-recognition is immediate");
    assert!(
        b.sc.cell().unwrap().snap.seq > seq_before,
        "a restarted successor must rotate: its previous incarnation may not have"
    );
    restore::restore(&mut b.sc).await.expect("restore");

    let err = batch::run_batch(
        &mut a.sc,
        vec![push(2, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: c1,
            new_oid: c2,
        }])],
        &Policy::default(),
    )
    .await
    .expect_err("the straggler must not land after the restarted successor served");
    assert!(matches!(err, ForgeError::Fenced(_)), "{err:?}");
}

/// A 412 on the renew whose cell still names us at our own epoch is a
/// lost response, not a deposal. Fencing on it once made a live lean
/// sidecar go silent for the rest of its tenant's life.
#[tokio::test]
async fn a_lost_renew_response_is_adopted_rather_than_fenced() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let lease_before = rig.sc.lease().unwrap();
    // Renew once behind the syncer's back: the cell still names this
    // holder at this epoch, but our token is now stale.
    rig.store
        .epoch_renew(&rig.sc.cfg.epoch_key(), &lease_before, None)
        .await
        .expect("renew");
    lease::renew(&mut rig.sc).await.expect("a lost response must be adopted");
    assert!(rig.sc.fenced().is_none());
    assert_eq!(rig.sc.lease().unwrap().epoch, lease_before.epoch);
    assert_ne!(rig.sc.lease().unwrap().token, lease_before.token);
}

/// A foreign project's claim cell refuses the syncer outright, and the
/// refusal is `Refused` so the delivery treats it as final.
#[tokio::test]
async fn a_foreign_project_claim_refuses_the_syncer() {
    let mut rig = Rig::new().await;
    rig.sc.cfg.project_id = Some("mine".into());
    rig.store.raw_put(
        &rig.sc.cfg.claim_key(),
        bytes::Bytes::from(r#"{"project_id":"someone-else"}"#),
        vec![],
    );
    match lease::verify_claim(&rig.sc).await {
        Err(ForgeError::Refused(m)) => assert!(m.contains("someone-else"), "{m}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// ── the sweep ────────────────────────────────────────────────────────

/// Falsifier 10: a pack the snapshot names is never deleted; an
/// orphan past the grace is; an orphan inside the grace is not.
#[tokio::test]
async fn the_sweep_keeps_named_packs_and_takes_orphans_past_the_grace() {
    let mut rig = Rig::new().await;
    rig.sc.cfg.orphan_grace_secs = 600;
    rig.start().await;
    let c = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: c,
    }])])
    .await;
    let live = rig.sc.cell().unwrap().snap.packs.clone();
    assert_eq!(live.len(), 1);

    // Two orphans: one old enough, one not.
    let old_key = rig.sc.cfg.pack_key("pack-deadbeef00000000000000000000000000000000.pack");
    let young_key = rig.sc.cfg.pack_key("pack-cafe000000000000000000000000000000000000.pack");
    rig.store.raw_put(&old_key, bytes::Bytes::from_static(b"old"), vec![]);
    rig.store.raw_put(&young_key, bytes::Bytes::from_static(b"young"), vec![]);
    rig.store.backdate_epoch(&old_key, 3600);

    let deleted = sweep::sweep(&mut rig.sc).await.expect("sweep");
    assert_eq!(deleted, 1, "only the orphan past the grace");
    assert!(rig.store.head(&old_key).await.is_err(), "the aged orphan is gone");
    rig.store.head(&young_key).await.expect("an orphan inside the grace stays");
    for p in &live {
        rig.store.head(&rig.sc.cfg.pack_key(p)).await.expect("a named pack is never swept");
    }
}

/// Rule 1: the reference set is read AFTER the listing, and a snapshot
/// that moved aborts the pass. A sweep that judged against the older
/// snapshot could delete a pack the newer one names.
#[tokio::test]
async fn the_sweep_aborts_when_the_snapshot_moved_under_it() {
    let mut rig = Rig::new().await;
    rig.sc.cfg.orphan_grace_secs = 0;
    rig.start().await;
    let c = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: c,
    }])])
    .await;
    let orphan = rig.sc.cfg.pack_key("pack-deadbeef00000000000000000000000000000000.pack");
    rig.store.raw_put(&orphan, bytes::Bytes::from_static(b"x"), vec![]);
    rig.store.backdate_epoch(&orphan, 3600);

    // Someone republished the snapshot; our etag is no longer current.
    // The bytes must actually differ: an etag is content-derived, in
    // the memory store and in S3 alike, so a byte-identical rewrite is
    // not a move and must not be treated as one.
    let mut moved = rig.sc.cell().unwrap().snap.clone();
    moved.seq += 1;
    rig.store.raw_put(
        &rig.sc.cfg.snapshot_key(),
        bytes::Bytes::from(serde_json::to_vec(&moved).unwrap()),
        vec![],
    );
    let deleted = sweep::sweep(&mut rig.sc).await.expect("sweep");
    assert_eq!(deleted, 0, "a moved reference set aborts the pass");
    rig.store.head(&orphan).await.expect("nothing is deleted on an aborted pass");
}

/// A pack the snapshot names is re-uploaded rather than skipped when
/// the batch runs again, so its age is refreshed — `LeanChunkGC`'s
/// rule 4, without which a live pack looks like an orphan forever.
#[tokio::test]
async fn the_repack_publishes_the_new_pack_and_the_sweep_takes_the_old_ones() {
    let mut rig = Rig::new().await;
    rig.sc.cfg.repack_threshold = 1;
    rig.sc.cfg.orphan_grace_secs = 0;
    rig.start().await;
    let mut parent: Option<String> = None;
    for i in 0..3 {
        let c = rig
            .stage_commit(parent.as_deref(), &[("a.txt", &format!("{i}\n"))], &format!("c{i}"))
            .await;
        let old = parent.clone().unwrap_or_else(zero);
        rig.run(vec![push(i as u64 + 1, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: old,
            new_oid: c.clone(),
        }])])
        .await;
        parent = Some(c);
    }
    assert!(rig.sc.cell().unwrap().snap.packs.len() > 1);

    assert!(restore::maybe_repack(&mut rig.sc).await.expect("repack"));
    assert_eq!(rig.sc.cell().unwrap().snap.packs.len(), 1, "one pack after a repack");
    let deleted = sweep::sweep(&mut rig.sc).await.expect("sweep");
    assert!(deleted > 0, "the superseded packs are collected");

    let mut cold = Rig::with_store(rig.store.clone(), "cold").await;
    restore::restore(&mut cold.sc).await.expect("cold restore after a repack");
    assert_eq!(cold.sc.git.ref_oid("refs/heads/main").await.unwrap(), parent);
    cold.sc.git.fsck_connectivity().await.expect("the repacked repository must be whole");
}

// ── the dumb protocol's derived files ────────────────────────────────

#[tokio::test]
async fn the_bucket_carries_a_bare_repository_layout() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: c.clone(),
    }])])
    .await;
    let (_, refs) = rig.store.get_whole(&rig.sc.cfg.info_refs_key(), None).await.expect("info/refs");
    let refs = String::from_utf8_lossy(&refs);
    assert!(refs.contains(&c) && refs.contains("refs/heads/main"), "{refs}");
    let (_, packs) =
        rig.store.get_whole(&rig.sc.cfg.info_packs_key(), None).await.expect("objects/info/packs");
    assert!(String::from_utf8_lossy(&packs).starts_with("P pack-"));
    let (_, head) = rig.store.get_whole(&rig.sc.cfg.head_key(), None).await.expect("HEAD");
    assert_eq!(String::from_utf8_lossy(&head).trim(), "ref: refs/heads/main");
}

// ── small surfaces ───────────────────────────────────────────────────

#[test]
fn globs_match_the_way_a_refspec_does() {
    use super::policy::glob_match;
    assert!(glob_match("refs/heads/main", "refs/heads/main"));
    assert!(!glob_match("refs/heads/main", "refs/heads/mainline"));
    assert!(glob_match("release/*", "release/1.2"));
    assert!(glob_match("refs/heads/*", "refs/heads/agent/pod-7"));
    assert!(!glob_match("refs/heads/*", "refs/tags/v1"));
    assert!(glob_match("*", "anything"));
    assert!(glob_match("refs/*/main", "refs/heads/main"));
    assert!(!glob_match("refs/*/main", "refs/heads/other"));
}

#[test]
fn pkt_lines_round_trip_and_a_flush_is_a_flush() {
    let mut buf: Vec<u8> = Vec::new();
    super::pktline::write_str(&mut buf, "version=1\0push-options").unwrap();
    super::pktline::write_str(&mut buf, "old new refs/heads/main\n").unwrap();
    super::pktline::write_flush(&mut buf).unwrap();
    let mut cursor = std::io::Cursor::new(buf);
    let lines = super::pktline::read_until_flush(&mut cursor).unwrap();
    assert_eq!(lines, vec!["version=1\0push-options", "old new refs/heads/main"]);
}

/// The document the lite operator's ladder parses. The field names are
/// the contract: `hubstatus` reads camelCase and treats an unknown
/// phase as "not safe to act on", so a renamed field silently becomes
/// a hub that is never suspended — or, far worse, one that is.
#[tokio::test]
async fn the_status_document_is_the_shape_the_ladder_reads() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let facts = status::facts(&rig.sc, status::Phase::Serving);
    let doc = status::document(&facts, rig.sc.started_unix + 90);
    assert_eq!(doc["phase"], "serving");
    assert_eq!(doc["activity"]["idleSecs"], 90);
    assert_eq!(doc["rpoClean"], true);
    assert_eq!(doc["epoch"]["held"], true);
    assert!(doc["fenced"].is_null());

    let mut fenced = rig.sc;
    fenced.fence("deposed");
    let facts = status::facts(&fenced, status::Phase::Draining);
    let doc = status::document(&facts, 0);
    assert_eq!(doc["rpoClean"], false, "a deposed server proves nothing");
    assert_eq!(doc["fenced"], "deposed");
}

/// A snapshot from a newer layout is refused, never parsed for what
/// this binary happens to understand. Concluding "empty" and re-seeding
/// is the one outcome no operator can undo.
#[tokio::test]
async fn a_newer_snapshot_layout_is_refused() {
    let rig = Rig::new().await;
    let mut snap = snapshot::Snapshot::empty();
    snap.version = snapshot::SNAPSHOT_VERSION + 1;
    rig.store.raw_put(
        &rig.sc.cfg.snapshot_key(),
        bytes::Bytes::from(serde_json::to_vec(&snap).unwrap()),
        vec![],
    );
    match snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await {
        Err(ForgeError::Refused(m)) => assert!(m.contains("version"), "{m}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// ── the policy, as both enforcers read it ────────────────────────────

fn policy_json(doc: &str) -> Policy {
    serde_json::from_str(doc).expect("the rendered document must parse")
}

/// The document the operator renders is the CR's own spelling: bare
/// branch names, camelCase keys. A policy that only accepted
/// `refs/heads/main` would be a policy nobody writes correctly.
#[test]
fn the_rendered_document_is_the_crs_spelling() {
    let p = policy_json(
        r#"{
            "protected": ["main", "release/*"],
            "pushers": { "main": ["system:serviceaccount:team-a:release-bot"] },
            "mergeInto": { "main": ["system:serviceaccount:team-a:agent-runner"] },
            "agentPattern": "agent/*",
            "allowNonFastForward": ["agent/*"]
        }"#,
    );
    assert!(p.is_protected("refs/heads/main"));
    assert!(p.is_protected("refs/heads/release/1.2"));
    assert!(!p.is_protected("refs/heads/agent/pod-7"));
    assert!(p.allows_non_fast_forward("refs/heads/agent/pod-7"));
    assert!(!p.allows_non_fast_forward("refs/heads/main"));
}

#[test]
fn an_agent_may_push_its_own_shape_and_nothing_else() {
    let p = policy_json(
        r#"{
            "protected": ["main"],
            "pushers": { "main": ["release-bot"] },
            "mergeInto": { "main": ["agent-runner"] },
            "agentPattern": "agent/*"
        }"#,
    );
    let agent = "agent-runner";
    assert_eq!(p.judge(agent, "refs/heads/agent/pod-7", "abc"), Verdict::Allow);
    assert_eq!(p.judge(agent, "refs/for/main", "abc"), Verdict::Allow);
    match p.judge(agent, "refs/heads/main", "abc") {
        Verdict::Refuse(m) => assert!(m.contains("release-bot"), "{m}"),
        v => panic!("an agent must not push main directly: {v:?}"),
    }
    match p.judge(agent, "refs/heads/sneaky", "abc") {
        Verdict::Refuse(m) => assert!(m.contains("agent/*"), "{m}"),
        v => panic!("agentPattern must bound what an agent creates: {v:?}"),
    }
    // The listed pusher may move main, and may not propose merges it
    // was not listed for.
    assert_eq!(p.judge("release-bot", "refs/heads/main", "abc"), Verdict::Allow);
    assert!(matches!(p.judge("release-bot", "refs/for/main", "abc"), Verdict::Refuse(_)));
}

/// `refs/for` must not be the way around the protection it exists to
/// serve: a protected target with no `mergeInto` entry is closed, and
/// an unprotected one is open to anyone who could push it directly.
#[test]
fn a_protected_target_with_no_merge_list_is_closed_and_an_open_one_is_not() {
    let closed = policy_json(r#"{"protected": ["main"]}"#);
    match closed.judge("anyone", "refs/for/main", "abc") {
        Verdict::Refuse(m) => assert!(m.contains("mergeInto"), "{m}"),
        v => panic!("expected a refusal, got {v:?}"),
    }
    let open = policy_json(r#"{}"#);
    assert_eq!(open.judge("anyone", "refs/for/topic", "abc"), Verdict::Allow);
}

/// A protected ref is moved by its pushers and deleted by nobody.
#[test]
fn a_protected_ref_is_never_deleted_through_the_server() {
    let p = policy_json(r#"{"protected": ["main"], "pushers": {"main": ["*"]}}"#);
    assert_eq!(p.judge("anyone", "refs/heads/main", "abc"), Verdict::Allow);
    match p.judge("anyone", "refs/heads/main", &zero()) {
        Verdict::Refuse(m) => assert!(m.contains("never deleted"), "{m}"),
        v => panic!("expected a refusal, got {v:?}"),
    }
}

/// An empty principal is a deployment with no door in front of it. The
/// policy still applies and every named list fails to contain it, so a
/// protected ref stays protected rather than becoming open.
#[test]
fn an_unauthenticated_push_is_not_a_privileged_one() {
    let p = policy_json(r#"{"protected": ["main"], "pushers": {"main": ["release-bot"]}}"#);
    assert!(matches!(p.judge("", "refs/heads/main", "abc"), Verdict::Refuse(_)));
}

/// Absent is permissive (the pre-operator posture); unreadable is an
/// error, because a rendering bug must never read as "no policy".
#[test]
fn an_absent_policy_is_permissive_and_an_unparseable_one_is_not() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(Policy::load(dir.path()).unwrap(), None);
    std::fs::write(dir.path().join(super::policy::POLICY_FILE), b"{not json").unwrap();
    assert!(Policy::load(dir.path()).is_err());
}

/// Defence in depth, at the writer. The syncer applies the same
/// document the hook applied, so a repository whose hooks were
/// misconfigured — a wrong `core.hooksPath`, a missing binary, an image
/// rolled without one — still refuses a push to a protected ref.
#[tokio::test]
async fn the_syncer_refuses_a_protected_push_with_no_hook_in_the_picture() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let base = rig.stage_commit(None, &[("a.txt", "base\n")], "base").await;
    let policy = policy_json(
        r#"{"protected": ["main"], "pushers": {"main": ["release-bot"]}, "mergeInto": {"main": ["agent-runner"]}}"#,
    );
    let mut agent = push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: base.clone(),
    }]);
    agent.principal = "agent-runner".into();
    let reports = batch::run_batch(&mut rig.sc, vec![agent], &policy).await.expect("batch");
    assert!(ng_reason(&reports[0].results[0]).contains("release-bot"));
    assert!(rig.sc.git.ref_oid("refs/heads/main").await.unwrap().is_none());

    let mut bot = push(2, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: base.clone(),
    }]);
    bot.principal = "release-bot".into();
    let reports = batch::run_batch(&mut rig.sc, vec![bot], &policy).await.expect("batch");
    assert!(is_ok(&reports[0].results[0]), "{:?}", reports[0].results[0]);
    assert_eq!(rig.sc.git.ref_oid("refs/heads/main").await.unwrap(), Some(base));
}

// ── the legible export (§9) ──────────────────────────────────────────

fn export_cfg(dir: &std::path::Path) -> super::export::ExportConfig {
    super::export::ExportConfig {
        reference: "refs/heads/main".into(),
        prefix: "tenant/export".into(),
        every_secs: 300,
        bucket: "bkt".into(),
        endpoint: None,
        sync_bin: "/usr/local/bin/flint-sync".into(),
        timeout_secs: 300,
        root: dir.join("export/tree"),
        index: dir.join("export/index"),
        project_id: Some("proj".into()),
    }
}

/// Falsifier 9's first half, decided locally: every file in the
/// exported tree is byte-identical to `git show <ref>:<path>`. The
/// bucket half needs a real barrier and a real store; what a unit test
/// can decide is that the TREE forge hands to lean is the ref's tree.
#[tokio::test]
async fn the_exported_tree_is_the_refs_tree() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let files = [("a.txt", "alpha\n"), ("b.txt", "beta\n"), ("c.txt", "gamma\n")];
    let c = rig.stage_commit(None, &files, "first").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: c.clone(),
    }])])
    .await;

    let cfg = export_cfg(rig._dir.path());
    super::export::materialize(&rig.sc.git, &cfg, None, &c).await.expect("materialize");
    for (name, content) in files {
        let on_disk = std::fs::read_to_string(cfg.root.join(name)).expect(name);
        assert_eq!(on_disk, content, "{name}");
        let from_git = rig.git(&["show", &format!("{c}:{name}")], None).await;
        assert_eq!(on_disk, from_git, "{name} must be byte-identical to what git holds");
    }
}

/// The half of falsifier 9 that the design's own first draft got
/// wrong. `git archive | tar -x` rewrites every file and leaves deleted
/// paths behind; the two-tree update touches exactly what changed AND
/// removes what the new tree no longer has. A stale file left behind is
/// a file the export publishes forever.
#[tokio::test]
async fn an_incremental_export_touches_only_what_changed_and_removes_what_is_gone() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let first = rig
        .stage_commit(
            None,
            &[("keep.txt", "same\n"), ("edit.txt", "before\n"), ("gone.txt", "doomed\n")],
            "first",
        )
        .await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: first.clone(),
    }])])
    .await;
    let cfg = export_cfg(rig._dir.path());
    super::export::materialize(&rig.sc.git, &cfg, None, &first).await.expect("first");

    // Mark the file that must not be rewritten. If the update touches
    // it, the marker's mtime moves — and lean's next scan would read
    // the whole tree as changed and re-upload it.
    let keep = cfg.root.join("keep.txt");
    let before = std::fs::metadata(&keep).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let second = rig
        .stage_commit(
            Some(&first),
            &[("keep.txt", "same\n"), ("edit.txt", "after\n"), ("new.txt", "fresh\n")],
            "second",
        )
        .await;
    rig.run(vec![push(2, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: first.clone(),
        new_oid: second.clone(),
    }])])
    .await;
    super::export::materialize(&rig.sc.git, &cfg, Some(&first), &second)
        .await
        .expect("incremental");

    assert_eq!(std::fs::read_to_string(cfg.root.join("edit.txt")).unwrap(), "after\n");
    assert_eq!(std::fs::read_to_string(cfg.root.join("new.txt")).unwrap(), "fresh\n");
    assert!(
        !cfg.root.join("gone.txt").exists(),
        "a path the new tree does not have must be REMOVED, not left to be published forever"
    );
    assert_eq!(
        std::fs::metadata(&keep).unwrap().modified().unwrap(),
        before,
        "an unchanged file must not be rewritten, or the next barrier re-uploads the whole tree"
    );
}

/// Lean's own state directory lives inside the exported tree and is its
/// baseline. Clearing the tree for a full re-materialise must not take
/// it: losing it costs one full re-upload, and losing it on every
/// export would make the export O(everything) forever.
#[tokio::test]
async fn a_full_rematerialise_keeps_leans_baseline_and_drops_stale_files() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let first = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: first.clone(),
    }])])
    .await;
    let cfg = export_cfg(rig._dir.path());
    super::export::materialize(&rig.sc.git, &cfg, None, &first).await.expect("first");

    std::fs::create_dir_all(cfg.root.join(".flint-sync")).unwrap();
    std::fs::write(cfg.root.join(".flint-sync/baseline"), b"lean's own").unwrap();
    std::fs::write(cfg.root.join("stale.txt"), b"left over").unwrap();

    // No index and no `from` — the shape a pod restart leaves.
    std::fs::remove_file(&cfg.index).ok();
    let second = rig.stage_commit(Some(&first), &[("a.txt", "two\n")], "second").await;
    super::export::materialize(&rig.sc.git, &cfg, None, &second).await.expect("full");

    assert_eq!(std::fs::read_to_string(cfg.root.join("a.txt")).unwrap(), "two\n");
    assert!(!cfg.root.join("stale.txt").exists(), "the clear must take a stale file");
    assert_eq!(
        std::fs::read_to_string(cfg.root.join(".flint-sync/baseline")).unwrap(),
        "lean's own",
        "lean's baseline is not ours to delete"
    );
}

/// The cadence floor and the already-exported check, which together
/// decide whether a push pays for an export at all.
#[test]
fn the_export_runs_on_a_floor_and_never_twice_for_one_commit() {
    use super::export::{plan, Plan, Record};
    let dir = tempfile::tempdir().unwrap();
    let cfg = export_cfg(dir.path());

    let never = Record::default();
    assert_eq!(
        plan(&cfg, Some("abc"), &never, 1000),
        Plan::Run { from: None, to: "abc".into() },
        "a repository that has never exported exports"
    );

    let done =
        Record { commit: Some("abc".into()), unix: 900, blocked_unix: 0, blocked_streak: 0 };
    assert!(matches!(plan(&cfg, Some("abc"), &done, 5000), Plan::Skip(_)), "same commit");
    assert!(
        matches!(plan(&cfg, Some("def"), &done, 1000), Plan::Skip(_)),
        "a new commit inside the floor waits for the next batch"
    );
    assert_eq!(
        plan(&cfg, Some("def"), &done, 1300),
        Plan::Run { from: Some("abc".into()), to: "def".into() },
        "past the floor it runs, and it knows which tree it is coming from"
    );
    assert!(matches!(plan(&cfg, None, &never, 1000), Plan::Skip(_)), "an absent ref");
}

/// The backoff after an abandoned barrier.
///
/// Without it the timeout only changes the SHAPE of the outage that
/// composition drill C2 measured: the serving loop would re-enter the
/// doomed barrier on the next batch and spend the whole timeout again,
/// which is the same repository-down, one batch at a time.
#[test]
fn an_abandoned_barrier_waits_out_a_floor_before_it_is_retried() {
    use super::export::{plan, Plan, Record};
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = export_cfg(dir.path());
    cfg.every_secs = 0; // the operator asked for "export as often as you can"
    cfg.timeout_secs = 60;

    // Even with NO cadence floor, a barrier abandoned at t=1000 is not
    // retried at t=1030.
    let blocked =
        Record { commit: None, unix: 0, blocked_unix: 1000, blocked_streak: 1 };
    assert!(
        matches!(plan(&cfg, Some("abc"), &blocked, 1030), Plan::Skip(_)),
        "a blocked export must not be re-entered on the very next batch"
    );
    assert_eq!(
        plan(&cfg, Some("abc"), &blocked, 1061),
        Plan::Run { from: None, to: "abc".into() },
        "past its own floor it tries again — the blocker may have gone"
    );

    // The ladder. A flat hold-off of one timeout would leave forge
    // blocked one timeout in every two for as long as the
    // misconfiguration stands, which is a 50% outage with better
    // manners. Doubling makes the standing fault nearly free.
    use super::export::backoff_secs;
    assert_eq!(backoff_secs(&cfg, 0), 60, "before any failure, one timeout");
    assert_eq!(backoff_secs(&cfg, 1), 60);
    assert_eq!(backoff_secs(&cfg, 2), 120);
    assert_eq!(backoff_secs(&cfg, 3), 240);
    assert_eq!(backoff_secs(&cfg, 30), 3600, "capped, and it does not overflow");
    let mut deep = blocked.clone();
    deep.blocked_streak = 4;
    assert!(
        matches!(plan(&cfg, Some("abc"), &deep, 1400), Plan::Skip(_)),
        "the fourth failure in a row holds off longer than the first"
    );
}

/// A record written before `blocked_unix` existed must still parse. If
/// it did not, an upgrade would read every export as "never ran" and
/// re-export the whole tree from scratch.
#[test]
fn an_export_record_from_before_the_timeout_still_parses() {
    let r: super::export::Record =
        serde_json::from_str(r#"{"commit":"abc","unix":900}"#).expect("old record parses");
    assert_eq!(r.commit.as_deref(), Some("abc"));
    assert_eq!(r.unix, 900);
    assert_eq!(r.blocked_unix, 0, "an old record is not treated as blocked");
}

/// The timeout itself, against a real child that never returns.
///
/// Two things are asserted, and the second is the one that matters:
/// the call comes back, AND the child is dead. An abandoned barrier
/// left running would hold whatever it claimed and be joined by a
/// fresh one on the next attempt.
#[tokio::test]
async fn a_barrier_that_never_returns_is_killed_and_named() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = export_cfg(dir.path());
    let marker = dir.path().join("the-child-outlived-the-timeout");
    let script = dir.path().join("hang.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nsleep 4\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    cfg.sync_bin = script;
    cfg.timeout_secs = 1;

    let t = std::time::Instant::now();
    let err = super::export::run_barrier(&cfg).await.expect_err("it must not hang");
    assert!(
        t.elapsed() < std::time::Duration::from_secs(3),
        "it returned on the timeout, not on the child ({:?})",
        t.elapsed()
    );
    match &err {
        super::ForgeError::ExportBlocked(m) => {
            assert!(
                m.contains("tenant/export"),
                "the message sends the operator to the prefix: {m}"
            );
            assert!(m.contains("SECOND WRITER"), "it names the usual cause: {m}");
        }
        other => panic!("a timeout must be ExportBlocked, not {other:?}"),
    }

    // The child must be DEAD, not merely abandoned.
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    assert!(
        !marker.exists(),
        "the abandoned barrier kept running past the timeout"
    );
}

/// Everything load-bearing about the export is in this environment. A
/// missing variable is a workspace published to the wrong prefix, or a
/// project's tree overwritten because the claim check never ran.
#[test]
fn the_barrier_command_carries_the_workspace_it_publishes() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = export_cfg(dir.path());
    cfg.endpoint = Some("http://minio:9000".into());
    let (bin, args, env) = super::export::barrier_command(&cfg);
    assert!(bin.ends_with("flint-sync"));
    assert_eq!(args, vec!["barrier".to_string()]);
    let map: std::collections::BTreeMap<_, _> = env.into_iter().collect();
    assert_eq!(map["FLINT_SYNC_BUCKET"], "bkt");
    assert_eq!(map["FLINT_SYNC_PREFIX"], "tenant/export");
    assert_eq!(map["FLINT_SYNC_ROOT"], cfg.root.to_string_lossy());
    assert_eq!(map["FLINT_SYNC_ENDPOINT"], "http://minio:9000");
    assert_eq!(
        map["FLINT_SYNC_PROJECT_ID"], "proj",
        "without it an export would overwrite another project's workspace"
    );
    assert_eq!(
        map["FLINT_SYNC_SOLE_WRITER"], "true",
        "the export is a mirror; without this a reader adopts a foreign write (C4)"
    );
    // The credentials are INHERITED, never rebuilt here: one place for
    // them to be wrong instead of two.
    assert!(!map.contains_key("AWS_ACCESS_KEY_ID"));
}

/// The export never writes the snapshot. It stashes its commit and the
/// NEXT batch's single CAS carries it — a second writer of the one
/// object the design says has exactly one is the whole thing this
/// avoids.
#[tokio::test]
async fn the_exported_commit_rides_the_next_batch_rather_than_a_cas_of_its_own() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c1 = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: c1.clone(),
    }])])
    .await;
    let seq_after_push = rig.sc.cell().unwrap().snap.seq;
    assert_eq!(rig.sc.cell().unwrap().snap.exported_commit, None);

    // An export happened.
    rig.sc.pending_exported_commit = Some(c1.clone());
    assert_eq!(
        snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await.unwrap().snap.seq,
        seq_after_push,
        "stashing an exported commit must not write the snapshot"
    );

    let c2 = rig.stage_commit(Some(&c1), &[("a.txt", "two\n")], "second").await;
    rig.run(vec![push(2, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: c1.clone(),
        new_oid: c2,
    }])])
    .await;
    let cell = snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await.unwrap();
    assert_eq!(cell.snap.exported_commit.as_deref(), Some(c1.as_str()));
    assert_eq!(cell.snap.seq, seq_after_push + 1, "one CAS, not two");
    assert!(rig.sc.pending_exported_commit.is_none(), "taken, not re-written every batch");
}

// ── the fleet levers (§8) ────────────────────────────────────────────

/// A bundle is cut, uploaded, advertised, and named by the NEXT
/// snapshot — never by a CAS of its own. The advertisement is the part
/// a stock client ignores unless it opted in, so what is asserted here
/// is that the server's half is complete and correct.
#[tokio::test]
async fn a_bundle_is_cut_uploaded_and_advertised() {
    use super::bundle::{self, BundleConfig};
    let mut rig = Rig::new().await;
    rig.start().await;
    let c = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: c.clone(),
    }])])
    .await;
    let seq_after_push = rig.sc.cell().unwrap().snap.seq;

    let cfg = BundleConfig { every_secs: 3600, url_ttl_secs: 600 };
    let name = bundle::maybe_run(&mut rig.sc, &cfg, 1_000_000)
        .await
        .expect("bundle")
        .expect("one was due");
    assert_eq!(name, format!("{c}.bundle"), "named by the tip it carries");
    rig.store
        .head(&rig.sc.cfg.bundle_key(&name))
        .await
        .expect("the bundle must be in the bucket before it is advertised");

    // The advertisement upload-pack reads.
    let cfg_get = |k: &str| {
        let git = rig.sc.git.clone();
        let k = k.to_string();
        async move { git.must(&["config", "--get", &k], None).await.unwrap().trim().to_string() }
    };
    assert_eq!(cfg_get("uploadpack.advertiseBundleURIs").await, "true");
    assert_eq!(cfg_get("bundle.version").await, "1");
    assert_eq!(cfg_get("bundle.mode").await, "all");
    let uri = cfg_get(&format!("bundle.{}.uri", bundle::BUNDLE_ID)).await;
    assert!(uri.contains(&name), "the advertised URL must name the bundle: {uri}");

    // Not written to the snapshot yet…
    assert_eq!(
        snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await.unwrap().snap.seq,
        seq_after_push,
        "cutting a bundle must not spend a CAS"
    );
    rig.sc.pending_bundle = Some(name.clone());
    let c2 = rig.stage_commit(Some(&c), &[("a.txt", "two\n")], "second").await;
    rig.run(vec![push(2, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: c.clone(),
        new_oid: c2,
    }])])
    .await;
    // …and named by the next one.
    let cell = snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await.unwrap();
    assert_eq!(cell.snap.bundles, vec![name]);
    assert_eq!(cell.snap.seq, seq_after_push + 1, "one CAS, not two");
}

/// THE DEFECT THE FIRST CLUSTER RUN FOUND. The advertisement lives in
/// the repository's LOCAL git config and the bundle lives in the
/// bucket, so a restore came back serving a repository whose bundle
/// existed, was paid for, and was advertised to nobody — until
/// `every_secs` elapsed and a new one was cut.
///
/// For forge that window is not an edge case: a repository that idles
/// to zero restores at the moment a clone storm wakes it, which is
/// exactly when the lever is meant to be pulled. On the cluster the
/// config came back empty while the snapshot still named the bundle.
#[tokio::test]
async fn a_restore_re_advertises_the_bundle_the_snapshot_names() {
    use super::bundle::{self, BundleConfig};
    let store = Arc::new(MemoryStore::new());
    let cfg = BundleConfig { every_secs: 3600, url_ttl_secs: 600 };
    let name = {
        let mut rig = Rig::with_store(store.clone(), "a").await;
        rig.start().await;
        let c = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
        rig.run(vec![push(1, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: zero(),
            new_oid: c.clone(),
        }])])
        .await;
        let name = bundle::maybe_run(&mut rig.sc, &cfg, 1_000_000)
            .await
            .expect("bundle")
            .expect("one was due");
        // Carry it into the snapshot, as the next batch's CAS does.
        rig.sc.pending_bundle = Some(name.clone());
        let c2 = rig.stage_commit(Some(&c), &[("a.txt", "two\n")], "second").await;
        rig.run(vec![push(2, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: c,
            new_oid: c2,
        }])])
        .await;
        name
    };

    // A NEW server on a NEW empty disk — a wake from idle-to-zero.
    let mut cold = Rig::with_store(store.clone(), "b").await;
    cold.start().await;
    assert_eq!(
        cold.sc.cell().unwrap().snap.bundles,
        vec![name.clone()],
        "the snapshot is the durable record and must still name it"
    );

    // `Rig::start` restores but is not the server's startup path, so
    // drive the same call the server makes.
    bundle::readvertise(&mut cold.sc, &cfg, 2_000_000).await.expect("re-advertise");

    let git = cold.sc.git.clone();
    let get = |k: &str| {
        let git = git.clone();
        let k = k.to_string();
        async move { git.run(&["config", "--get", &k], None).await.unwrap() }
    };
    assert_eq!(get("uploadpack.advertiseBundleURIs").await.stdout.trim(), "true");
    let uri = get(&format!("bundle.{}.uri", bundle::BUNDLE_ID)).await.stdout;
    assert!(
        uri.contains(&name),
        "a restored server must hand out the bundle the snapshot names, not nothing: {uri:?}"
    );
}

/// THE EXPORT'S BASELINE MUST OUTLIVE THE POD.
///
/// lean parks a file rather than overwriting bytes whose etag it did
/// not last write. With the baseline on the pod's emptyDir, the first
/// restart made every object in the export prefix foreign — so every
/// upload 412'd, every file parked, and the published workspace froze
/// for good while main moved on. The cluster run found README.md still
/// holding the first seed's text, 164 files parked, `up=0`.
#[tokio::test]
async fn the_export_baseline_survives_a_pod_that_does_not() {
    use super::export::{preserve_baseline, rehydrate_baseline, ExportConfig};
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().expect("tmp");
    let cfg = ExportConfig {
        reference: "refs/heads/main".into(),
        prefix: "p/export".into(),
        every_secs: 30,
        bucket: "b".into(),
        endpoint: None,
        sync_bin: std::path::PathBuf::from("/bin/true"),
        timeout_secs: 300,
        root: dir.path().join("tree"),
        index: dir.path().join("index"),
        project_id: None,
    };
    let key = "p/git/export-baseline.json";
    let bp = cfg.root.join(".flint-sync").join("baseline.json");

    // Nothing saved yet: rehydrate is a no-op, not an error.
    assert!(!rehydrate_baseline(store.as_ref(), key, &cfg).await.unwrap());

    std::fs::create_dir_all(bp.parent().unwrap()).unwrap();
    std::fs::write(&bp, br#"{"files":{"a.txt":"etag-1"}}"#).unwrap();
    preserve_baseline(store.as_ref(), key, &cfg, 7).await.expect("preserve");

    // A live pod keeps its own: rehydrate must not clobber it.
    std::fs::write(&bp, br#"{"files":{"a.txt":"etag-2"}}"#).unwrap();
    assert!(
        !rehydrate_baseline(store.as_ref(), key, &cfg).await.unwrap(),
        "a baseline that is already present must never be overwritten from the bucket"
    );
    assert!(std::fs::read_to_string(&bp).unwrap().contains("etag-2"));

    // A barrier succeeds, so the newer baseline is saved too.
    preserve_baseline(store.as_ref(), key, &cfg, 8).await.expect("preserve again");

    // The pod dies: the emptyDir goes with it.
    std::fs::remove_dir_all(cfg.root.join(".flint-sync")).unwrap();
    assert!(
        rehydrate_baseline(store.as_ref(), key, &cfg).await.unwrap(),
        "a fresh pod must get its baseline back, or every file parks forever"
    );
    assert!(
        std::fs::read_to_string(&bp).unwrap().contains("etag-2"),
        "the LAST preserved baseline must come back, not an older one"
    );
}

/// The floor, the already-cut check, and the re-sign clock. A bundle is
/// a full copy of the repository, so cutting one per push would spend
/// more than the storm it saves.
#[test]
fn a_bundle_is_cut_on_a_floor_and_re_signed_on_half_its_ttl() {
    use super::bundle::{needs_resign, plan, BundleConfig, Plan, Record};
    let cfg = BundleConfig { every_secs: 3600, url_ttl_secs: 600 };
    let never = Record::default();
    assert_eq!(plan(&cfg, Some("abc"), &never, 100), Plan::Cut { tip: "abc".into() });
    assert!(matches!(plan(&cfg, None, &never, 100), Plan::Skip(_)), "no default branch");

    let cut = Record {
        tip: Some("abc".into()),
        name: Some("abc.bundle".into()),
        cut_unix: 1000,
        signed_unix: 1000,
    };
    assert!(matches!(plan(&cfg, Some("abc"), &cut, 9000), Plan::Skip(_)), "same tip");
    assert!(matches!(plan(&cfg, Some("def"), &cut, 2000), Plan::Skip(_)), "inside the floor");
    assert_eq!(plan(&cfg, Some("def"), &cut, 5000), Plan::Cut { tip: "def".into() });

    // Re-signed at half the TTL, so a client that takes the
    // advertisement and then takes its time still has a live URL.
    assert!(!needs_resign(&cfg, &cut, 1200));
    assert!(needs_resign(&cfg, &cut, 1300));
    assert!(!needs_resign(&cfg, &Record::default(), 999_999), "nothing to re-sign");
}

/// A swept bundle must not stay advertised: a client handed a URL that
/// 404s pays a failed fetch before falling back to the server.
#[tokio::test]
async fn the_sweep_keeps_the_advertised_bundle_and_takes_the_old_ones() {
    use super::bundle::{self, BundleConfig};
    let mut rig = Rig::new().await;
    rig.sc.cfg.orphan_grace_secs = 600;
    rig.start().await;
    let c = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: c.clone(),
    }])])
    .await;
    let cfg = BundleConfig { every_secs: 0, url_ttl_secs: 600 };
    let live = bundle::maybe_run(&mut rig.sc, &cfg, 1_000).await.unwrap().unwrap();
    rig.sc.pending_bundle = Some(live.clone());
    let c2 = rig.stage_commit(Some(&c), &[("a.txt", "two\n")], "second").await;
    rig.run(vec![push(2, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: c.clone(),
        new_oid: c2,
    }])])
    .await;

    // An older bundle, past the grace.
    let stale = rig.sc.cfg.bundle_key("deadbeef.bundle");
    rig.store.raw_put(&stale, bytes::Bytes::from_static(b"old"), vec![]);
    rig.store.backdate_epoch(&stale, 3600);

    let deleted = sweep::sweep(&mut rig.sc).await.expect("sweep");
    assert_eq!(deleted, 1);
    assert!(rig.store.head(&stale).await.is_err(), "the aged bundle is collected");
    rig.store
        .head(&rig.sc.cfg.bundle_key(&live))
        .await
        .expect("the bundle the snapshot names is never swept");
}

/// The pruner's rule, and the half of it that matters: a branch that is
/// NOT contained in the integration branch is somebody's unfinished
/// work, and no clock may take it.
#[tokio::test]
async fn pruning_takes_merged_quiet_branches_and_never_unmerged_ones() {
    use super::prune::{candidates, PruneConfig};
    let mut rig = Rig::new().await;
    rig.start().await;
    let base = rig.stage_commit(None, &[("a.txt", "base\n")], "base").await;
    let merged = rig.stage_commit(Some(&base), &[("a.txt", "merged\n")], "merged").await;
    let orphan = rig.stage_commit(Some(&base), &[("b.txt", "unfinished\n")], "orphan").await;
    rig.run(vec![push(1, vec![
        RefUpdate { name: "refs/heads/main".into(), old_oid: zero(), new_oid: merged.clone() },
        RefUpdate {
            name: "refs/heads/agent/done".into(),
            old_oid: zero(),
            new_oid: merged.clone(),
        },
        RefUpdate {
            name: "refs/heads/agent/busy".into(),
            old_oid: zero(),
            new_oid: orphan.clone(),
        },
        RefUpdate { name: "refs/heads/keepme".into(), old_oid: zero(), new_oid: merged.clone() },
    ])])
    .await;

    let cfg = PruneConfig {
        pattern: "refs/heads/agent/*".into(),
        after_secs: 0,
        into: "refs/heads/main".into(),
        every_secs: 86_400,
    };
    let dead = candidates(&rig.sc, &cfg, super::now_unix() + 10_000).await.expect("candidates");
    let names: Vec<&str> = dead.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["refs/heads/agent/done"], "merged and quiet, and only that");
    assert!(dead[0].new_oid.bytes().all(|b| b == b'0'), "a prune is a delete");

    // Inside the TTL, nothing is taken even though it is merged: a
    // merge that just landed must not delete the branch out from under
    // the agent still pushing to it.
    let fresh = PruneConfig { after_secs: 86_400, ..cfg.clone() };
    assert!(candidates(&rig.sc, &fresh, super::now_unix()).await.unwrap().is_empty());

    // And with no integration branch there is nothing to be contained
    // in, so nothing is prunable at all.
    let nowhere = PruneConfig { into: "refs/heads/nosuch".into(), ..cfg };
    assert!(candidates(&rig.sc, &nowhere, super::now_unix() + 10_000).await.unwrap().is_empty());
}

/// The prune's deletions travel the ordinary batch: the same staleness
/// check, the same CAS, the same transaction. A ref this process moved
/// outside that path would be a ref the bucket does not know about.
#[tokio::test]
async fn a_prune_is_a_push_and_the_bucket_learns_about_it() {
    use super::prune::{candidates, PruneConfig};
    let mut rig = Rig::new().await;
    rig.start().await;
    let c = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    rig.run(vec![push(1, vec![
        RefUpdate { name: "refs/heads/main".into(), old_oid: zero(), new_oid: c.clone() },
        RefUpdate { name: "refs/heads/agent/old".into(), old_oid: zero(), new_oid: c.clone() },
    ])])
    .await;
    assert!(snapshot::load(rig.store.as_ref(), &rig.sc.cfg)
        .await
        .unwrap()
        .snap
        .refs
        .contains_key("refs/heads/agent/old"));

    let cfg = PruneConfig {
        pattern: "refs/heads/agent/*".into(),
        after_secs: 0,
        into: "refs/heads/main".into(),
        every_secs: 86_400,
    };
    let dead = candidates(&rig.sc, &cfg, super::now_unix() + 10_000).await.unwrap();
    let mut p = push(9, dead);
    p.principal = "system:flint-forge".into();
    let reports = rig.run(vec![p]).await;
    assert!(is_ok(&reports[0].results[0]), "{:?}", reports[0].results[0]);

    let cell = snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await.unwrap();
    assert!(!cell.snap.refs.contains_key("refs/heads/agent/old"), "the bucket learns of the delete");
    assert!(cell.snap.refs.contains_key("refs/heads/main"));
    assert!(rig.sc.git.ref_oid("refs/heads/agent/old").await.unwrap().is_none());
}

// ── git LFS (§14 phase 6) ────────────────────────────────────────────

fn lfs_oid(n: u8) -> String {
    format!("{:02x}", n).repeat(32)
}

fn batch_req(op: &str, objects: &[(&str, u64)]) -> super::lfs::BatchRequest {
    serde_json::from_value(serde_json::json!({
        "operation": op,
        "transfers": ["basic"],
        "hash_algo": "sha256",
        "objects": objects
            .iter()
            .map(|(oid, size)| serde_json::json!({"oid": oid, "size": size}))
            .collect::<Vec<_>>(),
    }))
    .expect("a batch request git-lfs would send")
}

/// The download path: an object the bucket holds is handed back as a
/// presigned URL, and one it does not is a 404 ON THAT OBJECT rather
/// than a failure of the whole batch — which is what lets a client
/// fetch nine of ten and be told precisely which one is missing.
#[tokio::test]
async fn a_download_batch_presigns_what_is_there_and_404s_what_is_not() {
    let rig = Rig::new().await;
    let here = lfs_oid(0xab);
    let gone = lfs_oid(0xcd);
    rig.store.raw_put(
        &super::lfs::object_key(&rig.sc.cfg.prefix, &here),
        bytes::Bytes::from_static(b"weights"),
        vec![],
    );

    let res = super::lfs::batch(
        rig.store.as_ref(),
        &rig.sc.cfg.prefix,
        &batch_req("download", &[(&here, 7), (&gone, 99)]),
        600,
    )
    .await
    .expect("the batch itself succeeds");
    assert_eq!(res.transfer, "basic");

    let ok = &res.objects[0];
    assert!(ok.error.is_none());
    let href = &ok.actions["download"].href;
    assert!(href.contains(&here), "the URL must name the object: {href}");
    assert_eq!(ok.actions["download"].expires_in, 600);
    assert!(ok.authenticated, "the client already authenticated at the door");

    let missing = &res.objects[1];
    assert!(missing.actions.is_empty());
    assert_eq!(missing.error.as_ref().unwrap().code, 404);
}

/// The dedupe that makes LFS cheap: an object already in the bucket
/// gets NO actions, which is how the protocol says "you already have
/// this". A rebased branch re-pushing the same checkpoint uploads
/// nothing.
#[tokio::test]
async fn an_upload_batch_offers_a_url_only_for_what_is_missing() {
    let rig = Rig::new().await;
    let have = lfs_oid(0x11);
    let want = lfs_oid(0x22);
    rig.store.raw_put(
        &super::lfs::object_key(&rig.sc.cfg.prefix, &have),
        bytes::Bytes::from_static(b"already"),
        vec![],
    );

    let res = super::lfs::batch(
        rig.store.as_ref(),
        &rig.sc.cfg.prefix,
        &batch_req("upload", &[(&have, 7), (&want, 4096)]),
        600,
    )
    .await
    .unwrap();

    assert!(
        res.objects[0].actions.is_empty(),
        "an object already in the bucket must be offered no upload at all"
    );
    assert!(res.objects[0].error.is_none());

    let fresh = &res.objects[1];
    assert!(fresh.actions.contains_key("upload"), "{:?}", fresh.actions);
    assert!(fresh.actions["upload"].href.contains(&want));
    assert!(
        fresh.actions.contains_key("verify"),
        "without verify a failed PUT is silently accepted — the bytes never came past us"
    );
}

/// The oid becomes an S3 KEY, so this is the boundary that stops a
/// traversal or a newline from reaching one. Nothing but 64 lower-case
/// hex characters is an oid.
#[test]
fn only_a_sha256_in_lower_case_hex_is_an_oid() {
    use super::lfs::{object_key, valid_oid};
    assert!(valid_oid(&lfs_oid(0xab)));
    assert!(!valid_oid(""), "empty");
    assert!(!valid_oid(&"a".repeat(63)), "short");
    assert!(!valid_oid(&"A".repeat(64)), "upper case would be a second key for one object");
    assert!(!valid_oid("../../etc/passwd"), "traversal");
    assert!(!valid_oid(&format!("{}\n", "a".repeat(63))), "newline");
    assert_eq!(
        object_key("tenant/repo/", &lfs_oid(0x0f)),
        format!("tenant/repo/lfs/objects/{}", lfs_oid(0x0f))
    );
}

/// A bad oid is refused per object, not per batch, and the refusal
/// never reaches the store.
#[tokio::test]
async fn a_malformed_oid_is_refused_without_touching_the_store() {
    let rig = Rig::new().await;
    rig.store.reset_op_counts();
    let res = super::lfs::batch(
        rig.store.as_ref(),
        &rig.sc.cfg.prefix,
        &batch_req("download", &[("../../etc/passwd", 1)]),
        600,
    )
    .await
    .unwrap();
    assert_eq!(res.objects[0].error.as_ref().unwrap().code, 422);
    assert_eq!(rig.store.total_ops(), 0, "a malformed oid must never become a key");
}

/// The whole request is refused when the client asks for something
/// this server does not do — a different hash algorithm, a transfer
/// that presigned URLs cannot serve, or an operation that is neither.
#[tokio::test]
async fn a_batch_this_server_cannot_serve_is_refused_whole() {
    let rig = Rig::new().await;
    let mut sha1 = batch_req("download", &[(&lfs_oid(1), 1)]);
    sha1.hash_algo = Some("sha1".into());
    assert!(super::lfs::batch(rig.store.as_ref(), "p", &sha1, 600).await.is_err());

    let mut exotic = batch_req("download", &[(&lfs_oid(1), 1)]);
    exotic.transfers = vec!["tus".into(), "multipart".into()];
    assert!(super::lfs::batch(rig.store.as_ref(), "p", &exotic, 600).await.is_err());

    let mut nonsense = batch_req("download", &[(&lfs_oid(1), 1)]);
    nonsense.operation = "delete".into();
    assert!(super::lfs::batch(rig.store.as_ref(), "p", &nonsense, 600).await.is_err());

    // A client that offers nothing is a client that will take `basic`.
    let mut silent = batch_req("download", &[(&lfs_oid(1), 1)]);
    silent.transfers = vec![];
    assert!(super::lfs::batch(rig.store.as_ref(), "p", &silent, 600).await.is_ok());
}

/// A presigned PUT is a grant to write at a key, and nothing about it
/// proves the write happened or finished. `verify` is where the server
/// finds out, and it is the only place it can — the bytes never came
/// through here to be counted.
#[tokio::test]
async fn verify_catches_an_upload_that_did_not_land_or_did_not_finish() {
    let rig = Rig::new().await;
    let oid = lfs_oid(0x33);
    let spec = super::lfs::ObjectSpec { oid: oid.clone(), size: 7 };

    match super::lfs::verify(rig.store.as_ref(), &rig.sc.cfg.prefix, &spec).await {
        Err((404, why)) => assert!(why.contains("did not complete"), "{why}"),
        other => panic!("an absent object must not verify: {other:?}"),
    }

    // Truncated: the PUT landed and stopped early.
    rig.store.raw_put(
        &super::lfs::object_key(&rig.sc.cfg.prefix, &oid),
        bytes::Bytes::from_static(b"abc"),
        vec![],
    );
    match super::lfs::verify(rig.store.as_ref(), &rig.sc.cfg.prefix, &spec).await {
        Err((422, why)) => assert!(why.contains("3 bytes"), "{why}"),
        other => panic!("a short object must not verify: {other:?}"),
    }

    rig.store.raw_put(
        &super::lfs::object_key(&rig.sc.cfg.prefix, &oid),
        bytes::Bytes::from_static(b"weights"),
        vec![],
    );
    super::lfs::verify(rig.store.as_ref(), &rig.sc.cfg.prefix, &spec).await.expect("verifies");
}

/// A store that is having a moment must not be reported as "the object
/// is not there": that would make a client re-upload bytes that are
/// already in the bucket.
#[tokio::test]
async fn an_unreachable_store_is_not_reported_as_a_missing_object() {
    let rig = Rig::new().await;
    rig.store.inject_head_failures(1);
    let res = super::lfs::batch(
        rig.store.as_ref(),
        &rig.sc.cfg.prefix,
        &batch_req("download", &[(&lfs_oid(0x44), 1)]),
        600,
    )
    .await
    .unwrap();
    assert_eq!(res.objects[0].error.as_ref().unwrap().code, 503);
}

/// A batch larger than the protocol expects would mean an unbounded
/// number of HEADs behind one request.
#[tokio::test]
async fn an_oversized_batch_is_refused() {
    let rig = Rig::new().await;
    let oids: Vec<String> = (0..super::lfs::MAX_BATCH + 1).map(|i| lfs_oid((i % 251) as u8)).collect();
    let pairs: Vec<(&str, u64)> = oids.iter().map(|o| (o.as_str(), 1u64)).collect();
    assert!(super::lfs::batch(rig.store.as_ref(), "p", &batch_req("download", &pairs), 600)
        .await
        .is_err());
}


// ── the git↔S3 transfer path (`packio`) ──────────────────────────────
//
// This module moves every byte between the repository and the bucket
// and had no coverage at all. The grid arithmetic is checked against
// S3's REAL constants rather than the memory store's permissive
// defaults: `MemoryStore::min_part` is 1 out of the box, so a grid that
// would earn `EntityTooSmall` from a real bucket passes against an
// unconfigured double. Setting it is what makes these tests able to
// fail.

/// S3's own limits, restated here because `flint_store::s3` is behind a
/// feature this crate does not enable by default.
const S3_MIN_PART: u64 = 5 * 1024 * 1024;
const S3_MAX_PARTS: usize = 10_000;

/// The grid must tile the object exactly — contiguous, from zero, no
/// gap and no overlap — because every part is a byte range of the same
/// local file and a hole would be silent corruption of a pack.
#[test]
fn the_part_grid_tiles_the_object_exactly() {
    let ceiling = super::packio::WHOLE_PUT_MAX;
    for size in [
        ceiling + 1,
        ceiling + S3_MIN_PART,
        2 * ceiling,
        2 * ceiling + 1,
        10 * ceiling,
        640 * 1024 * 1024 * 1024,          // the last size that fits at one part per 64 MiB
        640 * 1024 * 1024 * 1024 + 1,      // the first that forces a coarser grid
        5 * 1024 * 1024 * 1024 * 1024,     // S3's maximum object
    ] {
        let parts = super::packio::part_grid(size, S3_MIN_PART, S3_MAX_PARTS);
        assert!(!parts.is_empty(), "size {size} produced no parts");
        let mut expect = 0u64;
        for p in &parts {
            let (off, len) = match p {
                flint_store::PartSource::Local { offset, len }
                | flint_store::PartSource::BaseCopy { offset, len } => (*offset, *len),
            };
            assert_eq!(off, expect, "size {size}: part starts at {off}, expected {expect}");
            assert!(len > 0, "size {size}: zero-length part");
            expect = off + len;
        }
        assert_eq!(expect, size, "size {size}: grid covers {expect}");
    }
}

/// Every part but the last must clear the backend minimum. This is the
/// rule real S3 enforces with `EntityTooSmall`, and the one an
/// unconfigured memory store cannot catch.
#[test]
fn the_part_grid_never_undersizes_a_part_that_is_not_the_last() {
    let ceiling = super::packio::WHOLE_PUT_MAX;
    for size in [
        ceiling + 1,
        ceiling + 4096,
        3 * ceiling + 1,
        640 * 1024 * 1024 * 1024 + 1,
        5 * 1024 * 1024 * 1024 * 1024,
    ] {
        let parts = super::packio::part_grid(size, S3_MIN_PART, S3_MAX_PARTS);
        for (i, p) in parts.iter().enumerate() {
            let len = match p {
                flint_store::PartSource::Local { len, .. }
                | flint_store::PartSource::BaseCopy { len, .. } => *len,
            };
            if i + 1 != parts.len() {
                assert!(
                    len >= S3_MIN_PART,
                    "size {size}: part {i} is {len}, under the {S3_MIN_PART} minimum"
                );
            }
        }
    }
}

/// The grid must stay inside the backend's part ceiling at every size
/// up to S3's largest object. A grid one part over the limit fails the
/// upload at the LAST part, after the whole object has been sent.
#[test]
fn the_part_grid_stays_within_the_backend_part_limit() {
    for size in [
        640 * 1024 * 1024 * 1024,
        640 * 1024 * 1024 * 1024 + 1,
        1024 * 1024 * 1024 * 1024,
        5 * 1024 * 1024 * 1024 * 1024,
    ] {
        let parts = super::packio::part_grid(size, S3_MIN_PART, S3_MAX_PARTS);
        assert!(
            parts.len() <= S3_MAX_PARTS,
            "size {size}: {} parts exceeds {S3_MAX_PARTS}",
            parts.len()
        );
    }
}

/// A store configured the way a real bucket behaves: parts below the
/// minimum are refused. `MemoryStore::new()` ships `min_part = 1`,
/// which accepts grids S3 would reject.
fn s3_shaped_store() -> Arc<MemoryStore> {
    let mut ms = MemoryStore::new();
    ms.min_part = S3_MIN_PART;
    ms.max_parts = S3_MAX_PARTS;
    Arc::new(ms)
}

/// The fetch path spawns tasks and so takes the store as an `Arc`.
fn dynstore(s: &Arc<MemoryStore>) -> Arc<dyn ObjectStore> {
    s.clone()
}

fn write_pattern(path: &std::path::Path, len: u64) {
    use std::io::Write;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).expect("create"));
    // Deterministic, and varied enough that a misordered part shows up
    // as a CRC mismatch rather than as identical bytes in the wrong place.
    let mut block = vec![0u8; 1 << 20];
    let mut written = 0u64;
    let mut seed: u32 = 0x9e37_79b9;
    while written < len {
        for b in block.iter_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *b = (seed >> 24) as u8;
        }
        let n = ((len - written) as usize).min(block.len());
        f.write_all(&block[..n]).expect("write");
        written += n as u64;
    }
    f.flush().expect("flush");
}

/// Under the ceiling the transfer is ONE request. This pins the cheap
/// path: a pack that fits must not pay for a multipart handshake.
#[tokio::test]
async fn a_pack_under_the_ceiling_goes_up_as_one_request() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pack-small.pack");
    write_pattern(&path, 3 << 20);
    let store = s3_shaped_store();
    store.reset_op_counts();
    super::packio::upload_file(store.as_ref(), "p/git/objects/pack/pack-small.pack", &path, 7, None)
        .await
        .expect("upload");
    let ops = store.op_counts();
    assert_eq!(ops.get("put_whole").copied().unwrap_or(0), 1, "one PUT, got {ops:?}");
    assert_eq!(
        ops.get("compose_generation").copied().unwrap_or(0),
        0,
        "a small pack must not open a multipart upload: {ops:?}"
    );
}

/// Above the ceiling the transfer is composed, and what comes back is
/// what went up. This is the path no test and no e2e leg has ever
/// exercised — the largest payload in the whole suite was 12 MiB
/// against a 64 MiB ceiling — and it is the ordinary case for a
/// repacked repository.
///
/// It also decides the checksum question the two upload paths raise:
/// under the ceiling the CRC is taken from the body already in RAM,
/// above it the CRC streams the file in 4 MiB blocks. The store
/// validates the composed object's CRC against its assembled bytes, so
/// a streaming checksum that disagreed with a whole-buffer one would
/// fail here rather than at a real bucket.
#[tokio::test]
async fn a_pack_over_the_ceiling_is_composed_and_round_trips_byte_identical() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("pack-big.pack");
    let size = super::packio::WHOLE_PUT_MAX + (1 << 20);
    write_pattern(&src, size);
    let store = s3_shaped_store();
    store.reset_op_counts();
    let key = "p/git/objects/pack/pack-big.pack";
    super::packio::upload_file(store.as_ref(), key, &src, 7, None).await.expect("upload");
    let ops = store.op_counts();
    assert_eq!(
        ops.get("compose_generation").copied().unwrap_or(0),
        1,
        "a pack over the ceiling must be composed: {ops:?}"
    );

    let back = dir.path().join("fetched").join("pack-big.pack");
    super::packio::fetch_to_file(dynstore(&store), key, &back, 4).await.expect("fetch");
    let a = std::fs::read(&src).expect("src");
    let b = std::fs::read(&back).expect("back");
    assert_eq!(a.len(), b.len(), "size changed across the transfer");
    assert!(a == b, "the composed object did not round trip byte-identical");
}

/// Re-uploading a pack must reach the store every time. The sweep reads
/// object age to decide what to collect, so an upload skipped as
/// "already there" would let a pack the repository still needs age out
/// (`packio`'s own doc rule, and `LeanChunkGC` rule 4).
#[tokio::test]
async fn a_re_uploaded_pack_is_never_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pack-x.pack");
    write_pattern(&path, 1 << 20);
    let store = s3_shaped_store();
    let key = "p/git/objects/pack/pack-x.pack";
    super::packio::upload_file(store.as_ref(), key, &path, 7, None).await.expect("first");
    store.reset_op_counts();
    super::packio::upload_file(store.as_ref(), key, &path, 7, None).await.expect("second");
    assert_eq!(
        store.op_counts().get("put_whole").copied().unwrap_or(0),
        1,
        "the second upload must still reach the store"
    );
}

/// A fetch that fails must leave nothing at the pack's path. Git reads
/// a truncated `.idx` as corruption of the REPOSITORY rather than of
/// the transfer, so a partial file at the real name is worse than no
/// file at all.
#[tokio::test]
async fn a_failed_fetch_never_lands_a_partial_pack() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("sub").join("pack-missing.pack");
    let store = s3_shaped_store();
    let err =
        super::packio::fetch_to_file(dynstore(&store), "p/git/objects/pack/nope.pack", &dest, 4)
            .await;
    assert!(err.is_err(), "a missing key must not report success");
    assert!(!dest.exists(), "a failed fetch left a file at the pack path");
    assert!(
        !super::packio::part_of(&dest).exists(),
        "a failed fetch left its temporary behind"
    );
}



// ── the per-push S3 protocol, pinned ─────────────────────────────────
//
// §4 costs a batch at "one renew, two to four per new pack, one CAS,
// two derived". Nothing enforced that, and the shipped code spent a
// fifth request per push restating `HEAD` — an object §3 calls
// "derived, once". These tests hold the protocol to its documented
// shape: a regression that adds a round trip to every push shows up
// here rather than on a bucket's request bill.

/// The fixed cost of a batch, isolated from git's pack behaviour by
/// pushing a ref that introduces no new objects: one lease renewal,
/// one snapshot CAS, and the two derived files a dumb clone reads.
#[tokio::test]
async fn the_fixed_per_push_s3_cost_is_four_requests() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c1 = rig.stage_commit(None, &[("a.txt", "one")], "one").await;
    rig.run(vec![push(
        1,
        vec![RefUpdate { name: "refs/heads/main".into(), old_oid: zero(), new_oid: c1.clone() }],
    )])
    .await;

    // A second ref at the SAME commit: the pack is already in the
    // snapshot, so every request left is fixed overhead.
    rig.store.reset_op_counts();
    rig.run(vec![push(
        2,
        vec![RefUpdate { name: "refs/heads/side".into(), old_oid: zero(), new_oid: c1.clone() }],
    )])
    .await;
    let ops = rig.store.op_counts();
    assert_eq!(ops.get("epoch_renew").copied().unwrap_or(0), 1, "one renew per batch: {ops:?}");
    assert_eq!(
        rig.store.total_ops(),
        4,
        "the fixed per-push cost is renew + CAS + objects/info/packs + info/refs: {ops:?}"
    );
}

/// `HEAD` names the default branch. It is published once and then only
/// when it changes — not once per push, which is what the shipped code
/// did and what §3 says it must not.
#[tokio::test]
async fn head_is_published_once_not_once_per_push() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let head_key = rig.sc.cfg.head_key();

    let c1 = rig.stage_commit(None, &[("a.txt", "one")], "one").await;
    rig.run(vec![push(
        1,
        vec![RefUpdate { name: "refs/heads/main".into(), old_oid: zero(), new_oid: c1.clone() }],
    )])
    .await;
    assert!(
        rig.store.get_whole(&head_key, None).await.is_ok(),
        "the first batch must publish HEAD"
    );

    let c2 = rig.stage_commit(Some(&c1), &[("a.txt", "two")], "two").await;
    rig.store.reset_op_counts();
    rig.run(vec![push(
        2,
        vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: c1.clone(),
            new_oid: c2.clone(),
        }],
    )])
    .await;
    // Four fixed (minus HEAD) plus this push's new pack siblings.
    let ops = rig.store.op_counts();
    assert!(
        rig.store.total_ops() < 8,
        "a later batch must not restate HEAD: {ops:?}"
    );
}

/// A push that introduces a pack pays for that pack's siblings and
/// nothing else. This is the shape §4 documents; it pins the "two to
/// four per new pack" term against a batch that adds one pack.
#[tokio::test]
async fn a_push_with_a_new_pack_pays_only_for_its_siblings() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c1 = rig.stage_commit(None, &[("a.txt", "one")], "one").await;
    rig.run(vec![push(
        1,
        vec![RefUpdate { name: "refs/heads/main".into(), old_oid: zero(), new_oid: c1.clone() }],
    )])
    .await;

    let c2 = rig.stage_commit(Some(&c1), &[("a.txt", "two")], "two").await;
    rig.store.reset_op_counts();
    rig.run(vec![push(
        2,
        vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: c1.clone(),
            new_oid: c2.clone(),
        }],
    )])
    .await;
    let total = rig.store.total_ops();
    let ops = rig.store.op_counts();
    // 3 fixed (renew, CAS, and the two derived less HEAD is 4 — one of
    // which is the CAS) plus 2..=4 siblings.
    assert!(
        (5..=7).contains(&total),
        "a one-pack push should cost the fixed 4 plus 2-4 siblings, got {total}: {ops:?}"
    );
}

// ── the restore transfer is ranged, pinned, and retried per chunk ────

/// The memory bound, pinned as a request shape. A whole-object read is
/// one request and holds the object twice — measured at a flat 2.05x of
/// object size from 256 MiB to 2 GiB, which at section 5's 10 GB
/// envelope is ~20.5 GB to restore, at every pod start. A ranged fetch
/// is one request per chunk and holds one chunk, and the count is what
/// a test can see.
#[tokio::test]
async fn a_pack_is_fetched_in_ranges_not_as_one_object() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("pack-r.pack");
    let size = 20u64 << 20;
    write_pattern(&src, size);
    let store = s3_shaped_store();
    let key = "p/git/objects/pack/pack-r.pack";
    super::packio::upload_file(store.as_ref(), key, &src, 7, None).await.expect("upload");

    let dest = dir.path().join("out").join("pack-r.pack");
    store.reset_op_counts();
    super::packio::fetch_to_file(dynstore(&store), key, &dest, 4).await.expect("fetch");
    let ops = store.op_counts();

    let want = size.div_ceil(super::packio::FETCH_CHUNK);
    assert_eq!(
        ops.get("get_range").copied().unwrap_or(0),
        want,
        "a {size}-byte object should take {want} ranged reads: {ops:?}"
    );
    assert_eq!(
        ops.get("get_whole").copied().unwrap_or(0),
        0,
        "the restore must never read a pack whole: {ops:?}"
    );
    let a = std::fs::read(&src).expect("src");
    let b = std::fs::read(&dest).expect("dest");
    assert!(a == b, "the ranged fetch did not reproduce the pack");
}

/// A pack whose etag moved under the restore is REFUSED, not adopted.
/// This is the deliberate divergence from `tier::hydrate`, which adopts
/// on a 412 because a tier's object legitimately moves. A pack is
/// immutable and content-named: a moved etag means something wrote a
/// pack file that is not the pack it is named for.
#[tokio::test]
async fn a_pack_that_moved_under_the_restore_is_refused_not_adopted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("pack-m.pack");
    write_pattern(&src, 12 << 20);
    let store = s3_shaped_store();
    let key = "p/git/objects/pack/pack-m.pack";
    super::packio::upload_file(store.as_ref(), key, &src, 7, None).await.expect("upload");

    let dest = dir.path().join("out").join("pack-m.pack");
    let err = super::packio::fetch_pinned(
        dynstore(&store),
        key,
        &dest,
        12 << 20,
        "\"an-etag-this-object-never-had\"",
        4,
    )
    .await
    .expect_err("a moved etag must not be adopted");
    assert!(
        matches!(err, ForgeError::Refused(_)),
        "expected a refusal, got {err:?}"
    );
    assert!(!dest.exists(), "a refused fetch left a pack behind");
    assert!(!super::packio::part_of(&dest).exists(), "a refused fetch left its temporary");
}

/// A transport failure retries the CHUNK. The budget is per chunk, so a
/// cut connection partway through a multi-GiB pack does not discard the
/// chunks already written — the whole reason the fetch is chunked at
/// all rather than merely bounded.
#[tokio::test]
async fn a_cut_connection_retries_the_chunk_and_keeps_earlier_progress() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("pack-t.pack");
    let size = 20u64 << 20;
    write_pattern(&src, size);
    let store = s3_shaped_store();
    let key = "p/git/objects/pack/pack-t.pack";
    super::packio::upload_file(store.as_ref(), key, &src, 7, None).await.expect("upload");

    let dest = dir.path().join("out").join("pack-t.pack");
    store.reset_op_counts();
    store.inject_get_range_failures(2);
    super::packio::fetch_to_file(dynstore(&store), key, &dest, 4).await.expect("fetch");

    let chunks = size.div_ceil(super::packio::FETCH_CHUNK);
    let ops = store.op_counts();
    assert_eq!(
        ops.get("get_range").copied().unwrap_or(0),
        chunks + 2,
        "two failures should cost two extra RANGES, not a restarted file: {ops:?}"
    );
    let a = std::fs::read(&src).expect("src");
    let b = std::fs::read(&dest).expect("dest");
    assert!(a == b, "the retried fetch did not reproduce the pack");
}

/// Past the budget it fails, and leaves nothing a later pass could
/// mistake for a complete pack.
#[tokio::test]
async fn a_fetch_past_its_retry_budget_leaves_no_pack_behind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("pack-b.pack");
    write_pattern(&src, 12 << 20);
    let store = s3_shaped_store();
    let key = "p/git/objects/pack/pack-b.pack";
    super::packio::upload_file(store.as_ref(), key, &src, 7, None).await.expect("upload");

    let dest = dir.path().join("out").join("pack-b.pack");
    store.inject_get_range_failures(64);
    let err = super::packio::fetch_to_file(dynstore(&store), key, &dest, 4).await;
    assert!(err.is_err(), "an exhausted budget must not report success");
    assert!(!dest.exists(), "a failed fetch left a pack at the real name");
    assert!(!super::packio::part_of(&dest).exists(), "a failed fetch left its temporary");
}

// ── the restore's fan-out, bounded from both sides ───────────────────
//
// `fanout` was declared in the config and read nowhere: uploads ran at
// a hard-coded bound and the restore fetched one file at a time, one
// chunk at a time. These hold the restore to the bound it now has —
// exactly `fanout` ranged GETs in flight when there is work for them,
// and never more.

/// Two siblings of one pack, two chunks each, fetched under a per-GET
/// delay long enough that overlapping calls are in flight together.
/// The peak is read from the STORE, so it counts what the store saw,
/// not what a scheduler happened to interleave. The control is fanout
/// 1: the same fetch against the same store, and a peak of exactly one.
///
/// The two units share a stem on purpose. Under the old temporary name
/// (`with_extension("part")`) they shared one `.part` as well, and this
/// test fails there on the second rename.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_restore_fan_out_is_exactly_the_configured_bound() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = s3_shaped_store();
    let two_chunks = 2 * super::packio::FETCH_CHUNK;
    let mut srcs: Vec<(&str, String, std::path::PathBuf, String)> = Vec::new();
    for name in ["pack-f.pack", "pack-f.idx"] {
        let src = dir.path().join("src").join(name);
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        write_pattern(&src, two_chunks);
        let key = format!("p/git/objects/pack/{name}");
        super::packio::upload_file(store.as_ref(), &key, &src, 7, None).await.expect("upload");
        let etag = store.head(&key).await.expect("head").etag;
        srcs.push((name, key, src, etag));
    }
    store.inject_get_range_delay_ms(40);

    for fanout in [1usize, 2, 4] {
        let out = dir.path().join(format!("out-{fanout}"));
        let units: Vec<super::packio::FetchUnit> = srcs
            .iter()
            .map(|(name, key, _, etag)| super::packio::FetchUnit {
                key: key.clone(),
                dest: out.join(name),
                size: two_chunks,
                etag: etag.clone(),
            })
            .collect();
        store.reset_peak_get_range_in_flight();
        super::packio::fetch_all(dynstore(&store), units, fanout, None).await.expect("fetch");
        assert_eq!(
            store.peak_get_range_in_flight(),
            fanout as u64,
            "four chunks to fetch: the peak in flight must be exactly the bound {fanout}"
        );
        for (name, _, src, _) in &srcs {
            let a = std::fs::read(src).unwrap();
            let b = std::fs::read(out.join(name)).unwrap();
            assert!(a == b, "{name} did not round trip at fanout {fanout}");
            assert!(
                !super::packio::part_of(&out.join(name)).exists(),
                "{name} left its temporary behind at fanout {fanout}"
            );
        }
    }
}

/// A failure in ONE chunk of ONE sibling lands none of the set: no
/// `.part` of either and no file at either real name. The set is what
/// the snapshot names, and a restore that left the `.pack` complete
/// beside a missing `.idx` would hand git a pack it cannot open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_chunk_lands_none_of_the_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = s3_shaped_store();
    let size = 12u64 << 20;
    let mut units = Vec::new();
    for name in ["pack-g.pack", "pack-g.idx"] {
        let src = dir.path().join("src").join(name);
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        write_pattern(&src, size);
        let key = format!("p/git/objects/pack/{name}");
        super::packio::upload_file(store.as_ref(), &key, &src, 7, None).await.expect("upload");
        let etag = store.head(&key).await.expect("head").etag;
        units.push(super::packio::FetchUnit {
            key,
            dest: dir.path().join("out").join(name),
            size,
            etag,
        });
    }
    let dests: Vec<std::path::PathBuf> = units.iter().map(|u| u.dest.clone()).collect();
    store.inject_get_range_failures(64);
    let err = super::packio::fetch_all(dynstore(&store), units, 4, None).await;
    assert!(err.is_err(), "an exhausted budget must not report success");
    for d in &dests {
        assert!(!d.exists(), "{} landed although a sibling failed", d.display());
        assert!(!super::packio::part_of(d).exists(), "{} left its temporary", d.display());
    }
}

// ── the lease, off the loop ──────────────────────────────────────────
//
// The heartbeat was a timer arm of the serving loop's select!, so it
// could not fire while the loop was inside a batch, a restore or an
// export. At 10 GiB the token was measured silent for 125 s during a
// push and 141 s during a restore against a 60 s takeover window. The
// renewer is now its own task — and it is gated on progress, so the
// fix does not trade "a live pod loses its repository" for "a wedged
// one keeps it".

fn shared_for(rig: &Rig, phase: Phase) -> status::Shared {
    Arc::new(std::sync::Mutex::new(status::facts(&rig.sc, phase)))
}

fn renews(rig: &Rig) -> u64 {
    rig.store.op_counts().get("epoch_renew").copied().unwrap_or(0)
}

/// Serving idle: one renewal per heartbeat. A push that moves: the
/// same. A push that has stopped moving: the token goes quiet. Virtual
/// time, so the counts are exact rather than approximate.
#[tokio::test(start_paused = true)]
async fn the_renewer_renews_a_moving_push_and_lets_a_stalled_one_go_quiet() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let shared = shared_for(&rig, Phase::Serving);
    let task = lease::spawn_renewer(
        rig.store.clone() as Arc<dyn ObjectStore>,
        rig.sc.cfg.epoch_key(),
        rig.sc.hold.clone(),
        shared.clone(),
        std::time::Duration::from_millis(100),
    );
    let tick = std::time::Duration::from_millis(100);

    // Idle serving renews on every heartbeat, moving or not.
    rig.store.reset_op_counts();
    tokio::time::sleep(tick * 6 + std::time::Duration::from_millis(50)).await;
    let idle = renews(&rig);
    assert!((5..=7).contains(&idle), "serving idle: one renew per heartbeat, got {idle}");

    // A push that moves: progress advances between heartbeats.
    shared.lock().unwrap().phase = Phase::Pushing;
    rig.store.reset_op_counts();
    for _ in 0..20 {
        rig.sc.hold.tick(1);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    let moving = renews(&rig);
    assert!(moving >= 4, "a moving push must keep renewing, got {moving}");

    // The same push, wedged: nothing advances. At most the one renewal
    // that credits the progress made before the stall.
    rig.store.reset_op_counts();
    tokio::time::sleep(tick * 6).await;
    let stalled = renews(&rig);
    assert!(stalled <= 1, "a stalled push must let the token go quiet, got {stalled} renewals");

    // Back to serving: renews again without any progress at all.
    shared.lock().unwrap().phase = Phase::Serving;
    rig.store.reset_op_counts();
    tokio::time::sleep(tick * 6).await;
    let again = renews(&rig);
    assert!(again >= 4, "serving idle must renew again, got {again}");
    task.abort();
}

/// A restore is judged the same way as a push: it renews while chunks
/// land and goes quiet when they stop.
#[tokio::test(start_paused = true)]
async fn the_renewer_judges_a_restore_by_its_chunks() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let shared = shared_for(&rig, Phase::Importing);
    let task = lease::spawn_renewer(
        rig.store.clone() as Arc<dyn ObjectStore>,
        rig.sc.cfg.epoch_key(),
        rig.sc.hold.clone(),
        shared,
        std::time::Duration::from_millis(100),
    );
    rig.store.reset_op_counts();
    tokio::time::sleep(std::time::Duration::from_millis(650)).await;
    assert!(renews(&rig) <= 1, "an importing server that lands nothing must go quiet");
    let progress = rig.sc.hold.progress_handle();
    rig.store.reset_op_counts();
    for _ in 0..12 {
        progress.fetch_add(8 << 20, std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(renews(&rig) >= 4, "a restore landing chunks must keep its lease");
    task.abort();
}

/// Deposed while the loop is busy: the renewer fences the hold and the
/// loop's watch wakes. Nothing after that touches the store.
#[tokio::test(start_paused = true)]
async fn a_renewer_that_is_deposed_fences_the_syncer_and_wakes_the_loop() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let shared = shared_for(&rig, Phase::Serving);
    let mut fenced_rx = rig.sc.hold.subscribe();
    let task = lease::spawn_renewer(
        rig.store.clone() as Arc<dyn ObjectStore>,
        rig.sc.cfg.epoch_key(),
        rig.sc.hold.clone(),
        shared,
        std::time::Duration::from_millis(100),
    );
    // A successor takes the cell.
    let key = rig.sc.cfg.epoch_key();
    let state = rig.store.epoch_read(&key).await.unwrap().expect("cell");
    rig.store.epoch_acquire(&key, "successor", Some(&state)).await.expect("supersede");

    tokio::time::timeout(std::time::Duration::from_secs(5), fenced_rx.changed())
        .await
        .expect("the loop must be woken by the fence")
        .expect("the hold outlives the loop");
    let why = rig.sc.fenced().expect("fenced");
    assert!(why.contains("deposed at renew"), "{why}");
    assert!(matches!(rig.sc.check_fence(), Err(ForgeError::Fenced(_))));
    assert!(rig.sc.lease().is_err(), "a fenced hold has no lease");

    rig.store.reset_op_counts();
    let c1 = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    let err = batch::run_batch(
        &mut rig.sc,
        vec![push(1, vec![RefUpdate { name: "refs/heads/main".into(), old_oid: zero(), new_oid: c1 }])],
        &Policy::default(),
    )
    .await
    .expect_err("a fenced syncer must refuse the batch");
    assert!(matches!(err, ForgeError::Fenced(_)), "{err:?}");
    assert_eq!(rig.store.total_ops(), 0, "a fenced batch must not touch the store");
    task.abort();
}

/// A clean release stops the renewer: no renewal lands on a released
/// cell, which would un-release it under a successor's claim.
#[tokio::test(start_paused = true)]
async fn a_release_stops_the_renewer() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let shared = shared_for(&rig, Phase::Serving);
    let task = lease::spawn_renewer(
        rig.store.clone() as Arc<dyn ObjectStore>,
        rig.sc.cfg.epoch_key(),
        rig.sc.hold.clone(),
        shared,
        std::time::Duration::from_millis(100),
    );
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    lease::release(&mut rig.sc).await.expect("release");
    rig.store.reset_op_counts();
    tokio::time::sleep(std::time::Duration::from_millis(650)).await;
    assert_eq!(renews(&rig), 0, "no renewal may follow a release");
    let state = rig.store.epoch_read(&rig.sc.cfg.epoch_key()).await.unwrap().expect("cell");
    assert!(state.released, "the cell must stay released");
    assert!(task.is_finished(), "the renewer must have exited on its own");
}

// ── orphaned uploads ─────────────────────────────────────────────────
//
// The scale drill's S4: one kill inside a 2 GiB push left 384 MiB of
// parts in an upload nothing would ever complete or abort, billed
// until a hand abort. Forge had no sweep; lean and the tier both do.

/// What a crashed predecessor left in flight is aborted by the next
/// start, before the restore — the moment nothing of ours can be in
/// flight.
#[tokio::test]
async fn a_start_aborts_the_uploads_a_predecessor_left_in_flight() {
    let mut rig = Rig::new().await;
    let prefix = format!("{}/", rig.sc.cfg.git_prefix());
    rig.store.raw_begin_upload(&rig.sc.cfg.pack_key("pack-crashed.pack"));
    rig.store.raw_begin_upload(&rig.sc.cfg.bundle_key("clone-crashed.bundle"));
    // Not ours: a neighbouring prefix's upload is left alone.
    rig.store.raw_begin_upload("elsewhere/git/objects/pack/pack-theirs.pack");
    assert_eq!(rig.store.list_uploads(&prefix).await.unwrap().len(), 2);
    rig.start().await;
    assert!(
        rig.store.list_uploads(&prefix).await.unwrap().is_empty(),
        "the start must abort every upload pending under the repository"
    );
    assert_eq!(
        rig.store.list_uploads("elsewhere/").await.unwrap().len(),
        1,
        "another prefix's upload is not this server's to abort"
    );
}

/// Between batches the sweep does the same, so an orphan that outlives
/// a start (a listing that failed then) is still collected.
#[tokio::test]
async fn the_sweep_between_batches_aborts_a_pending_upload() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c1 = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    rig.run(vec![push(1, vec![RefUpdate { name: "refs/heads/main".into(), old_oid: zero(), new_oid: c1 }])])
        .await;
    let prefix = format!("{}/", rig.sc.cfg.git_prefix());
    rig.store.raw_begin_upload(&rig.sc.cfg.pack_key("pack-stale.pack"));
    assert_eq!(rig.store.list_uploads(&prefix).await.unwrap().len(), 1);
    sweep::sweep(&mut rig.sc).await.expect("sweep");
    assert!(rig.store.list_uploads(&prefix).await.unwrap().is_empty());
}

/// Transfers report what they landed, in bytes, on the counter the
/// renewer reads — a whole PUT once, a ranged fetch per chunk.
#[tokio::test]
async fn transfers_report_their_progress_in_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("pack-p.pack");
    let size = 12u64 << 20;
    write_pattern(&src, size);
    let store = s3_shaped_store();
    let key = "p/git/objects/pack/pack-p.pack";
    let progress = Arc::new(std::sync::atomic::AtomicU64::new(0));
    super::packio::upload_file(store.as_ref(), key, &src, 7, Some(progress.clone()))
        .await
        .expect("upload");
    assert_eq!(progress.load(std::sync::atomic::Ordering::Relaxed), size, "the upload's bytes");
    let etag = store.head(key).await.expect("head").etag;
    let unit = super::packio::FetchUnit {
        key: key.into(),
        dest: dir.path().join("out").join("pack-p.pack"),
        size,
        etag,
    };
    super::packio::fetch_all(dynstore(&store), vec![unit], 4, Some(progress.clone()))
        .await
        .expect("fetch");
    assert_eq!(
        progress.load(std::sync::atomic::Ordering::Relaxed),
        2 * size,
        "the fetch's bytes on top"
    );
}
