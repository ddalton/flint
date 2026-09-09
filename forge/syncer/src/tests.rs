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
use super::{fold, lease, restore, snapshot, status, sweep, undo, ForgeConfig, ForgeError, Syncer};

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

    /// One commit on `branch`, pushed as its own batch. Returns the tip.
    async fn push_commit(&mut self, branch: &str, parent: Option<&str>, tag: &str) -> String {
        let c = self
            .stage_commit(parent, &[("f.txt", &format!("{tag}\n"))], tag)
            .await;
        let old = parent.map(|p| p.to_string()).unwrap_or_else(zero);
        let reports = self
            .run(vec![push(1, vec![RefUpdate { name: branch.into(), old_oid: old, new_oid: c.clone() }])])
            .await;
        assert!(is_ok(&reports[0].results[0]), "push {tag}: {:?}", reports[0].results[0]);
        c
    }

    /// Plan one fold, run its task to completion and commit it — what
    /// the serving loop does across its post-batch hook, its task and
    /// its fold arm. `None` when nothing was planned.
    async fn fold_once(&mut self) -> Option<(fold::Plan, Option<String>)> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let now = super::now_unix();
        let plan = fold::maybe_spawn(&mut self.sc, tx, now).expect("plan")?;
        let res = rx.recv().await.expect("the fold task reports");
        assert!(res.error.is_none(), "the fold task failed: {:?}", res.error);
        let named = fold::commit(&mut self.sc, res, now).await.expect("commit");
        Some((plan, named))
    }

    /// Tier folds at every opportunity and never a base: the floor off
    /// (the rig's packs are bytes, not megabytes) and the base rule
    /// out of reach.
    fn tiers_only(&mut self) {
        self.sc.cfg.fold_factor = 2;
        self.sc.cfg.base_min_bytes = u64::MAX;
        self.sc.cfg.fold_min_bytes = 0;
    }
}

/// A push as a hook would hand it over. A free function, not a method
/// on the rig, so a test can build one while the rig is borrowed for
/// the batch it is about to run.
fn push(id: u64, cmds: Vec<RefUpdate>) -> PushRequest {
    PushRequest { id, principal: "tester".into(), options: vec![], atomic: false, packs: vec![], commands: cmds, server_created: vec![] }
}

/// `git push --atomic`: every command lands or none does.
fn atomic_push(id: u64, cmds: Vec<RefUpdate>) -> PushRequest {
    PushRequest { id, principal: "tester".into(), options: vec![], atomic: true, packs: vec![], commands: cmds, server_created: vec![] }
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
    cold.sc.git.fsck_connectivity_all().await.expect("a restored repository must be whole");
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
    cold.sc.git.fsck_connectivity_all().await.expect("the merge must be in the bucket");
}

/// TWO MERGES IN ONE BATCH, and the second's base is the first's TIP.
///
/// Found on runcj by F14 (2026-09-08): forge published a ref whose
/// PARENT had reached no pack in the bucket, and every restart then
/// refused with exit 78 — `fsck --connectivity-only` reporting
/// `broken link ... missing commit`. The repository was unrecoverable
/// from S3 while being perfectly intact on the pod's disk.
///
/// THE MECHANISM. `judge_merge` takes its base from the EFFECTIVE ref
/// map, which earlier commands in the same batch have already moved. So
/// a second merge's base is the first merge's tip — a commit that
/// exists at that moment only as a LOOSE object this batch is supposed
/// to pack. The packing step excluded every base, so `pack-objects` was
/// handed `M1 ^M1` and dropped M1 from its own pack. The snapshot then
/// named M2, whose parent M1 was nowhere.
///
/// The single-merge test above cannot see this: with one merge the only
/// base is already durable in the bucket, which is exactly the case the
/// exclusion was written for.
#[tokio::test]
async fn two_merges_in_one_batch_both_reach_the_bucket() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let base = rig.stage_commit(None, &[("a.txt", "base\n")], "base").await;
    rig.run(vec![push(1, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: zero(),
        new_oid: base.clone(),
    }])])
    .await;
    // main moves, so neither proposal below can be a fast-forward.
    let main2 = rig.stage_commit(Some(&base), &[("a.txt", "base\n"), ("m.txt", "m\n")], "on main").await;
    rig.run(vec![push(2, vec![RefUpdate {
        name: "refs/heads/main".into(),
        old_oid: base.clone(),
        new_oid: main2.clone(),
    }])])
    .await;
    // Two agents, each branched from the ORIGINAL base and each
    // touching a file of its own, so both merges are clean.
    let side_a = rig.stage_commit(Some(&base), &[("a.txt", "base\n"), ("c.txt", "c\n")], "side a").await;
    let side_b = rig.stage_commit(Some(&base), &[("a.txt", "base\n"), ("d.txt", "d\n")], "side b").await;

    // ONE batch. This is the whole point: the second merge's base is
    // the first merge's tip.
    //
    // Before the fix this batch did two things wrong, in this order:
    // the pack excluded the first merge (a base that was also a tip),
    // and then `update_refs` refused two updates to one ref — at STEP
    // 6, after the pack, the upload and the snapshot CAS. So a batch
    // git rejects was published first and errored afterwards, which is
    // why runcj's snapshot advanced past a batch whose log entry (seq
    // 52) never appeared.
    let outcome = batch::run_batch(
        &mut rig.sc,
        vec![
            push(3, vec![RefUpdate {
                name: "refs/for/main".into(),
                old_oid: zero(),
                new_oid: side_a.clone(),
            }]),
            push(4, vec![RefUpdate {
                name: "refs/for/main".into(),
                old_oid: zero(),
                new_oid: side_b.clone(),
            }]),
        ],
        &Policy::default(),
    )
    .await;
    let reports = outcome.expect("two proposals for one ref must not error the batch");
    for (i, r) in reports.iter().enumerate() {
        assert!(
            matches!(r.results[0], CommandResult::Ok { .. }),
            "proposal {i} was not accepted: {:?} — both agents' work must land",
            r.results[0]
        );
    }
    let tip = rig
        .sc
        .cell()
        .unwrap()
        .snap
        .refs
        .get("refs/heads/main")
        .cloned()
        .expect("the batch published a tip for main");
    // The local repository and the bucket must AGREE, or the next batch
    // refuses every push to this ref as `disagreed`.
    assert_eq!(
        rig.sc.git.ref_oid("refs/heads/main").await.unwrap(),
        Some(tip.clone()),
        "the ref transaction and the snapshot disagree"
    );

    // THE PREMISE: the published tip is a merge built on ANOTHER merge
    // this same batch created. Without that the test proves nothing —
    // one merge's base is the snapshot's own ref and is durable.
    let parents = rig.git(&["rev-list", "--parents", "-n", "1", &tip], None).await;
    let first_parent = parents.split_whitespace().nth(1).unwrap_or("").to_string();
    assert_ne!(first_parent, main2, "the published tip must be built on the FIRST merge, not on main");
    let subject = rig.git(&["log", "-1", "--format=%s", &first_parent], None).await;
    assert!(subject.contains("Merge"), "the parent must itself be a server-built merge: {subject}");

    // THE ORACLE, and it is the one the real syncer runs on every
    // start: restore from the bucket ALONE and walk the graph. A
    // published tip whose parent reached no pack fails here exactly as
    // it did on runcj, with `broken link ... missing commit`.
    let mut cold = Rig::with_store(rig.store.clone(), "cold").await;
    restore::restore(&mut cold.sc).await.expect("cold restore");
    cold.sc
        .git
        .fsck_connectivity_all()
        .await
        .expect("every server-built commit the snapshot depends on must be in the bucket");
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

/// Compaction to a single pack, end to end: the consolidated pack is
/// published, the sweep collects what it superseded, and a cold restore
/// off the bucket alone is whole.
///
/// This was the control rule's test (`restore::maybe_repack`, the full
/// `repack -a -d -b`). That path was the tiers' control arm and went
/// with the measurement it existed for (design §10, phase 4); the base
/// rebuild is the same operation — one pack holding everything
/// reachable, with a bitmap — carried out with the coverage check, the
/// lease renewal, the retention window and the ledger that the control
/// rule had none of.
#[tokio::test]
async fn compaction_to_one_pack_publishes_it_and_the_sweep_takes_the_old_ones() {
    let mut rig = Rig::new().await;
    rig.sc.cfg.fold_factor = 2;
    rig.sc.cfg.base_min_bytes = 0;
    rig.sc.cfg.base_rebuild_min_secs = 0;
    rig.sc.cfg.fold_min_bytes = 0;
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

    let (plan, named) = rig.fold_once().await.expect("no base yet: the base rule fires");
    assert!(matches!(plan, fold::Plan::Base { .. }), "{plan:?}");
    named.expect("a base was named");
    assert_eq!(rig.sc.cell().unwrap().snap.packs.len(), 1, "one pack after the rebuild");
    // Retention is why this needs its own step and the repack did not:
    // the superseded inputs stay on disk for readers, so the sweep is
    // what takes them out of the BUCKET.
    let deleted = sweep::sweep(&mut rig.sc).await.expect("sweep");
    assert!(deleted > 0, "the superseded packs are collected");

    let mut cold = Rig::with_store(rig.store.clone(), "cold").await;
    restore::restore(&mut cold.sc).await.expect("cold restore after compaction");
    assert_eq!(cold.sc.git.ref_oid("refs/heads/main").await.unwrap(), parent);
    cold.sc.git.fsck_connectivity_all().await.expect("the compacted repository must be whole");
}

/// `fold_factor == 0` means no compaction at all, and that is now the
/// whole of what the setting does — the full repack it used to select
/// went with the tiers' measurement (design §10, phase 4). Four e2e
/// rigs run in this mode, so it is a deployment shape and not a dead
/// branch: nothing is planned, the packs accumulate, and the bucket
/// still restores.
#[tokio::test]
async fn factor_zero_compacts_nothing_and_the_bucket_still_restores() {
    let mut rig = Rig::new().await;
    // Set up a repository the planner WOULD compact, so that "nothing
    // is planned" below is about the factor and not about the rig.
    //
    // It must be a BASE rebuild, and that took three attempts to get
    // right. A tier fold is the wrong control: the geometric split
    // refuses at factor 0 on its own (a zero factor makes every
    // progression hold), so with a tier-fold control this leg passes
    // even with the factor guards mutated out — it would have been a
    // test that could not fail. The base rule is the one path that
    // never consults the factor, so the guard is load-bearing there and
    // only there. Two guards enforce it — `planned`'s and `plan`'s —
    // and it takes removing BOTH to move this leg; either alone is
    // covered by the other.
    rig.sc.cfg.fold_factor = 2;
    rig.sc.cfg.base_min_bytes = 0;
    rig.sc.cfg.base_rebuild_min_secs = 0;
    rig.sc.cfg.fold_min_bytes = 0;
    rig.start().await;
    let mut parent: Option<String> = None;
    for i in 0..3 {
        parent = Some(rig.push_commit("refs/heads/main", parent.as_deref(), &format!("c{i}")).await);
    }
    let packs = rig.sc.cell().unwrap().snap.packs.len();
    assert!(packs > 1, "the packs must accumulate for this to be about compaction: {packs}");
    assert!(
        fold::planned(&rig.sc, super::now_unix()).expect("plan").is_some(),
        "the positive control: at factor 2 this repository rebuilds its base"
    );

    // The one thing that changes. The post-batch hook and the tick both
    // go through `maybe_spawn`, and `planned` refuses at factor 0.
    rig.sc.cfg.fold_factor = 0;
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    assert!(fold::maybe_spawn(&mut rig.sc, tx, super::now_unix()).expect("plan").is_none());
    assert_eq!(rig.sc.cell().unwrap().snap.packs.len(), packs, "and nothing was compacted");

    let mut cold = Rig::with_store(rig.store.clone(), "cold").await;
    restore::restore(&mut cold.sc).await.expect("cold restore with compaction off");
    assert_eq!(cold.sc.git.ref_oid("refs/heads/main").await.unwrap(), parent);
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
/// pushing a ref that introduces no new objects.
///
/// §4 costs it at four requests: one lease renewal, one snapshot CAS,
/// and the two derived files a dumb clone reads. Two of those four are
/// now on a timer rather than on the push (`derived_every_secs`), so
/// there are two numbers and both are asserted here: four on the batch
/// that refreshes them, and TWO on every batch until the timer comes
/// round again. The second is the one a repository at rate actually
/// pays, and at 8,000 refs the two it no longer pays were a
/// full-ref-scan subprocess and a 511 KB upload.
#[tokio::test]
async fn the_fixed_per_push_s3_cost_is_four_requests_then_two() {
    let mut rig = Rig::new().await;
    rig.start().await;
    // The batch log (`log.rs`) is one more request by design, and its
    // own test below measures it against this one. Here it is off, so
    // this stays what §4 documents.
    rig.sc.cfg.log_max_entries = 0;
    let c1 = rig.stage_commit(None, &[("a.txt", "one")], "one").await;
    rig.run(vec![push(
        1,
        vec![RefUpdate { name: "refs/heads/main".into(), old_oid: zero(), new_oid: c1.clone() }],
    )])
    .await;

    // A second ref at the SAME commit: the pack is already in the
    // snapshot, so every request left is fixed overhead. The derived
    // files are due (nothing has refreshed them inside the window),
    // which is §4's four.
    rig.sc.last_derived_unix = 0;
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
        "publishing batch: renew + CAS + objects/info/packs + info/refs: {ops:?}"
    );

    // And the next one inside the window pays the renew and the CAS.
    rig.store.reset_op_counts();
    rig.run(vec![push(
        3,
        vec![RefUpdate { name: "refs/heads/third".into(), old_oid: zero(), new_oid: c1 }],
    )])
    .await;
    let ops = rig.store.op_counts();
    assert_eq!(
        rig.store.total_ops(),
        2,
        "inside the window a batch is renew + CAS and nothing else: {ops:?}"
    );
}

/// The derived files leave the push path, and nothing else does.
///
/// The control is `derived_every_secs = 0`, the shipped behaviour, in
/// which every batch republishes them. Without that arm "they were not
/// written" could mean the timer worked or could mean the write broke.
#[tokio::test]
async fn the_derived_files_are_on_a_timer_and_the_first_batch_still_publishes() {
    async fn puts_per_batch(every: u64, batches: usize) -> Vec<u64> {
        let mut rig = Rig::new().await;
        rig.start().await;
        rig.sc.cfg.log_max_entries = 0;
        rig.sc.cfg.derived_every_secs = every;
        let c1 = rig.stage_commit(None, &[("a.txt", "one")], "one").await;
        let mut out = Vec::new();
        for i in 0..batches {
            rig.store.reset_op_counts();
            rig.run(vec![push(
                i as u64 + 1,
                vec![RefUpdate {
                    name: format!("refs/heads/b{i}"),
                    old_oid: zero(),
                    new_oid: c1.clone(),
                }],
            )])
            .await;
            out.push(rig.store.total_ops());
        }
        out
    }
    let timed = puts_per_batch(60, 3).await;
    assert_eq!(
        timed,
        vec![8, 2, 2],
        "the first batch publishes (and carries this push's pack), the rest do not: {timed:?}"
    );
    let control = puts_per_batch(0, 3).await;
    assert_eq!(
        control,
        vec![8, 4, 4],
        "with the timer off every batch republishes them: {control:?}"
    );

    // The bucket still gets them: what the timer changes is when.
    let mut rig = Rig::new().await;
    rig.start().await;
    let c1 = rig.push_commit("refs/heads/main", None, "one").await;
    assert!(rig.store.get_whole(&rig.sc.cfg.info_refs_key(), None).await.is_ok());
    let _ = rig.push_commit("refs/heads/main", Some(&c1), "two").await;
    let before = rig.store.get_whole(&rig.sc.cfg.info_refs_key(), None).await.unwrap().1;
    // The tick's due check, which is what the serving loop runs.
    assert!(!batch::derived_due(&mut rig.sc, super::now_unix()), "not due inside the window");
    assert!(
        batch::derived_due(&mut rig.sc, super::now_unix() + 61),
        "due once the window has passed"
    );
    batch::publish_derived(&mut rig.sc).await.expect("derived");
    let after = rig.store.get_whole(&rig.sc.cfg.info_refs_key(), None).await.unwrap().1;
    assert_ne!(before, after, "and the refresh carries the second push's ref");
}

/// `HEAD` names the default branch. It is published once and then only
/// when it changes — not once per push, which is what the shipped code
/// did and what §3 says it must not.
#[tokio::test]
async fn head_is_published_once_not_once_per_push() {
    let mut rig = Rig::new().await;
    rig.start().await;
    rig.sc.cfg.log_max_entries = 0;
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
    rig.sc.cfg.log_max_entries = 0;
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

/// X13. The facts' one readiness decision: serving needs the phase,
/// no fence, and a renewal within the term.
#[tokio::test]
async fn readiness_needs_a_renewal_within_the_term() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let mut f = status::facts(&rig.sc, Phase::Serving);
    assert!(f.serving(), "serving, held, freshly renewed: ready");
    f.renewal_overdue = true;
    assert!(!f.serving(), "no renewal within the term: not ready (X13)");
    f.renewal_overdue = false;
    f.fenced = Some("deposed".into());
    assert!(!f.serving(), "fenced: not ready");
    f.fenced = None;
    f.phase = Phase::Pushing;
    assert!(!f.serving(), "a batch in flight: not ready");
    let doc = status::document(&status::facts(&rig.sc, Phase::Serving), super::now_unix());
    assert_eq!(doc["epoch"]["renewalOverdue"], false);
    assert_eq!(doc["epoch"]["termSecs"], rig.sc.cfg.renew_term().as_secs());
}

/// X13, the falsifier-11 shape in-process. The store stops answering
/// renewals (a transport error, not a 412). Within the term the holder
/// keeps serving; past it readiness is withdrawn, the lease is NOT
/// given up and the process is NOT fenced; when the store answers
/// again the next renewal restores readiness with the same epoch.
/// Before X13 the first assertion after the term failed: the holder
/// served for as long as the outage lasted.
#[tokio::test(start_paused = true)]
async fn a_holder_that_cannot_renew_withdraws_readiness_after_the_term_and_resumes() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let shared = shared_for(&rig, Phase::Serving);
    let tick = std::time::Duration::from_millis(100);
    let term = tick * lease::QUIET_POLLS;
    let epoch = rig.sc.hold.lease().expect("held").epoch;
    let task = lease::spawn_renewer(
        rig.store.clone() as Arc<dyn ObjectStore>,
        rig.sc.cfg.epoch_key(),
        rig.sc.hold.clone(),
        shared.clone(),
        tick,
    );
    // Healthy: renewals land, readiness holds.
    tokio::time::sleep(tick * 3).await;
    assert!(shared.lock().unwrap().serving(), "healthy: serving");

    // The store goes away for the holder. Within the term: still serving.
    rig.store.inject_epoch_renew_failures(1_000);
    rig.store.reset_op_counts();
    tokio::time::sleep(term / 2).await;
    assert!(shared.lock().unwrap().serving(), "inside the term: still serving");

    // Past the term: readiness withdrawn; nothing fenced; the lease kept;
    // the renewer still trying (the attempts are what a returning store
    // answers).
    tokio::time::sleep(term).await;
    let f = shared.lock().unwrap().clone();
    assert!(f.renewal_overdue, "past the term: overdue");
    assert!(!f.serving(), "past the term: NOT serving (X13)");
    assert!(rig.sc.hold.fenced().is_none(), "an outage is not a deposal: no fence");
    assert_eq!(rig.sc.hold.lease().map(|l| l.epoch), Some(epoch), "the lease is kept");
    assert!(renews(&rig) >= lease::QUIET_POLLS as u64, "the renewer kept trying: {} attempts", renews(&rig));

    // The store answers again: the next heartbeat lands and readiness
    // returns, same epoch — no restore, no restart.
    rig.store.inject_epoch_renew_failures(0);
    tokio::time::sleep(tick * 2).await;
    let f = shared.lock().unwrap().clone();
    assert!(!f.renewal_overdue && f.serving(), "a landed renewal restores readiness");
    assert_eq!(rig.sc.hold.lease().map(|l| l.epoch), Some(epoch), "same epoch after the outage");
    task.abort();
}

// ── undo points (X15, undo.rs) ───────────────────────────────────────

/// The leg the walgit comparison lost (P11): a branch is force-pushed
/// back one commit, and the state before it is recoverable FROM THE
/// BUCKET ALONE — the undo point names the refs and the packs, the
/// sweep leaves those packs alone while it stands, and a fresh
/// repository built from them has the pre-force tip whole.
///
/// The control is the same run with the window at 0 (undo off), which
/// is what the code did before: no point is written, and the sweep
/// takes the pack that held the rewound commit.
#[tokio::test]
async fn a_force_push_leaves_the_previous_state_recoverable_from_the_bucket() {
    for undo_on in [true, false] {
        let mut rig = Rig::new().await;
        rig.tiers_only();
        rig.sc.cfg.orphan_grace_secs = 0;
        rig.sc.cfg.undo_window_secs = if undo_on { 7 * 24 * 3600 } else { 0 };
        rig.start().await;
        let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
        let c1 = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;
        let c1_pack = rig
            .sc
            .cell()
            .unwrap()
            .snap
            .packs
            .last()
            .cloned()
            .expect("c1's pack");

        // The force-push: main back to c0. c1 is now reachable from
        // nothing the snapshot names.
        let pol = Policy { allow_non_fast_forward: vec!["*".into()], ..Policy::default() };
        let reports = batch::run_batch(
            &mut rig.sc,
            vec![push(9, vec![RefUpdate { name: "refs/heads/main".into(), old_oid: c1.clone(), new_oid: c0.clone() }])],
            &pol,
        )
        .await
        .expect("rewind batch");
        assert!(is_ok(&reports[0].results[0]), "the rewind is allowed");
        assert_eq!(rig.sc.cell().unwrap().snap.refs.get("refs/heads/main"), Some(&c0));

        let points = undo::list(rig.sc.store.as_ref(), &rig.sc.cfg, 16).await.unwrap();
        if !undo_on {
            assert!(points.is_empty(), "control: undo off writes nothing");
            // And the sweep is free to take c1's pack: a full repack
            // would drop the objects and nothing names the pack.
            let mut next = rig.sc.cell().unwrap().snap.clone();
            next.packs.retain(|p| p != &c1_pack);
            let cell = rig.sc.cell().unwrap().clone();
            let epoch = rig.sc.lease().unwrap().epoch;
            let c = snapshot::cas(rig.sc.store.as_ref(), &rig.sc.cfg, &cell, next, epoch, "test").await.unwrap();
            rig.sc.cell = Some(c);
            sweep::sweep(&mut rig.sc).await.expect("sweep");
            assert!(
                rig.store.head(&rig.sc.cfg.pack_key(&c1_pack)).await.is_err(),
                "control: with no undo point the sweep takes the rewound commit's pack"
            );
            continue;
        }

        // The point names the state before the rewind.
        assert_eq!(points.len(), 1, "one destructive push, one point");
        let p = &points[0];
        assert_eq!(p.snap.refs.get("refs/heads/main"), Some(&c1), "the point holds the pre-force tip");
        assert!(p.snap.packs.contains(&c1_pack), "and names the pack that holds it");

        // The sweep leaves that pack alone even once the snapshot has
        // stopped naming it (a fold, a base rebuild, or a repack).
        let mut next = rig.sc.cell().unwrap().snap.clone();
        next.packs.retain(|q| q != &c1_pack);
        let cell = rig.sc.cell().unwrap().clone();
        let epoch = rig.sc.lease().unwrap().epoch;
        let c = snapshot::cas(rig.sc.store.as_ref(), &rig.sc.cfg, &cell, next, epoch, "test").await.unwrap();
        rig.sc.cell = Some(c);
        sweep::sweep(&mut rig.sc).await.expect("sweep");
        rig.store
            .head(&rig.sc.cfg.pack_key(&c1_pack))
            .await
            .expect("the undo point's pack survives the sweep");

        // Recovery, from the bucket and nothing else: a fresh
        // repository, the point's packs fetched, its refs installed.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("recovered.git");
        let git = super::gitcmd::Git::new(&repo);
        git.init_bare("main", None).await.unwrap();
        let pack_dir = repo.join("objects/pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        for pack in &p.snap.packs {
            for ext in [".pack", ".idx"] {
                let name = format!("{}{ext}", pack.trim_end_matches(".pack"));
                if let Ok((_, bytes)) = rig.store.get_whole(&rig.sc.cfg.pack_key(&name), None).await {
                    std::fs::write(pack_dir.join(&name), &bytes).unwrap();
                }
            }
        }
        let mut script = String::new();
        for (name, oid) in &p.snap.refs {
            script.push_str(&format!("update {name} {oid}\n"));
        }
        let out = git.run(&["update-ref", "--stdin"], Some(script.as_bytes())).await.unwrap();
        assert!(out.ok(), "the point's refs install: {}", out.stderr);
        assert_eq!(git.ref_oid("refs/heads/main").await.unwrap(), Some(c1.clone()), "the pre-force tip is back");
        git.fsck_connectivity_all().await.expect("and the recovered repository is whole");
    }
}

/// A fast-forward writes no undo point: nothing became unreachable, and
/// an ordinary push must not pay for the destructive one's insurance.
/// The control is the force-push in the same test, which does write one.
#[tokio::test]
async fn only_a_destructive_push_writes_an_undo_point() {
    let mut rig = Rig::new().await;
    rig.tiers_only();
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    let c1 = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;
    let c2 = rig.push_commit("refs/heads/main", Some(&c1), "c2").await;
    assert!(
        undo::list(rig.sc.store.as_ref(), &rig.sc.cfg, 16).await.unwrap().is_empty(),
        "three fast-forwards, no undo point"
    );

    // A branch CREATE is not destructive either.
    let side = rig.stage_commit(Some(&c2), &[("f.txt", "side\n")], "side").await;
    let reports = rig
        .run(vec![push(6, vec![RefUpdate { name: "refs/heads/side".into(), old_oid: zero(), new_oid: side.clone() }])])
        .await;
    assert!(is_ok(&reports[0].results[0]), "the create is allowed: {:?}", reports[0].results[0]);
    assert!(
        undo::list(rig.sc.store.as_ref(), &rig.sc.cfg, 16).await.unwrap().is_empty(),
        "a create loses nothing"
    );

    // A branch delete is destructive.
    let pol = Policy { allow_non_fast_forward: vec!["*".into()], ..Policy::default() };
    let reports = batch::run_batch(
        &mut rig.sc,
        vec![push(7, vec![RefUpdate { name: "refs/heads/side".into(), old_oid: side.clone(), new_oid: zero() }])],
        &pol,
    )
    .await
    .expect("delete batch");
    assert!(is_ok(&reports[0].results[0]), "the delete is allowed: {:?}", reports[0].results[0]);
    let points = undo::list(rig.sc.store.as_ref(), &rig.sc.cfg, 16).await.unwrap();
    assert_eq!(points.len(), 1, "the delete wrote one");
    assert_eq!(points[0].snap.refs.get("refs/heads/side"), Some(&side), "with the branch it removed");
}

/// Past its window an undo point is deleted, and only then do its packs
/// become ordinary orphans. The order matters: while the point stands
/// its packs are referenced, so a sweep that deleted the copy first
/// would open a pass where the packs are free and the record of why
/// they mattered is gone.
#[tokio::test]
async fn an_undo_point_expires_before_its_packs_do() {
    let mut rig = Rig::new().await;
    rig.tiers_only();
    rig.sc.cfg.orphan_grace_secs = 0;
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    let c1 = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;
    let c1_pack = rig.sc.cell().unwrap().snap.packs.last().cloned().unwrap();
    let pol = Policy { allow_non_fast_forward: vec!["*".into()], ..Policy::default() };
    batch::run_batch(
        &mut rig.sc,
        vec![push(9, vec![RefUpdate { name: "refs/heads/main".into(), old_oid: c1, new_oid: c0 }])],
        &pol,
    )
    .await
    .expect("rewind");
    let point_key = undo::list(rig.sc.store.as_ref(), &rig.sc.cfg, 16).await.unwrap()[0].key.clone();

    // Stop naming the pack, then sweep inside the window: both survive.
    let mut next = rig.sc.cell().unwrap().snap.clone();
    next.packs.retain(|q| q != &c1_pack);
    let cell = rig.sc.cell().unwrap().clone();
    let epoch = rig.sc.lease().unwrap().epoch;
    let c = snapshot::cas(rig.sc.store.as_ref(), &rig.sc.cfg, &cell, next, epoch, "test").await.unwrap();
    rig.sc.cell = Some(c);
    sweep::sweep(&mut rig.sc).await.unwrap();
    rig.store.head(&point_key).await.expect("the point stands");
    rig.store.head(&rig.sc.cfg.pack_key(&c1_pack)).await.expect("and holds its pack");

    // Age the point past the window: this pass takes the point, and
    // the pack only on the pass after, when nothing references it.
    rig.store.backdate_epoch(&point_key, rig.sc.cfg.undo_window_secs + 1);
    sweep::sweep(&mut rig.sc).await.unwrap();
    assert!(rig.store.head(&point_key).await.is_err(), "the point expired");
    sweep::sweep(&mut rig.sc).await.unwrap();
    assert!(
        rig.store.head(&rig.sc.cfg.pack_key(&c1_pack)).await.is_err(),
        "and its pack is an ordinary orphan on the pass after"
    );
}

// ── compaction tiers (X18, fold.rs) ──────────────────────────────────

/// The fold's shape end to end: three push packs roll into one, the
/// snapshot names the roll-up and none of its inputs, the inputs are
/// uploaded before they are named (the bucket holds the roll-up when
/// the CAS lands), retention keeps the inputs on disk, the ledger sweep
/// takes them from the bucket past the grace, and a cold restore of the
/// result is whole.
#[tokio::test]
async fn a_fold_publishes_the_rolled_pack_and_the_sweep_takes_its_inputs() {
    let mut rig = Rig::new().await;
    rig.tiers_only();
    rig.sc.cfg.orphan_grace_secs = 0;
    rig.start().await;
    let mut parent: Option<String> = None;
    for i in 0..3 {
        let c = rig.push_commit("refs/heads/main", parent.as_deref(), &format!("c{i}")).await;
        parent = Some(c);
    }
    let inputs = rig.sc.cell().unwrap().snap.packs.clone();
    assert_eq!(inputs.len(), 3);

    let (plan, named) = rig.fold_once().await.expect("three equal packs fold");
    assert!(matches!(plan, fold::Plan::Fold { .. }));
    let f = named.expect("a new pack was named");
    assert_eq!(rig.sc.cell().unwrap().snap.packs, vec![f.clone()], "the roll-up replaces its inputs");
    rig.store.head(&rig.sc.cfg.pack_key(&f)).await.expect("the roll-up is in the bucket");
    rig.store.head(&rig.sc.cfg.pack_key(&f.replace(".pack", ".idx"))).await.expect("with its index");
    for p in &inputs {
        assert!(rig.sc.git.pack_path(p).exists(), "retention keeps {p} on disk");
        rig.store.head(&rig.sc.cfg.pack_key(p)).await.expect("an input is still in the bucket until the sweep");
    }
    assert_eq!(rig.sc.retained.len(), 3);
    assert_eq!(rig.sc.fold_ledger.len(), 1);

    let deleted = fold::sweep_ledger(&mut rig.sc, super::now_unix(), 64).await.expect("ledger sweep");
    assert!(deleted >= 3, "every input's files go: {deleted}");
    for p in &inputs {
        assert!(rig.store.head(&rig.sc.cfg.pack_key(p)).await.is_err(), "{p} swept");
    }
    rig.store.head(&rig.sc.cfg.pack_key(&f)).await.expect("the named pack is never swept");
    assert!(rig.sc.fold_ledger.is_empty());

    let past = super::now_unix() + rig.sc.cfg.fold_retain_secs + 1;
    let unlinked = fold::unlink_retained(&mut rig.sc, past).expect("unlink");
    assert_eq!(unlinked, 3);
    for p in &inputs {
        assert!(!rig.sc.git.pack_path(p).exists(), "{p} unlinked past retention");
    }

    let mut cold = Rig::with_store(rig.store.clone(), "cold").await;
    restore::restore(&mut cold.sc).await.expect("cold restore after a fold");
    assert_eq!(cold.sc.git.ref_oid("refs/heads/main").await.unwrap(), parent);
    cold.sc.git.fsck_connectivity_all().await.expect("the folded repository is whole");
}

/// The commit's CAS names `(snapshot.packs \ S) ∪ {F}` and never the
/// directory: a pack on disk that no batch uploaded (a refused push's)
/// stays unnamed, so the restore of that snapshot cannot refuse. The
/// control is the formula the drafts had — naming the directory would
/// name the stray, and the bucket does not hold it.
#[tokio::test]
async fn a_fold_cas_names_the_snapshots_packs_not_the_directory() {
    let mut rig = Rig::new().await;
    rig.tiers_only();
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    let c1 = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;
    // A pack on disk with no batch behind it.
    rig.stage_commit(Some(&c1), &[("stray.txt", "x\n")], "stray").await;
    let on_disk = rig.sc.git.local_packs().unwrap();
    let named_before = rig.sc.cell().unwrap().snap.packs.clone();
    let stray: Vec<&String> = on_disk.iter().filter(|p| !named_before.contains(p)).collect();
    assert_eq!(stray.len(), 1, "the stray pack is on disk");

    let (_, named) = rig.fold_once().await.expect("the two named packs fold");
    let f = named.unwrap();
    let packs = rig.sc.cell().unwrap().snap.packs.clone();
    assert_eq!(packs, vec![f]);
    assert!(!packs.contains(stray[0]), "the stray is not named");
    assert!(rig.store.head(&rig.sc.cfg.pack_key(stray[0])).await.is_err(), "and not in the bucket");

    let mut cold = Rig::with_store(rig.store.clone(), "cold").await;
    restore::restore(&mut cold.sc).await.expect("the snapshot names only what the bucket holds");
}

/// A base rebuild over an unchanged reachable set reproduces the base's
/// own name. The commit then renames nothing, uploads nothing and —
/// the case the A refuter found — never unlinks or unnames it.
#[tokio::test]
async fn a_rebuild_that_reproduces_the_bases_name_never_unlinks_it() {
    let mut rig = Rig::new().await;
    rig.sc.cfg.fold_factor = 2;
    rig.sc.cfg.base_min_bytes = 0;
    rig.sc.cfg.base_rebuild_min_secs = 0;
    rig.sc.cfg.fold_min_bytes = 0;
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    let _c1 = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;
    let (plan, named) = rig.fold_once().await.expect("no base yet: the base rule fires");
    assert!(matches!(plan, fold::Plan::Base { .. }));
    let base = named.expect("a base was named");
    assert_eq!(rig.sc.cell().unwrap().snap.packs, vec![base.clone()]);
    assert!(fold::is_base_marker(&rig.sc.cfg.repo, &base), "the base carries the marker");
    assert!(rig.sc.git.pack_path(&base.replace(".pack", ".bitmap")).exists(), "and the bitmap");
    let seq = rig.sc.cell().unwrap().snap.seq;

    // Rebuild again over the same reachable set.
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    fold::spawn(&mut rig.sc, fold::Plan::Base { inputs: vec![base.clone()] }, tx, super::now_unix())
        .expect("spawn");
    let res = rx.recv().await.unwrap();
    assert_eq!(res.pack, base, "the same objects give the same name");
    let named = fold::commit(&mut rig.sc, res, super::now_unix()).await.expect("commit");
    assert_eq!(named, None, "nothing new was named");
    assert_eq!(rig.sc.cell().unwrap().snap.packs, vec![base.clone()], "the base stays named");
    assert_eq!(rig.sc.cell().unwrap().snap.seq, seq, "no CAS was spent");
    assert!(rig.sc.git.pack_path(&base).exists(), "the base stays on disk");
    assert!(!rig.sc.retained.iter().any(|r| r.name == base), "and is not retained for unlinking");
    rig.sc.git.fsck_connectivity_all().await.expect("whole");
}

/// A base rebuild whose pack is the one already named — the reachable
/// set is exactly one push pack — still marks that pack the base and
/// still stamps the cadence, though it renames, uploads and CASes
/// nothing. The control is what the code did before: with neither, the
/// planner sees no base and proposes a rebuild again at once, which on
/// the runcc rate leg wrote a second, 128 MiB copy of the repository
/// ten seconds after the first.
#[tokio::test]
async fn a_reproduced_base_rebuild_still_marks_the_base_and_stamps_the_cadence() {
    let mut rig = Rig::new().await;
    rig.sc.cfg.fold_factor = 2;
    rig.sc.cfg.base_min_bytes = 0;
    rig.sc.cfg.base_rebuild_min_secs = 3600;
    rig.sc.cfg.fold_min_bytes = 0;
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    let one = rig.sc.cell().unwrap().snap.packs.clone();
    assert_eq!(one.len(), 1, "one push, one pack");

    let (plan, named) = rig.fold_once().await.expect("the base rule fires with no base yet");
    assert!(matches!(plan, fold::Plan::Base { .. }));
    assert_eq!(named, None, "the rebuild reproduced the push pack's name: nothing new was named");
    assert_eq!(rig.sc.cell().unwrap().snap.packs, one, "and the snapshot is unchanged");
    assert!(
        fold::is_base_marker(&rig.sc.cfg.repo, &one[0]),
        "the reproduced pack is still marked the base"
    );
    assert!(rig.sc.last_base_rebuild_unix > 0, "and the cadence is stamped");

    // A second push, then a plan: inside the cadence, with a base now
    // known, no rebuild is proposed.
    let _c1 = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;
    assert!(
        !matches!(fold::planned(&rig.sc, super::now_unix()).unwrap(), Some(fold::Plan::Base { .. })),
        "a second rebuild was proposed at once"
    );
    // The control: unmark and unstamp, as the early return left it, and
    // the planner proposes the rebuild that wrote the extra copy.
    let keep = rig.sc.cfg.repo.join("objects/pack").join(format!("{}.keep", one[0].trim_end_matches(".pack")));
    std::fs::remove_file(&keep).unwrap();
    rig.sc.last_base_rebuild_unix = 0;
    assert!(
        matches!(fold::planned(&rig.sc, super::now_unix()).unwrap(), Some(fold::Plan::Base { .. })),
        "control: with no marker and no stamp the rebuild is proposed again"
    );
}

/// The reflog trap (design §7.7): a base rebuild drops what a rewind
/// left reachable only from the reflog; a warm restart keeps the
/// reflog; the proof must not walk it. The control is the trap itself
/// on this git: without the expiry the plain proof refuses.
#[tokio::test]
async fn a_base_rebuild_after_a_rewind_survives_a_warm_restart() {
    let mut rig = Rig::new().await;
    rig.sc.cfg.fold_factor = 2;
    rig.sc.cfg.base_min_bytes = 0;
    rig.sc.cfg.base_rebuild_min_secs = 0;
    rig.sc.cfg.fold_min_bytes = 0;
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    let c1 = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;
    let c2 = rig.push_commit("refs/heads/main", Some(&c1), "c2").await;
    // Rewind main to c1: c2 is now reachable from the reflog only. (Two
    // commits stay reachable so the rebuild's pack is not byte-identical
    // to a push pack — a reproduced name is its own test above.)
    let pol = Policy { allow_non_fast_forward: vec!["*".into()], ..Policy::default() };
    let reports = batch::run_batch(
        &mut rig.sc,
        vec![push(9, vec![RefUpdate { name: "refs/heads/main".into(), old_oid: c2.clone(), new_oid: c1.clone() }])],
        &pol,
    )
    .await
    .expect("rewind batch");
    assert!(is_ok(&reports[0].results[0]), "the rewind is allowed: {:?}", reports[0].results[0]);
    // The rig's staging leaves loose copies behind that a push never
    // does (`receive.unpackLimit = 1`): drop them, or the rewound tip
    // survives the rebuild as a loose object and the trap is masked.
    rig.git(&["prune-packed", "-q"], None).await;

    // The control first, on a copy of the state: a rebuild WITHOUT the
    // expiry, its inputs unlinked, and the plain proof.
    let scratch = rig.sc.cfg.state_dir.join("control");
    std::fs::create_dir_all(&scratch).unwrap();
    let f = rig.sc.git.pack_base(&scratch.join("pack"), 1).await.unwrap().unwrap();
    let dir = rig.sc.cfg.repo.join("objects/pack");
    let old: Vec<String> = rig.sc.git.local_packs().unwrap().into_iter().filter(|p| *p != f).collect();
    assert_eq!(old.len(), 3, "the three push packs are the rebuild's inputs");
    for file in super::gitcmd::siblings_in(&scratch, &f) {
        std::fs::rename(scratch.join(&file), dir.join(&file)).unwrap();
    }
    let mut stash = Vec::new();
    for p in &old {
        for file in rig.sc.git.pack_siblings(p) {
            let bytes = std::fs::read(dir.join(&file)).unwrap();
            std::fs::remove_file(dir.join(&file)).unwrap();
            stash.push((file, bytes));
        }
    }
    let plain = rig.sc.git.run(&["fsck", "--connectivity-only", "--no-progress"], None).await.unwrap();
    assert!(!plain.ok(), "control: the plain proof walks the reflog and refuses ({})", plain.stderr.trim());
    rig.sc.git.fsck_connectivity_all().await.expect("the proof with --no-reflogs passes on the same state");
    // Put the state back.
    for file in super::gitcmd::siblings_in(&dir, &f) {
        std::fs::remove_file(dir.join(&file)).unwrap();
    }
    for (file, bytes) in stash {
        std::fs::write(dir.join(&file), bytes).unwrap();
    }

    // The design's path: expire, rebuild, commit, unlink, warm restart.
    let (plan, _) = rig.fold_once().await.expect("the base rule fires");
    assert!(matches!(plan, fold::Plan::Base { .. }));
    fold::unlink_retained(&mut rig.sc, super::now_unix() + 100_000).unwrap();
    let plain = rig.sc.git.run(&["fsck", "--connectivity-only", "--no-progress"], None).await.unwrap();
    assert!(plain.ok(), "after the expiry even the plain proof passes: {}", plain.stderr.trim());
    restore::restore(&mut rig.sc).await.expect("a warm restart serves");
    assert_eq!(rig.sc.git.ref_oid("refs/heads/main").await.unwrap(), Some(c1));
}

/// The cadence is the base's age by the store's clock, not process
/// memory: the pod P5 restarted on runca rebuilt a 12 GiB base the
/// moment it restored. A fresh incarnation restores, reads the base's
/// age from the LIST and plans no rebuild inside the cadence though
/// the tiers are past the percent; with the base aged past the cadence
/// IN THE STORE it plans one. The control is the same incarnation with
/// the age unread (`last_base_rebuild_unix = 0`, what the code did):
/// it plans the rebuild at once.
#[tokio::test]
async fn the_cadence_is_the_bases_age_in_the_store_not_process_memory() {
    let mut rig = Rig::new().await;
    rig.sc.cfg.fold_factor = 2;
    rig.sc.cfg.base_min_bytes = 0;
    rig.sc.cfg.base_rebuild_min_secs = 0;
    rig.sc.cfg.fold_min_bytes = 0;
    rig.start().await;
    // Two commits before the base, so the rebuild's pack is not a push
    // pack's byte-for-byte twin (a reproduced name is its own test).
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    let c1 = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;
    let (plan, named) = rig.fold_once().await.expect("the base rule fires");
    assert!(matches!(plan, fold::Plan::Base { .. }));
    let base = named.expect("a base was named");
    // Two more pushes: the tiers are past the percent; the cadence is
    // now an hour, and this incarnation remembers the rebuild.
    rig.sc.cfg.base_rebuild_min_secs = 3600;
    let c2 = rig.push_commit("refs/heads/main", Some(&c1), "c2").await;
    let _c3 = rig.push_commit("refs/heads/main", Some(&c2), "c3").await;
    let now = super::now_unix();
    assert!(
        !matches!(fold::planned(&rig.sc, now).unwrap(), Some(fold::Plan::Base { .. })),
        "inside the cadence the holder plans no rebuild"
    );

    // A fresh incarnation on the same store, as the restarted pod.
    let mut warm = Rig::with_store(rig.store.clone(), "warm").await;
    warm.sc.cfg.fold_factor = 2;
    warm.sc.cfg.base_min_bytes = 0;
    warm.sc.cfg.base_rebuild_min_secs = 3600;
    warm.sc.cfg.fold_min_bytes = 0;
    restore::restore(&mut warm.sc).await.expect("restore");
    assert!(warm.sc.last_base_rebuild_unix > 0, "the restore read the base's age from the LIST");
    assert!(
        !matches!(fold::planned(&warm.sc, now).unwrap(), Some(fold::Plan::Base { .. })),
        "a fresh incarnation inside the cadence plans no rebuild"
    );
    // The control: the age unread, as the code did — a rebuild at once.
    let remembered = warm.sc.last_base_rebuild_unix;
    warm.sc.last_base_rebuild_unix = 0;
    assert!(matches!(fold::planned(&warm.sc, now).unwrap(), Some(fold::Plan::Base { .. })), "control: without the age, at once");
    warm.sc.last_base_rebuild_unix = remembered;

    // The base aged past the cadence in the store: the next restore
    // reads an old base and the rebuild is due.
    rig.store.backdate_epoch(&rig.sc.cfg.pack_key(&base), 3601);
    restore::restore(&mut warm.sc).await.expect("restore again");
    assert!(
        matches!(fold::planned(&warm.sc, now).unwrap(), Some(fold::Plan::Base { .. })),
        "past the cadence by the store's clock the rebuild is planned"
    );
}

/// The check that was missing when a fold lost objects on runcd
/// (2026-09-07): a roll-up must hold everything the packs it supersedes
/// hold, and `pack-objects` is not taken at its word for it.
///
/// The failure that motivated this: a fold's output did not contain
/// thirteen commits its inputs held; the commit stopped naming the
/// inputs; the refs still pointed past the missing commits; and the next
/// cold restore could not prove the repository and refused to serve it.
/// Nothing was lost — the inputs were still in the bucket, unnamed — but
/// the repository was unservable until a human intervened.
///
/// The predicate is tested both ways round, because a coverage check
/// that always answers "covered" would have passed that fold too.
#[tokio::test]
async fn a_roll_up_that_drops_an_object_is_named_by_the_coverage_check() {
    let mut rig = Rig::new().await;
    rig.tiers_only();
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    let _c1 = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;
    let packs = rig.sc.cell().unwrap().snap.packs.clone();
    assert_eq!(packs.len(), 2, "one pack per push");
    let dir = rig.sc.cfg.repo.join("objects/pack");
    let idx = |p: &str| dir.join(p.trim_end_matches(".pack").to_string() + ".idx");

    // Each push's pack holds its own objects and not the other's, so
    // one is exactly the "roll-up that dropped something" case.
    let first = idx(&packs[0]);
    let second = idx(&packs[1]);
    let missed = rig
        .sc
        .git
        .pack_covers(&first, std::slice::from_ref(&second))
        .await
        .expect("the check runs");
    assert!(
        missed.is_some(),
        "a pack that does not hold the other's objects must be NAMED as not covering it"
    );

    // The control: a pack covers itself. Without this the assertion
    // above passes for a check that answers "missing" to everything.
    assert_eq!(
        rig.sc.git.pack_covers(&first, std::slice::from_ref(&first)).await.unwrap(),
        None,
        "a pack covers itself"
    );

    // And the real thing: a genuine fold's output covers its inputs, so
    // the check does not refuse the happy path.
    let (plan, named) = rig.fold_once().await.expect("two equal packs fold");
    assert!(matches!(plan, fold::Plan::Fold { .. }));
    let f = named.expect("the fold committed");
    let inputs: Vec<std::path::PathBuf> = packs.iter().map(|p| idx(p)).collect();
    assert_eq!(
        rig.sc.git.pack_covers(&idx(&f), &inputs).await.unwrap(),
        None,
        "the roll-up holds everything its inputs held"
    );
}

/// The refusal must name what is actually wrong. `git fsck` writes a
/// broken history to STDOUT (`missing commit …`, `broken link from …`)
/// and puts only chatter on stderr (`notice: HEAD points to an unborn
/// branch`). Reporting stderr alone is what made runcd's refusal blame
/// an unborn HEAD while thirteen commits were missing — an operator
/// reading that log would have chased the notice and never found the
/// fold that stranded them.
///
/// The repository here is broken the same way runcd's was: a ref points
/// at a commit that is present, whose PARENT is in no pack the
/// repository still has.
#[tokio::test]
async fn a_refusal_names_the_missing_commit_and_not_just_the_unborn_head_notice() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/topic", None, "c0").await;
    let c1 = rig.push_commit("refs/heads/topic", Some(&c0), "c1").await;
    // HEAD is the unborn default branch — the source of the notice that
    // masked the real fault.
    assert_eq!(rig.sc.git.head_target().await.unwrap(), "refs/heads/main");

    // Strand c0: drop every loose object and the pack that carries it,
    // keeping the pack that carries c1. The ref still points at c1.
    let objdir = rig.sc.cfg.repo.join("objects");
    for e in std::fs::read_dir(&objdir).unwrap().flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.len() == 2 && n.chars().all(|c| c.is_ascii_hexdigit()) {
            std::fs::remove_dir_all(e.path()).ok();
        }
    }
    let packs = rig.sc.cell().unwrap().snap.packs.clone();
    let dir = objdir.join("pack");
    let mut stranded = false;
    for p in &packs {
        let stem = p.trim_end_matches(".pack");
        let idx = dir.join(format!("{stem}.idx"));
        let ids = rig.sc.git.pack_object_ids(&idx).await.unwrap();
        if ids.contains(&c0) && !ids.contains(&c1) {
            for f in rig.sc.git.pack_siblings(p) {
                std::fs::remove_file(dir.join(f)).ok();
            }
            stranded = true;
        }
    }
    assert!(stranded, "the rig must put c0 and c1 in different packs for this to test anything");

    let err = rig.sc.git.fsck_connectivity_all().await.expect_err("a stranded parent fails the proof");
    let msg = format!("{err}");
    assert!(
        msg.contains(&c0),
        "the refusal must name the MISSING COMMIT (git puts it on stdout), got: {msg}"
    );
    assert!(
        !msg.contains("dangling"),
        "dangling objects are noise in a refusal, not the fault: {msg}"
    );
}

/// Refs are folded into `packed-refs` on the derived-files tick.
///
/// forge sets `gc.auto=0` and never repacked, so every ref it accepted
/// stayed a loose file forever, and `receive-pack` walked all of them on
/// every push. Measured on a scratch repository at 8,002 refs: 683 ms
/// per lone push loose against 121 ms packed, warm. The refs must come
/// out the other side naming exactly the same objects — this is a
/// storage change and nothing a reader can see.
#[tokio::test]
async fn the_derived_tick_packs_the_refs_away_without_moving_any() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let mut want = std::collections::BTreeMap::new();
    for i in 0..6 {
        let b = format!("refs/heads/agent/b{i}");
        let c = rig.push_commit(&b, None, &format!("c{i}")).await;
        want.insert(b, c);
    }
    let loose = |sc: &Syncer| -> usize {
        fn walk(d: &std::path::Path) -> usize {
            let Ok(rd) = std::fs::read_dir(d) else { return 0 };
            rd.flatten()
                .map(|e| if e.path().is_dir() { walk(&e.path()) } else { 1 })
                .sum()
        }
        walk(&sc.cfg.repo.join("refs/heads"))
    };
    let packed = rig.sc.cfg.repo.join("packed-refs");
    // The first batch of a fresh syncer publishes the derived files, so
    // one ref is packed already; every push after that leaves a loose
    // file behind, which is the state this tick exists to clear.
    assert!(loose(&rig.sc) > 0, "the later pushes left loose refs");

    batch::publish_derived(&mut rig.sc).await.expect("the derived tick runs");

    assert!(packed.exists(), "the tick wrote packed-refs");
    assert_eq!(loose(&rig.sc), 0, "and took the loose files away");
    // The oracle that matters: every ref still names what it named.
    let after = rig.sc.git.refs().await.expect("refs");
    for (name, oid) in &want {
        assert_eq!(after.get(name), Some(oid), "{name} moved");
    }
    // And a push still works afterwards, onto a packed ref.
    let c = rig.push_commit("refs/heads/agent/b0", want.get("refs/heads/agent/b0").map(|s| s.as_str()), "next").await;
    assert_eq!(rig.sc.git.ref_oid("refs/heads/agent/b0").await.unwrap(), Some(c));
}

/// runcd's defect, reproduced deterministically (2026-09-07). A batch
/// names every pack in the DIRECTORY, and git migrates a push's pack out
/// of quarantine as soon as pre-receive passes, so a push queued behind
/// the running batch has its pack named one batch BEFORE its ref moves.
/// A base rebuild planned in that gap takes the pack as an input; its
/// `--all` cannot see the commit the ref has not reached; and its commit
/// then unnames the only pack that holds it. The next cold restore
/// cannot prove the repository.
///
/// The rig can stage that exactly: a commit packed into the directory
/// with no ref (the queued push, past pre-receive), another pusher's
/// batch naming the directory, the base planned and packed in between,
/// and the queued push's own batch landing before the base commits.
#[tokio::test]
async fn a_pack_named_before_its_ref_moves_survives_the_base_rebuild_that_could_not_see_it() {
    let mut rig = Rig::new().await;
    rig.sc.cfg.fold_factor = 2;
    rig.sc.cfg.base_min_bytes = 0;
    rig.sc.cfg.base_rebuild_min_secs = 0;
    rig.sc.cfg.fold_min_bytes = 0;
    rig.start().await;
    let _c0 = rig.push_commit("refs/heads/main", None, "c0").await;

    // The queued push: past pre-receive, so its pack is in the
    // directory; its proc-receive hook is still waiting, so no ref.
    let queued = rig.stage_commit(None, &[("q.txt", "q\n")], "queued").await;
    // Another pusher's batch lands first and names the DIRECTORY.
    let _c1 = rig.push_commit("refs/heads/other", None, "c1").await;
    let packs = rig.sc.cell().unwrap().snap.packs.clone();
    let dir = rig.sc.cfg.repo.join("objects/pack");
    let mut qpack = None;
    for p in &packs {
        let idx = dir.join(p.trim_end_matches(".pack").to_string() + ".idx");
        if rig.sc.git.pack_object_ids(&idx).await.unwrap().contains(&queued) {
            qpack = Some(p.clone());
        }
    }
    let qpack = qpack.expect("the batch named the queued push's pack, one batch before its ref");
    assert!(rig.sc.git.ref_oid("refs/heads/queued").await.unwrap().is_none(), "and the ref has not moved");

    // The base rebuild, planned and packed in exactly that gap.
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let now = super::now_unix();
    let plan = fold::maybe_spawn(&mut rig.sc, tx, now).unwrap().expect("a base is planned");
    assert!(matches!(plan, fold::Plan::Base { .. }), "{plan:?}");
    assert!(plan.inputs().contains(&qpack), "the queued pack is one of the base's inputs");
    let res = rx.recv().await.expect("the task reports");
    assert!(res.error.is_none(), "the rebuild itself is fine: {:?}", res.error);

    // The queued push's own batch now runs: its ref moves onto a commit
    // whose ONLY pack is a base input the rebuild could not see.
    let reports = rig
        .run(vec![push(7, vec![RefUpdate { name: "refs/heads/queued".into(), old_oid: zero(), new_oid: queued.clone() }])])
        .await;
    assert!(is_ok(&reports[0].results[0]), "{:?}", reports[0].results[0]);

    // The commit must NOT unname that pack.
    let named = fold::commit(&mut rig.sc, res, now).await.expect("the base commits");
    assert!(named.is_some());
    let after = rig.sc.cell().unwrap().snap.packs.clone();
    assert!(after.contains(&qpack), "the pack holding a commit that became reachable during the rebuild stays named: {after:?}");

    // The proof that matters: a cold restore from the bucket is whole.
    let mut cold = Rig::with_store(rig.store.clone(), "cold").await;
    restore::restore(&mut cold.sc).await.expect("a cold restore proves the repository");
    assert_eq!(cold.sc.git.ref_oid("refs/heads/queued").await.unwrap(), Some(queued));
    cold.sc.git.fsck_connectivity_all().await.expect("whole");
}

/// TLC'S COUNTEREXAMPLE, EXECUTABLE — the ordering the test above does
/// NOT have (2026-09-08).
///
/// The sibling above lands the queued push BEFORE the fold's commit, so
/// any rule that asks "has it landed?" at commit time answers yes and
/// keeps the pack. That ordering cannot refute a reachability-based
/// supersede rule, and for a while nothing here could: the pack-pinning
/// finding's "direction 1" passed every test in this file.
///
/// `formal/ForgeSyncFoldReachableCoverage.cfg` finds the ordering that
/// kills it — the push lands AFTER the commit (states 21 then 27) — and
/// this is that trace in the rig:
///
///   the pack is on disk and NAMED, its ref has not moved;
///   a base rebuild cannot reach it, and commits;
///   only then does the push land.
///
/// At the moment of the commit the pack is indistinguishable from the
/// one in `a_refused_pushs_dead_objects_pin_their_pack_...` — objects
/// no ref reaches — and there the right answer is "dead", here it is
/// "about to be live". Strict object coverage is what refuses to guess,
/// and this test is the reason it may not be relaxed to reachability.
#[tokio::test]
async fn a_base_commit_may_not_unname_a_pack_whose_push_lands_after_it() {
    let mut rig = Rig::new().await;
    rig.sc.cfg.fold_factor = 2;
    rig.sc.cfg.base_min_bytes = 0;
    rig.sc.cfg.base_rebuild_min_secs = 0;
    rig.sc.cfg.fold_min_bytes = 0;
    rig.start().await;
    let _c0 = rig.push_commit("refs/heads/main", None, "c0").await;

    // Past pre-receive, so the pack is in the directory; the
    // proc-receive hook is still queued, so no ref names it.
    let queued = rig.stage_commit(None, &[("q.txt", "q\n")], "queued").await;
    let _c1 = rig.push_commit("refs/heads/other", None, "c1").await;
    let qpack = named_holder_of(&rig, &queued)
        .await
        .expect("a batch named the queued push's pack, one batch before its ref");
    assert!(
        rig.sc.git.ref_oid("refs/heads/queued").await.unwrap().is_none(),
        "and the ref has not moved — this is the gap"
    );

    // The base rebuild, planned and packed in exactly that gap.
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let now = super::now_unix();
    let plan = fold::maybe_spawn(&mut rig.sc, tx, now).unwrap().expect("a base is planned");
    assert!(matches!(plan, fold::Plan::Base { .. }), "{plan:?}");
    assert!(plan.inputs().contains(&qpack), "the queued pack is one of the base's inputs");
    let res = rx.recv().await.expect("the task reports");
    assert!(res.error.is_none(), "the rebuild itself is fine: {:?}", res.error);

    // THE COMMIT GOES FIRST. Everything the syncer can see right now
    // says this pack holds nothing any ref reaches.
    let named = fold::commit(&mut rig.sc, res, now).await.expect("the base commits");
    assert!(named.is_some());
    assert!(
        rig.sc.cell().unwrap().snap.packs.contains(&qpack),
        "a pack whose push has not landed YET must stay named: {:?}",
        rig.sc.cell().unwrap().snap.packs
    );

    // ...and only now does the push land.
    let reports = rig
        .run(vec![push(
            7,
            vec![RefUpdate { name: "refs/heads/queued".into(), old_oid: zero(), new_oid: queued.clone() }],
        )])
        .await;
    assert!(is_ok(&reports[0].results[0]), "{:?}", reports[0].results[0]);

    // The proof that matters, and the one that fails when the rule is
    // relaxed: a cold restore from the bucket alone is whole.
    let mut cold = Rig::with_store(rig.store.clone(), "cold").await;
    restore::restore(&mut cold.sc).await.expect("a cold restore proves the repository");
    assert_eq!(cold.sc.git.ref_oid("refs/heads/queued").await.unwrap(), Some(queued));
    cold.sc.git.fsck_connectivity_all().await.expect("whole");
}


/// Which pack the SNAPSHOT names holds `oid`, if any. The question the
/// pinning finding is about: not "is it on disk" but "does a restore
/// download it".
async fn named_holder_of(rig: &Rig, oid: &str) -> Option<String> {
    let dir = rig.sc.cfg.repo.join("objects/pack");
    let mut found = None;
    for p in &rig.sc.cell().unwrap().snap.packs.clone() {
        let idx = dir.join(p.trim_end_matches(".pack").to_string() + ".idx");
        if rig.sc.git.pack_object_ids(&idx).await.unwrap().iter().any(|o| o == oid) {
            found = Some(p.clone());
        }
    }
    found
}

/// THE PRICE OF THE RULE THE TEST ABOVE BUYS (runcl, 2026-09-08), and a
/// guard on anyone who tries to stop paying it.
///
/// The same shape with ONE dimension moved: the staged push is REFUSED
/// instead of landing. Its pack is in the directory either way — git
/// migrates the quarantine as soon as pre-receive passes, before the
/// proc-receive hook that carries forge's answer — and a later batch
/// names the directory, so a correctly refused push leaves objects the
/// snapshot names and no ref can ever reach.
///
/// A base rebuild (`--all`) then drops them, and the supersede check is
/// strict object coverage, so the input pack stays named. FOREVER. On
/// runcl that was 45,664 of 79,159 snapshot-named bytes — 58% —
/// downloaded by every restore, for three dead objects.
///
/// Keeping it named is nonetheless CORRECT, and that is the point of
/// putting this test beside the one above: at the moment of the
/// decision the two are the SAME OBSERVATION — a named pack holding
/// objects no ref reaches — and one of them is a push about to land.
/// TLC refutes every rule that tries to separate them
/// (`ForgeSyncFoldReachableCoverage.cfg` violates
/// `Inv_LandedPackComplete`; it is runcd with the arrow reversed).
///
/// So this asserts the SAFETY half — the pack stays named — and only
/// MEASURES the cost. It is not a claim that the cost is desirable; it
/// is a claim that reclaiming it by weakening coverage breaks the test
/// above, and must be done somewhere no push can be in flight.
#[tokio::test]
async fn a_refused_pushs_dead_objects_pin_their_pack_and_coverage_keeps_it_named() {
    let mut rig = Rig::new().await;
    // Tiers first and no base yet: the amplification step needs a tier
    // fold to happen BEFORE the collection, which is the runcl order.
    rig.tiers_only();
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;

    // A push git has already given the repository and forge is about to
    // refuse. NOTE the rig's limits, because a plan was once written on
    // the assumption that this shape is what a cluster produces:
    // `stage_commit` packs with NO `--fix-thin` pass, so this pack holds
    // only dead objects. A real `receive-pack` completes a thin pack
    // with the delta bases the server already holds, which ARE
    // reachable — measured on git 2.50.1, a one-line edit to a
    // 4,000-line file leaves a 4-object residue pack of which 1 object
    // is live. Any rule keyed on "no object here is reachable" fires in
    // this test and not on that repository.
    let dead = rig.stage_commit(None, &[("d.txt", "d\n")], "dead").await;
    let reports = rig
        .run(vec![push(
            9,
            vec![RefUpdate { name: "refs/heads/main".into(), old_oid: c0.clone(), new_oid: dead.clone() }],
        )])
        .await;
    assert_eq!(
        ng_reason(&reports[0].results[0]),
        "non-fast-forward update to refs/heads/main",
        // NOT arm A's shape, and an earlier version of this comment said
        // it was. `dead` is a ROOT commit, so this is an unrelated-history
        // force push. Arm A never force-pushes, and `judge` tests
        // staleness BEFORE ancestry (batch.rs), so its 28 refusals were
        // `stale info: fetch first`. The residue mechanism is the same
        // either way; the CLASS is not, and a plan was mis-scoped off
        // this sentence.
        "an unrelated-history push, refused"
    );
    // A batch that accepts nothing spends no CAS, so the refusal alone
    // names nothing. The NEXT push is what names the directory.
    let c1 = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;

    let dir = rig.sc.cfg.repo.join("objects/pack");
    assert!(
        named_holder_of(&rig, &dead).await.is_some(),
        "a batch named the refused push's pack — it names the DIRECTORY"
    );
    assert_eq!(
        rig.sc.git.ref_oid("refs/heads/main").await.unwrap(),
        Some(c1.clone()),
        "and no ref moved onto the refused commit"
    );

    // THE AMPLIFICATION. A tier fold is a pure roll-up
    // (`pack-objects --stdin-packs`) and must hold every object its
    // inputs hold, so it carries the dead triple into a pack that is
    // otherwise all live content. Three objects now ride a big pack.
    rig.fold_once().await.expect("two equal packs fold");
    let dpack = named_holder_of(&rig, &dead)
        .await
        .expect("the tier fold carried the dead objects into its roll-up");

    // Now the collection. `--all` cannot reach the refused commit, and
    // never will.
    rig.sc.cfg.base_min_bytes = 0;
    rig.sc.cfg.base_rebuild_min_secs = 0;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let now = super::now_unix();
    let plan = fold::maybe_spawn(&mut rig.sc, tx, now).unwrap().expect("a base is planned");
    assert!(matches!(plan, fold::Plan::Base { .. }), "{plan:?}");
    assert!(plan.inputs().contains(&dpack), "the refused push's pack is one of the base's inputs");
    let res = rx.recv().await.expect("the task reports");
    assert!(res.error.is_none(), "the rebuild itself is fine: {:?}", res.error);
    let base_name = res.pack.clone();

    let named = fold::commit(&mut rig.sc, res, now).await.expect("the base commits");
    assert!(named.is_some());
    let after = rig.sc.cell().unwrap().snap.packs.clone();

    // THE FINDING. The roll-up is not to blame and the commit is not
    // to blame: coverage is doing exactly what runcd bought it for.
    assert!(
        after.contains(&dpack),
        "the pack holding only dead objects stays named — coverage cannot tell it from the \
         queued push in the test above: {after:?}"
    );

    // ...and the price, measured rather than asserted: everything in
    // that pack except the dead objects is already in the base.
    let bidx = dir.join(base_name.trim_end_matches(".pack").to_string() + ".idx");
    let didx = dir.join(dpack.trim_end_matches(".pack").to_string() + ".idx");
    let held: std::collections::HashSet<String> =
        rig.sc.git.pack_object_ids(&bidx).await.unwrap().into_iter().collect();
    let pinned = rig.sc.git.pack_object_ids(&didx).await.unwrap();
    let uncovered: Vec<&String> = pinned.iter().filter(|o| !held.contains(*o)).collect();
    assert!(
        uncovered.contains(&&dead),
        "the refused commit is what the base could not reach: {uncovered:?}"
    );
    assert!(
        uncovered.len() * 2 < pinned.len(),
        "a pack is pinned by a MINORITY of its objects — {} of {} — which is what makes the \
         redundancy worth naming: on runcl it was 3 of 263, twice over",
        uncovered.len(),
        pinned.len()
    );

    // And it is a COST, not a defect: the bucket still restores whole.
    let mut cold = Rig::with_store(rig.store.clone(), "cold").await;
    restore::restore(&mut cold.sc).await.expect("a cold restore proves the repository");
    assert_eq!(cold.sc.git.ref_oid("refs/heads/main").await.unwrap(), Some(c1));
    cold.sc.git.fsck_connectivity_all().await.expect("whole");
    assert!(
        cold.sc.git.has_object(&dead).await.unwrap(),
        "and the restore downloaded the dead objects too — that is the whole cost"
    );
}


/// A batch beside a fold (design §3.5). The fold's upload is slow and a
/// push lands while it is in flight: the batch runs on the loop with
/// the fold's task beside it, never sees the scratch — its snapshot
/// names its own pack and none of the fold's — and the fold's commit
/// afterwards CASes on the BATCH's snapshot (the loop's current belief,
/// not the etag the fold was planned under), naming the batch's pack,
/// the roll-up, and none of the roll-up's inputs, with nothing fenced.
/// The ordering holds by construction, not by the clock: the fold's
/// first upload is parked at the store until the batch has returned.
#[tokio::test]
async fn a_batch_beside_a_fold_names_its_own_pack_and_the_fold_commits_on_the_batchs_snapshot() {
    let mut rig = Rig::new().await;
    rig.tiers_only();
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    let c1 = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;
    let inputs = rig.sc.cell().unwrap().snap.packs.clone();
    assert_eq!(inputs.len(), 2);
    let seq0 = rig.sc.cell().unwrap().snap.seq;

    // Every whole PUT parks at the store's door until released: the
    // fold's first sibling stops there and stays there, whatever the
    // clock does. (A fixed three-second delay here was a race: under a
    // loaded suite the batch outlasted the sleep and the fold finished
    // first.)
    rig.store.inject_put_hold(true);
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let plan = fold::maybe_spawn(&mut rig.sc, tx, super::now_unix()).unwrap().expect("two equal packs fold");
    assert!(matches!(plan, fold::Plan::Fold { .. }));
    // Wait for pack-objects to finish and the first PUT to park: a wait
    // for progress, not a window.
    for _ in 0..1500 {
        if rig.store.held_puts() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(rig.store.held_puts(), 1, "the fold's first upload is parked at the store");
    assert_eq!(rig.sc.fold.as_ref().unwrap().stage.lock().map(|s| *s).unwrap(), "uploading");
    // New PUTs flow from here — the batch's own — while the parked one
    // stays parked.
    rig.store.inject_put_hold(false);

    // The batch, beside the fold's upload.
    let c2 = rig.push_commit("refs/heads/main", Some(&c1), "c2").await;
    let stage = rig.sc.fold.as_ref().expect("the fold is still in flight").stage.lock().map(|s| *s).unwrap();
    assert_eq!(stage, "uploading", "the fold was still uploading when the batch returned: its first PUT is parked");
    let after_batch = rig.sc.cell().unwrap().snap.packs.clone();
    assert_eq!(after_batch.len(), 3, "the batch named its own pack beside the two inputs");
    for p in &inputs {
        assert!(after_batch.contains(p), "the batch re-named the input {p}, which is still what git sees");
    }
    let c2_pack: String = after_batch.iter().find(|p| !inputs.contains(p)).cloned().unwrap();
    assert_eq!(rig.sc.cell().unwrap().snap.seq, seq0 + 1);

    // Let the fold's upload through: it lands and commits on the
    // batch's snapshot.
    rig.store.release_held_puts();
    let res = rx.recv().await.expect("the fold task reports");
    assert!(res.error.is_none(), "{:?}", res.error);
    let f = res.pack.clone();
    assert!(!after_batch.contains(&f), "the batch never saw the scratch");
    let named = fold::commit(&mut rig.sc, res, super::now_unix()).await.expect("the commit CASes on the batch's snapshot");
    assert_eq!(named, Some(f.clone()));
    assert!(rig.sc.fenced().is_none(), "nothing fenced: the fold committed on the loop's current belief");
    let mut want = vec![c2_pack.clone(), f.clone()];
    want.sort();
    assert_eq!(rig.sc.cell().unwrap().snap.packs, want, "the batch's pack and the roll-up, none of the inputs");
    assert_eq!(rig.sc.cell().unwrap().snap.seq, seq0 + 2);
    assert_eq!(rig.sc.retained.len(), 2, "the inputs are retained for readers");
    assert_eq!(rig.sc.git.ref_oid("refs/heads/main").await.unwrap(), Some(c2.clone()));

    // A cold restore of the result is whole, and a batch after the
    // commit uploads nothing of the roll-up.
    let mut cold = Rig::with_store(rig.store.clone(), "cold").await;
    restore::restore(&mut cold.sc).await.expect("cold restore");
    assert_eq!(cold.sc.git.ref_oid("refs/heads/main").await.unwrap(), Some(c2));
    cold.sc.git.fsck_connectivity_all().await.expect("whole");
}

/// The fold's commit renews the lease first, so a deposed holder cannot
/// land it (`ForgeSync.tla`'s `FoldNoRenew`; the trace: a holder deposed
/// while its restore ran, whose restore then read the successor's
/// rotated snapshot, so its If-Match would have matched). The control is
/// the same commit with the lease still held: it lands.
#[tokio::test]
async fn a_deposed_holders_fold_commit_is_refused_by_its_renewal() {
    let store = Arc::new(MemoryStore::new());
    let mut rig = Rig::with_store(store.clone(), "a").await;
    rig.tiers_only();
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    let _c1 = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;

    // The fold's task runs while this holder still holds.
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let plan = fold::maybe_spawn(&mut rig.sc, tx, super::now_unix()).unwrap().expect("planned");
    assert!(matches!(plan, fold::Plan::Fold { .. }));
    let res = rx.recv().await.expect("the task reports");
    assert!(res.error.is_none());

    // A successor takes the lease over: this holder is deposed and does
    // not know it. Its belief still matches the snapshot, so the
    // snapshot CAS alone would let the commit land.
    let mut heir = Rig::with_store(store.clone(), "b").await;
    store.backdate_epoch(&rig.sc.cfg.epoch_key(), 10_000);
    heir.start().await;
    assert!(heir.sc.lease().unwrap().epoch > rig.sc.lease().unwrap().epoch, "the heir holds a later epoch");
    let before = rig.sc.cell().unwrap().snap.clone();

    let err = fold::commit(&mut rig.sc, res, super::now_unix()).await.expect_err("the renewal refuses");
    assert!(matches!(err, ForgeError::Fenced(_)), "deposed at renew is the fence, got {err:?}");
    assert!(rig.sc.fenced().is_some());
    let snap_now = snapshot::load(rig.sc.store.as_ref(), &rig.sc.cfg).await.unwrap().snap;
    assert_eq!(snap_now.packs, before.packs, "no straggler CAS landed");
    assert!(rig.sc.retained.is_empty(), "and nothing was retained for the ledger sweep to delete");

    // The control: the heir's own fold commits, with its lease held.
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    heir.tiers_only();
    let plan = fold::maybe_spawn(&mut heir.sc, tx, super::now_unix()).unwrap().expect("the heir plans");
    assert!(matches!(plan, fold::Plan::Fold { .. }));
    let res = rx.recv().await.unwrap();
    let named = fold::commit(&mut heir.sc, res, super::now_unix()).await.expect("the holder's commit lands");
    assert!(named.is_some());
}

/// The fold ticks its own counter, never the hold's: a fold's upload
/// must not keep a wedged batch's holder renewing (F21). The commit's
/// single tick is the loop's, after the CAS.
#[tokio::test]
async fn the_fold_never_ticks_the_holds_counter() {
    let mut rig = Rig::new().await;
    rig.tiers_only();
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    let _ = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;
    let before = rig.sc.hold.progress();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let plan = fold::maybe_spawn(&mut rig.sc, tx, super::now_unix()).unwrap().expect("planned");
    assert!(matches!(plan, fold::Plan::Fold { .. }));
    let res = rx.recv().await.unwrap();
    assert!(res.error.is_none());
    assert!(rig.sc.fold.as_ref().unwrap().progress.load(std::sync::atomic::Ordering::Relaxed) > 0, "the fold's own counter moved");
    assert_eq!(rig.sc.hold.progress(), before, "the hold's did not");
    fold::commit(&mut rig.sc, res, super::now_unix()).await.unwrap();
}

/// Both sweeps refuse while a fold is in flight: the upload sweep's
/// premise ("nothing of ours is in flight") is false then, and it
/// would abort the holder's own base rebuild.
#[tokio::test]
async fn the_sweeps_refuse_while_a_fold_is_in_flight() {
    let mut rig = Rig::new().await;
    rig.tiers_only();
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    let _ = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    fold::maybe_spawn(&mut rig.sc, tx, super::now_unix()).unwrap().expect("planned");
    assert!(sweep::sweep(&mut rig.sc).await.is_err(), "the full sweep waits");
    assert!(sweep::abort_orphaned_uploads(&rig.sc).await.is_err(), "the upload sweep waits");
    let res = rx.recv().await.unwrap();
    fold::commit(&mut rig.sc, res, super::now_unix()).await.unwrap();
    sweep::sweep(&mut rig.sc).await.expect("and runs once the fold is committed");
}

/// The ledger sweep deletes past the grace by the store's clock, and
/// never a pack the snapshot names.
#[tokio::test]
async fn the_ledger_sweep_deletes_only_past_the_grace_and_only_unnamed() {
    let mut rig = Rig::new().await;
    rig.tiers_only();
    rig.sc.cfg.orphan_grace_secs = 3600;
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    let _ = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;
    let inputs = rig.sc.cell().unwrap().snap.packs.clone();
    let (_, named) = rig.fold_once().await.unwrap();
    let f = named.unwrap();
    let now = super::now_unix();
    assert_eq!(fold::sweep_ledger(&mut rig.sc, now, 64).await.unwrap(), 0, "inside the grace nothing goes");
    assert_eq!(rig.sc.fold_ledger.len(), 1, "the entry waits");
    rig.sc.cfg.orphan_grace_secs = 0;
    let deleted = fold::sweep_ledger(&mut rig.sc, now, 64).await.unwrap();
    assert!(deleted >= 2);
    for p in &inputs {
        assert!(rig.store.head(&rig.sc.cfg.pack_key(p)).await.is_err());
    }
    rig.store.head(&rig.sc.cfg.pack_key(&f)).await.expect("the named roll-up stays");
}

/// The restore reconciles packs as it reconciles refs: a local pack
/// the snapshot does not name is unlinked — unless retention keeps it.
/// This is what makes every fold crash window benign.
#[tokio::test]
async fn a_restore_prunes_packs_the_snapshot_does_not_name_unless_retained() {
    let mut rig = Rig::new().await;
    rig.tiers_only();
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    rig.stage_commit(Some(&c0), &[("stray.txt", "x\n")], "stray").await;
    rig.stage_commit(Some(&c0), &[("kept.txt", "y\n")], "kept").await;
    let named = rig.sc.cell().unwrap().snap.packs.clone();
    let unnamed: Vec<String> =
        rig.sc.git.local_packs().unwrap().into_iter().filter(|p| !named.contains(p)).collect();
    assert_eq!(unnamed.len(), 2);
    rig.sc.retained.push(fold::Retained { name: unnamed[1].clone(), unlink_after_unix: u64::MAX });
    fold::save_retained(&rig.sc).unwrap();
    restore::restore(&mut rig.sc).await.expect("warm restore");
    assert!(!rig.sc.git.pack_path(&unnamed[0]).exists(), "the stray is unlinked");
    assert!(rig.sc.git.pack_path(&unnamed[1]).exists(), "the retained pack is kept");
    assert!(rig.sc.git.pack_path(&named[0]).exists(), "the named pack is kept");
}

/// The counters M6's vacuity guard rests on. A byte window in which no
/// fold committed has measured the ladder's ABSENCE, not the ladder, and
/// the drill needs that as a fact reported by the process rather than a
/// grep of its log.
///
/// The control is the leg that must NOT count: a plan that produces
/// nothing to commit leaves the counter where it was. Without it this
/// asserts only that a number goes up.
#[tokio::test]
async fn a_landed_fold_is_counted_and_a_fold_that_does_not_land_is_not() {
    let mut rig = Rig::new().await;
    rig.tiers_only();
    rig.start().await;
    assert_eq!(rig.sc.folds_committed, 0, "nothing has folded yet");
    assert_eq!(rig.sc.base_rebuilds, 0);

    // The control, first and at the same ordinal position as the real
    // fold: one pack cannot make a tier, so nothing is planned and
    // nothing may be counted.
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    assert!(fold::planned(&rig.sc, super::now_unix()).unwrap().is_none(), "one pack plans no fold");
    assert_eq!(rig.sc.folds_committed, 0, "a fold that never planned is not counted");

    // Now the real one: two more pushes make three packs, which is a tier.
    let mut parent = Some(c0);
    for i in 1..3 {
        parent = Some(rig.push_commit("refs/heads/main", parent.as_deref(), &format!("c{i}")).await);
    }
    let (_, named) = rig.fold_once().await.expect("three packs fold");
    assert!(named.is_some(), "the roll-up was named");
    assert_eq!(rig.sc.folds_committed, 1, "the landed fold is counted");
    assert_eq!(rig.sc.base_rebuilds, 0, "and it was a tier fold, not a base rebuild");

    // The counter is what /status reports, which is what the drill reads.
    let f = fold::facts(&rig.sc);
    assert_eq!(f.committed, 1);
    assert_eq!(f.base_rebuilds, 0);
}

/// Retention never unlinks a pack the snapshot NAMES, however long its
/// deadline has lapsed. The protocol cannot reach that state — see the
/// comment on `unlink_retained` — but `ForgeSync.tla` can, and the cost
/// is the repository: the proof hardlinks every named pack, so one
/// missing from disk refuses to serve until a restore refetches it.
///
/// The unnamed arm is the control. Watching the named pack survive on
/// its own would pass just as well if the sweep had stopped deleting
/// anything at all.
#[tokio::test]
async fn retention_never_unlinks_a_pack_the_snapshot_names() {
    let mut rig = Rig::new().await;
    rig.tiers_only();
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    rig.stage_commit(Some(&c0), &[("stray.txt", "x\n")], "stray").await;
    let named = rig.sc.cell().unwrap().snap.packs.clone();
    let unnamed: Vec<String> =
        rig.sc.git.local_packs().unwrap().into_iter().filter(|p| !named.contains(p)).collect();
    assert_eq!(unnamed.len(), 1, "one pack on disk that the snapshot does not name");

    // Both entries are past their deadline; only the unnamed one may go.
    rig.sc.retained.push(fold::Retained { name: named[0].clone(), unlink_after_unix: 0 });
    rig.sc.retained.push(fold::Retained { name: unnamed[0].clone(), unlink_after_unix: 0 });
    fold::save_retained(&rig.sc).unwrap();

    let unlinked = fold::unlink_retained(&mut rig.sc, u64::MAX).unwrap();
    assert_eq!(unlinked, 1, "the unnamed pack is unlinked and the named one is not");
    assert!(
        !rig.sc.git.pack_path(&unnamed[0]).exists(),
        "the control: an unnamed retained pack past its deadline still goes"
    );
    assert!(
        rig.sc.git.pack_path(&named[0]).exists(),
        "a pack the snapshot names survives its lapsed retention"
    );
    assert!(
        rig.sc.retained.iter().any(|r| r.name == named[0]),
        "and it stays on the retained list, to be unlinked once nothing names it"
    );
}

/// A retained pack is subtracted from every listing: the batch after a
/// fold names the roll-up and its own pack, never the inputs retention
/// still holds on disk — which would re-upload them under rule 4.
#[tokio::test]
async fn a_retained_pack_is_never_named_by_a_batch() {
    let mut rig = Rig::new().await;
    rig.tiers_only();
    rig.start().await;
    let c0 = rig.push_commit("refs/heads/main", None, "c0").await;
    let c1 = rig.push_commit("refs/heads/main", Some(&c0), "c1").await;
    let inputs = rig.sc.cell().unwrap().snap.packs.clone();
    let (_, named) = rig.fold_once().await.unwrap();
    let f = named.unwrap();
    for p in &inputs {
        assert!(rig.sc.git.pack_path(p).exists(), "still on disk");
    }
    let _ = rig.push_commit("refs/heads/main", Some(&c1), "c2").await;
    let packs = rig.sc.cell().unwrap().snap.packs.clone();
    assert_eq!(packs.len(), 2, "the roll-up and the new push: {packs:?}");
    assert!(packs.contains(&f));
    for p in &inputs {
        assert!(!packs.contains(p), "{p} is retained, not re-named");
    }
    // `objects/info/packs` is the snapshot's list, not the directory's.
    // Written on the batch here rather than on the timer: what this
    // asserts is WHICH packs the derived list names, not when.
    rig.sc.cfg.derived_every_secs = 0;
    batch::publish_derived(&mut rig.sc).await.expect("derived");
    let info = std::fs::read_to_string(rig.sc.cfg.repo.join("objects/info/packs")).unwrap();
    let listed: Vec<&str> = info.lines().filter_map(|l| l.strip_prefix("P ")).collect();
    let mut want: Vec<&str> = packs.iter().map(|s| s.as_str()).collect();
    want.sort();
    let mut got = listed.clone();
    got.sort();
    assert_eq!(got, want, "info/packs lists exactly the snapshot's packs");
}

// ── the batch log, and the wake it makes cheap (X15's second half, X14) ─

/// The log's price, measured against the same batch without it.
///
/// It is one PUT and it must stay one PUT. The batch it is measured on
/// is one inside the derived files' window — the shape a repository at
/// rate actually runs — so the control is two requests and the log
/// makes it three.
#[tokio::test]
async fn the_batch_log_costs_exactly_one_put_per_batch() {
    async fn cost(log_on: bool) -> u64 {
        let mut rig = Rig::new().await;
        rig.start().await;
        rig.sc.cfg.log_max_entries = if log_on { 512 } else { 0 };
        let c1 = rig.stage_commit(None, &[("a.txt", "one")], "one").await;
        rig.run(vec![push(
            1,
            vec![RefUpdate { name: "refs/heads/main".into(), old_oid: zero(), new_oid: c1.clone() }],
        )])
        .await;
        // A second ref at the same commit: no new pack, so what is left
        // is fixed overhead only.
        rig.store.reset_op_counts();
        rig.run(vec![push(
            2,
            vec![RefUpdate { name: "refs/heads/side".into(), old_oid: zero(), new_oid: c1 }],
        )])
        .await;
        rig.store.total_ops()
    }
    let off = cost(false).await;
    let on = cost(true).await;
    assert_eq!(off, 2, "the control inside the derived window is renew + CAS");
    assert_eq!(on, off + 1, "the log is one PUT per batch and nothing else");
}

/// What one entry says: the refs that moved, the pack that appeared,
/// and the files beside it a follower must fetch.
#[tokio::test]
async fn a_batch_leaves_a_log_entry_naming_what_it_changed() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c1 = rig.push_commit("refs/heads/main", None, "one").await;
    let seq = snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await.unwrap().snap.seq;

    let entry = super::log::read(rig.store.as_ref(), &rig.sc.cfg, seq)
        .await
        .expect("read")
        .expect("the batch leaves an entry at its own seq");
    assert_eq!(entry.from_seq + 1, entry.seq, "an entry chains to the seq it replaced");
    assert_eq!(entry.refs.get("refs/heads/main"), Some(&c1), "the ref it moved, by name");
    assert_eq!(entry.packs_added.len(), 1, "the pack the push brought: {:?}", entry.packs_added);
    let files = &entry.packs_added[0].files;
    assert!(
        files.iter().any(|f| f.ends_with(".pack")) && files.iter().any(|f| f.ends_with(".idx")),
        "a follower fetches files, not stems: {files:?}"
    );

    // A delete is spelled as the empty oid, so applying an entry can
    // remove a ref rather than only move one.
    rig.run(vec![push(
        9,
        vec![RefUpdate { name: "refs/heads/main".into(), old_oid: c1.clone(), new_oid: zero() }],
    )])
    .await;
    let seq2 = snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await.unwrap().snap.seq;
    let del = super::log::read(rig.store.as_ref(), &rig.sc.cfg, seq2)
        .await
        .expect("read")
        .expect("entry");
    assert_eq!(del.refs.get("refs/heads/main"), Some(&String::new()), "a delete is the empty oid");
}

/// A fold moves no ref and changes the pack set. A follower that only
/// watched refs would keep fetching packs the bucket no longer holds,
/// so the entry carries both sides of the swap.
#[tokio::test]
async fn a_fold_commit_leaves_a_log_entry_naming_the_swap() {
    let mut rig = Rig::new().await;
    rig.start().await;
    rig.tiers_only();
    let mut parent = None;
    for i in 0..3 {
        parent = Some(rig.push_commit("refs/heads/main", parent.as_deref(), &format!("c{i}")).await);
    }
    let (plan, named) = rig.fold_once().await.expect("a fold is planned");
    let named = named.expect("the fold commits");
    let seq = snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await.unwrap().snap.seq;
    let entry = super::log::read(rig.store.as_ref(), &rig.sc.cfg, seq)
        .await
        .expect("read")
        .expect("the fold's commit leaves an entry");
    assert!(entry.refs.is_empty(), "a fold moves no ref: {:?}", entry.refs);
    assert_eq!(entry.packs_added.len(), 1, "one pack in: {:?}", entry.packs_added);
    assert_eq!(entry.packs_added[0].pack, named, "the pack the fold named");
    assert_eq!(
        entry.packs_removed.len(),
        plan.inputs().len(),
        "and its inputs out: {:?}",
        entry.packs_removed
    );
}

/// The log is bounded by count, and the pruner runs inside the sweep.
#[tokio::test]
async fn the_log_keeps_only_the_newest_entries() {
    let mut rig = Rig::new().await;
    rig.start().await;
    rig.sc.cfg.log_max_entries = 3;
    let mut parent = None;
    for i in 0..6 {
        parent = Some(rig.push_commit("refs/heads/main", parent.as_deref(), &format!("c{i}")).await);
    }
    let before = super::log::seqs(rig.store.as_ref(), &rig.sc.cfg).await.expect("seqs");
    assert!(before.len() > 3, "six batches leave six entries: {before:?}");
    sweep::sweep(&mut rig.sc).await.expect("sweep");
    let after = super::log::seqs(rig.store.as_ref(), &rig.sc.cfg).await.expect("seqs");
    assert_eq!(after.len(), 3, "the newest three survive: {after:?}");
    assert_eq!(after, before[before.len() - 3..], "and they are the newest, not the oldest");

    // Turned off, the pruner takes the lot: entries a previous
    // configuration wrote are ordinary rubbish once nothing follows.
    rig.sc.cfg.log_max_entries = 0;
    sweep::sweep(&mut rig.sc).await.expect("sweep");
    assert!(super::log::seqs(rig.store.as_ref(), &rig.sc.cfg).await.unwrap().is_empty());
}

/// The point of the log: a follower catches up on the deltas and never
/// reads the snapshot.
///
/// The oracle is a POISONED snapshot. After the follower has a
/// position, the snapshot object is overwritten with bytes
/// `snapshot::load` refuses; a follower that reads it fails loudly, so
/// a pass that converges cannot have read it. The control is the same
/// run with the log off, which must NOT converge.
#[tokio::test]
async fn a_follower_catches_up_from_the_log_and_not_from_the_snapshot() {
    let store = Arc::new(MemoryStore::new());
    let mut a = Rig::with_store(store.clone(), "a").await;
    a.start().await;
    let c1 = a.push_commit("refs/heads/main", None, "one").await;

    // The follower's first pass: no position yet, so it reads the
    // snapshot once and proves what it fetched.
    let mut b = Rig::with_store(store.clone(), "b").await;
    let first = super::follow::warm(&mut b.sc).await.expect("first warm");
    assert_eq!(first.entries, None, "the first pass has no position and reads the snapshot");
    assert!(first.files_fetched > 0, "and it brings the packs down");
    assert_eq!(b.sc.git.ref_oid("refs/heads/main").await.unwrap().as_deref(), Some(c1.as_str()));

    // Two more batches on the holder, then the snapshot is made
    // unreadable.
    let c2 = a.push_commit("refs/heads/main", Some(&c1), "two").await;
    let c3 = a.push_commit("refs/heads/main", Some(&c2), "three").await;
    store.raw_put(&a.sc.cfg.snapshot_key(), bytes::Bytes::from_static(b"not json"), vec![]);
    assert!(
        snapshot::load(store.as_ref(), &a.sc.cfg).await.is_err(),
        "the poison must actually poison"
    );

    let second = super::follow::warm(&mut b.sc).await.expect("the follower catches up on the log");
    assert_eq!(second.entries, Some(2), "two batches, two entries: {second:?}");
    assert_eq!(
        b.sc.git.ref_oid("refs/heads/main").await.unwrap().as_deref(),
        Some(c3.as_str()),
        "and it lands on the holder's tip"
    );
    assert!(matches!(second.proof, Some(super::follow::Proof::Delta { .. })), "{second:?}");

    // The control: a follower of a repository with no log has nothing
    // to read, and the poisoned snapshot is all that is left.
    let mut c = Rig::with_store(store.clone(), "c").await;
    assert!(
        super::follow::warm(&mut c.sc).await.is_err(),
        "without a position there is only the snapshot, and it is poisoned"
    );
}

/// S3 answers a contended conditional write with 409
/// `ConditionalRequestConflict`, and that answer is INDETERMINATE: it
/// does not say whether the write landed. A lost response to a PUT that
/// did land is indistinguishable from it.
///
/// `run_batch`'s contract turns an `Err` into `ng` for every push in the
/// batch, so guessing "it failed" tells as many as `batch_max` clients
/// their push was refused while the snapshot naming it is already
/// durable. `fold::commit` has always re-read to settle this; the batch
/// path did not.
///
/// The oracle is the CLIENT'S REPORT, not the bucket: the bucket is
/// durable either way, and that is exactly what makes the wrong answer
/// invisible without this test.
#[tokio::test]
async fn a_cas_that_lands_and_then_reports_a_conflict_does_not_refuse_a_durable_push() {
    let store = Arc::new(MemoryStore::new());
    let mut rig = Rig::with_store(store.clone(), "a").await;
    rig.start().await;
    let c = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;

    // The object IS stored; only the answer is lost. Narrowed to the
    // snapshot, because a batch uploads its packs first and an
    // untargeted injection would never reach the write under test.
    store.inject_put_lands_then_fails("snapshot", 1);

    let reports = rig
        .run(vec![push(1, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: zero(),
            new_oid: c.clone(),
        }])])
        .await;
    assert!(
        is_ok(&reports[0].results[0]),
        "the push is durable, so it must not be told it failed: {:?}",
        reports[0].results[0]
    );

    // And it really is durable — the half that was never in doubt.
    let cell = snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await.expect("snapshot");
    assert_eq!(
        cell.snap.refs.get("refs/heads/main").map(String::as_str),
        Some(c.as_str()),
        "the CAS landed: {:?}",
        cell.snap.refs
    );

    // A cold restore agrees, so the adopted cell is not a local fiction.
    let mut cold = Rig::with_store(store.clone(), "cold").await;
    restore::restore(&mut cold.sc).await.expect("a cold restore proves the repository");
    assert_eq!(
        cold.sc.git.ref_oid("refs/heads/main").await.unwrap().as_deref(),
        Some(c.as_str())
    );
}

/// The quiet window (`Reach::TailOnly`). A follower that already has a
/// position keeps chasing the log through the 60 s before a takeover;
/// one that has none declines the snapshot and stays where it is.
///
/// Both halves matter and they pull against each other. The first is
/// the point of the change — `server.rs` used to stop warming the
/// moment a poll came back quiet, so a follower deliberately went cold
/// over exactly the window that decides its own takeover. The second is
/// the reason the old guard existed: a COLD follower must not start
/// pulling a whole repository seconds before it claims.
///
/// The oracle for the first is a POISONED snapshot: a pass that
/// converges cannot have read it. The oracle for the second is a
/// PERFECTLY READABLE one that the pass must decline anyway — the
/// interesting direction, because a tail pass that quietly fell back
/// would look identical to a working one on every other assertion.
#[tokio::test]
async fn the_quiet_window_keeps_a_warm_follower_chasing_and_leaves_a_cold_one_alone() {
    let store = Arc::new(MemoryStore::new());
    let mut a = Rig::with_store(store.clone(), "a").await;
    a.start().await;
    let c1 = a.push_commit("refs/heads/main", None, "one").await;

    // A follower with a position, taken while the token was moving.
    let mut b = Rig::with_store(store.clone(), "b").await;
    super::follow::warm(&mut b.sc).await.expect("the first pass takes a position");
    assert_eq!(b.sc.git.ref_oid("refs/heads/main").await.unwrap().as_deref(), Some(c1.as_str()));

    // Half two first, while the snapshot is still READABLE — declining
    // a snapshot that could not be read would prove nothing.
    let mut cold = Rig::with_store(store.clone(), "cold").await;
    assert!(
        snapshot::load(store.as_ref(), &a.sc.cfg).await.is_ok(),
        "the snapshot must be readable, or declining it proves nothing"
    );
    let declined = super::follow::warm_tail(&mut cold.sc).await.expect("a tail pass never fails");
    assert_eq!(declined.files_fetched, 0, "it fetched nothing: {declined:?}");
    assert_eq!(
        cold.sc.git.ref_oid("refs/heads/main").await.unwrap(),
        None,
        "a cold follower stays cold in the quiet window; the restore after the claim carries it"
    );

    // Half one. The holder pushes twice more and then dies; its
    // snapshot is poisoned so that reading it is loud rather than
    // silent.
    let c2 = a.push_commit("refs/heads/main", Some(&c1), "two").await;
    let c3 = a.push_commit("refs/heads/main", Some(&c2), "three").await;
    store.raw_put(&a.sc.cfg.snapshot_key(), bytes::Bytes::from_static(b"not json"), vec![]);
    assert!(
        snapshot::load(store.as_ref(), &a.sc.cfg).await.is_err(),
        "the poison must actually poison"
    );

    // The quiet window: the warm follower keeps up, on entries alone.
    let tail = super::follow::warm_tail(&mut b.sc).await.expect("the tail pass carries it");
    assert_eq!(tail.entries, Some(2), "two batches, two entries: {tail:?}");
    assert_eq!(
        b.sc.git.ref_oid("refs/heads/main").await.unwrap().as_deref(),
        Some(c3.as_str()),
        "and it arrives at its own takeover holding the holder's tip"
    );
}

/// A hole in the log is not a wrong answer, it is a slow one: the
/// follower stops at the gap and the timed resync reads the snapshot.
#[tokio::test]
async fn a_gap_in_the_log_stops_the_follower_and_the_resync_carries_it() {
    let store = Arc::new(MemoryStore::new());
    let mut a = Rig::with_store(store.clone(), "a").await;
    a.start().await;
    let c1 = a.push_commit("refs/heads/main", None, "one").await;
    let mut b = Rig::with_store(store.clone(), "b").await;
    super::follow::warm(&mut b.sc).await.expect("first warm");

    let c2 = a.push_commit("refs/heads/main", Some(&c1), "two").await;
    let c3 = a.push_commit("refs/heads/main", Some(&c2), "three").await;
    // Take the FIRST of the two entries the follower needs: the crash
    // window between a CAS and its put, or an entry the pruner reached.
    let seqs = super::log::seqs(store.as_ref(), &b.sc.cfg).await.expect("seqs");
    let hole = seqs[seqs.len() - 2];
    store.delete(&b.sc.cfg.log_key(hole)).await.expect("delete");

    let stalled = super::follow::warm(&mut b.sc).await.expect("warm");
    assert_eq!(stalled.entries, Some(0), "the chain is broken at the hole: {stalled:?}");
    assert_eq!(
        b.sc.git.ref_oid("refs/heads/main").await.unwrap().as_deref(),
        Some(c1.as_str()),
        "so the follower has not moved"
    );

    // The resync timer is what stops that from being permanent.
    b.sc.cfg.prewarm_resync_secs = 0;
    let caught = super::follow::warm(&mut b.sc).await.expect("warm");
    assert_eq!(caught.entries, None, "it read the snapshot: {caught:?}");
    assert_eq!(b.sc.git.ref_oid("refs/heads/main").await.unwrap().as_deref(), Some(c3.as_str()));
}

/// X14's cheap half, end to end: a challenger that warmed while it
/// waited claims without fetching a byte and without a full `fsck`.
///
/// The control is the same takeover with no warm pass, which must pay
/// both — otherwise the measurement is of a repository small enough for
/// the difference not to exist.
#[tokio::test]
async fn a_prewarmed_challenger_claims_without_the_bytes_or_the_full_proof() {
    async fn takeover(warm_first: bool) -> restore::RestoreReport {
        let store = Arc::new(MemoryStore::new());
        let mut a = Rig::with_store(store.clone(), "a").await;
        a.start().await;
        let mut parent = None;
        for i in 0..4 {
            parent =
                Some(a.push_commit("refs/heads/main", parent.as_deref(), &format!("c{i}")).await);
        }

        let mut b = Rig::with_store(store.clone(), "b").await;
        if warm_first {
            // What the claim loop does while the holder's token still
            // moves.
            let r = super::follow::warm(&mut b.sc).await.expect("warm");
            assert!(r.files_fetched > 0, "the warm pass is what pays the bytes: {r:?}");
        }
        lease::release(&mut a.sc).await.expect("release");
        for _ in 0..16 {
            if matches!(
                lease::claim_step(&mut b.sc).await.expect("claim"),
                lease::ClaimOutcome::Claimed(_)
            ) {
                break;
            }
        }
        assert!(b.sc.lease().is_ok(), "the successor must hold the lease");
        restore::restore(&mut b.sc).await.expect("restore")
    }

    let cold = takeover(false).await;
    let warm = takeover(true).await;
    assert!(cold.files_fetched > 0, "the control fetches the repository: {cold:?}");
    assert_eq!(cold.proof, Some(super::follow::Proof::Full), "and proves all of it: {cold:?}");
    assert_eq!(warm.files_fetched, 0, "the warmed successor fetches nothing: {warm:?}");
    assert_eq!(warm.bytes_fetched, 0);
    assert!(!warm.proof.unwrap().is_full(), "and walks no more than the delta: {warm:?}");
    assert_eq!(cold.seq, warm.seq, "both arms end at the same snapshot");
}

/// The warm restart: a container that dies and comes back on the same
/// `emptyDir` has already proved what it holds.
#[tokio::test]
async fn a_second_restore_on_the_same_disk_proves_only_what_moved() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c1 = rig.push_commit("refs/heads/main", None, "one").await;
    super::follow::checkpoint(&rig.sc, super::now_unix()).expect("checkpoint");
    let c2 = rig.push_commit("refs/heads/main", Some(&c1), "two").await;

    // The restart: the cell is dropped, the disk is not.
    rig.sc.cell = None;
    let again = restore::restore(&mut rig.sc).await.expect("restore");
    assert_eq!(again.files_fetched, 0, "nothing to fetch: {again:?}");
    assert_eq!(
        again.proof,
        Some(super::follow::Proof::Delta { new: 1 }),
        "one tip moved since the checkpoint: {again:?}"
    );
    assert_eq!(rig.sc.git.ref_oid("refs/heads/main").await.unwrap().as_deref(), Some(c2.as_str()));

    // The control: with no record of a proof, the same restore walks
    // everything.
    super::follow::forget(&rig.sc);
    rig.sc.cell = None;
    let cold = restore::restore(&mut rig.sc).await.expect("restore");
    assert_eq!(cold.proof, Some(super::follow::Proof::Full), "{cold:?}");
}

/// What the incremental proof rests on: the snapshot's PACK LIST, not
/// the files on disk.
///
/// This test used to assert the opposite, and its comment defended it:
/// a fold rewrites the pack list, the first draft expected that to cost
/// a full proof, "it does not, and the code was right — the fold
/// RETAINS its inputs on disk for readers, so every object the last
/// proof walked is still exactly where it was walked". Every clause of
/// that is true and the conclusion is wrong. A proof is only worth
/// taking as a statement about the BUCKET; the retained files are on
/// their way out of it, so resting on them left the roll-up — the one
/// object nothing had verified — inside a `Proof::Nothing`. The first
/// draft was right, and this test is why the defect survived the audit
/// that went looking for it (F2).
#[tokio::test]
async fn a_fold_costs_a_full_proof_because_what_is_proved_is_the_bucket() {
    let mut rig = Rig::new().await;
    rig.start().await;
    rig.tiers_only();
    let mut parent = None;
    for i in 0..3 {
        parent = Some(rig.push_commit("refs/heads/main", parent.as_deref(), &format!("c{i}")).await);
    }
    super::follow::checkpoint(&rig.sc, super::now_unix()).expect("checkpoint");
    rig.fold_once().await.expect("a fold");

    rig.sc.cell = None;
    let held = restore::restore(&mut rig.sc).await.expect("restore");
    assert_eq!(
        held.proof,
        Some(super::follow::Proof::Full),
        "the fold unnamed the packs that carried the last proof, so it is spent — \
         retention keeping the files on disk is not the question: {held:?}"
    );
    // The retained inputs really are still there: without this the leg
    // above could be passing because the files went, which is the OLD
    // rule, and the test would once again assert nothing.
    let on_disk = rig.sc.git.local_packs().expect("packs");
    assert!(
        !rig.sc.retained.is_empty()
            && rig.sc.retained.iter().all(|r| on_disk.contains(&r.name)),
        "the fold's inputs must still be on disk for this leg to be about naming: {:?} vs {on_disk:?}",
        rig.sc.retained
    );

    // The control: the proof taken AFTER the fold covers the snapshot
    // the fold left, so the next restart is cheap again. Without this
    // the rule above would be indistinguishable from "always full".
    rig.sc.cell = None;
    let next = restore::restore(&mut rig.sc).await.expect("restore");
    assert_eq!(
        next.proof,
        Some(super::follow::Proof::Nothing),
        "nothing moved since the post-fold proof: {next:?}"
    );

    // And retention ending changes nothing either way — the files were
    // never what the proof rested on.
    let n = fold::unlink_retained(&mut rig.sc, super::now_unix() + 86_400).expect("unlink");
    assert!(n > 0, "the fold's inputs must actually be dropped for this leg to mean anything");
    rig.sc.cell = None;
    let after = restore::restore(&mut rig.sc).await.expect("restore");
    assert_eq!(after.proof, Some(super::follow::Proof::Nothing), "{after:?}");
}

/// F2, the audit's open defect: a proof must refuse a snapshot that
/// names less than its refs need, even when the missing objects are
/// sitting on disk in a pack the snapshot no longer names.
///
/// This is the shape a bad fold leaves behind, and the shape the whole
/// retention window has: the roll-up is named, the inputs are not, and
/// the inputs are still on disk for 900 s. `fsck` over the object
/// directory passes for that entire window; the ledger sweep then
/// deletes the inputs from the bucket and the next restart cannot
/// restore. runcd was one sweep away from it. The pack list is the
/// bucket's set, so proving over it is what makes the answer mean what
/// the caller reads it as.
#[tokio::test]
async fn a_proof_refuses_what_the_snapshot_does_not_name_though_the_objects_are_on_disk() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c1 = rig.push_commit("refs/heads/main", None, "one").await;
    let before: std::collections::BTreeSet<String> =
        rig.sc.git.local_packs().expect("packs").into_iter().collect();
    let c2 = rig.push_commit("refs/heads/main", Some(&c1), "two").await;
    let named = rig.sc.git.local_packs().expect("packs");
    let tip_pack: Vec<String> = named.iter().filter(|p| !before.contains(*p)).cloned().collect();
    assert_eq!(tip_pack.len(), 1, "the second push must land exactly one pack: {named:?}");
    let refs = rig.sc.cell().expect("cell").snap.refs.clone();
    assert_eq!(refs.get("refs/heads/main").map(String::as_str), Some(c2.as_str()));

    // The positive control: over everything the snapshot names, the
    // same walk passes. Without it the refusal below could be a walk
    // that fails for any reason at all.
    super::follow::forget(&rig.sc);
    assert_eq!(
        super::follow::prove(&rig.sc, &refs, &named).await.expect("the named set holds the tip"),
        super::follow::Proof::Full
    );

    // F2: the tip's pack leaves the snapshot and stays on disk —
    // exactly what a fold's retention does to its inputs.
    let short: Vec<String> = named.iter().filter(|p| **p != tip_pack[0]).cloned().collect();
    super::follow::forget(&rig.sc);
    let err = super::follow::prove(&rig.sc, &refs, &short)
        .await
        .expect_err("a snapshot that cannot reach its own tip must be refused");
    assert!(matches!(err, ForgeError::Refused(_)), "{err:?}");

    // The file is still right there. That is the point: the proof
    // refused on what the bucket holds, not on what the disk holds, and
    // an unscoped `fsck` passes on this very state.
    assert!(
        rig.sc.git.local_packs().expect("packs").contains(&tip_pack[0]),
        "the leg is vacuous unless the objects are still on disk"
    );
    rig.sc.git.fsck_connectivity_all().await.expect("the disk is coherent — and that is not the question");
}

/// The incremental proof is a proof: an object that is not there fails
/// it. Without this leg every "Delta" above could be a walk of nothing.
#[tokio::test]
async fn the_incremental_proof_refuses_a_tip_it_cannot_walk() {
    let mut rig = Rig::new().await;
    rig.start().await;
    let c1 = rig.push_commit("refs/heads/main", None, "one").await;

    let packs = rig.sc.git.local_packs().expect("packs");

    // The positive control: the tip this repository holds walks.
    rig.sc
        .git
        .prove_reachable_over(&packs, std::slice::from_ref(&c1), &[])
        .await
        .expect("a present tip walks");

    // A tip nothing in the repository reaches.
    let absent = "1".repeat(40);
    let err =
        rig.sc.git.prove_reachable_over(&packs, &[absent], &[c1]).await.expect_err("must refuse");
    assert!(matches!(err, ForgeError::Refused(_)), "{err:?}");
}

/// THE REQUEST BUDGET — what each operation costs the object store, as
/// an assertion rather than as a sentence in a design note.
///
/// Every number below is a round-trip a real deployment pays on every
/// occurrence, and this project has a documented history of budgets
/// that lived only in prose or in a drill log and drifted without
/// anyone noticing. walgit pins its equivalent with a test
/// (`ROUNDTRIPS.md` + an assertion); this is forge's.
///
///   operation            | puts | get_whole | get_range | head | list
///   ---------------------|------|-----------|-----------|------|-----
///   push, first          |   8  |     -     |     -     |   -  |  -
///   push, steady state   |   5  |     -     |     -     |   -  |  -
///   warm follower, idle  |   -  |     1     |     -     |   -  |  -
///   warm follower, +1    |   -  |     2     |     3     |   3  |  -
///   warm follower, cold  |   -  |     1     |     6     |   6  |  1
///   restore, cold        |   -  |     2     |     9     |   -  |  1
///
/// The one that matters most is **warm follower, idle: ONE get_whole**.
/// `log.rs` claims "an idle poll is one 404 on `<seq+1>` instead of a
/// whole snapshot" — that was documentation, and it is now checked. A
/// follower polls per heartbeat forever, so a regression here is paid
/// by every idle repository in the fleet, continuously, and would show
/// up in a bill long before it showed up in a test.
///
/// IF YOU CHANGE A NUMBER HERE, change the table with it and say in the
/// commit message which round trip you added and why. A budget nobody
/// has to argue with is not a budget.
///
/// `epoch_renew` is deliberately NOT pinned exactly: the renewer runs
/// on a timer (design §5 — a quiet repository must keep renewing), so
/// its count is a function of wall clock, not of the operation. The
/// batch's own renew is asserted as a floor.
#[tokio::test]
async fn the_request_budget_per_operation_is_pinned() {
    let store = Arc::new(MemoryStore::new());
    let mut rig = Rig::with_store(store.clone(), "a").await;
    rig.start().await;
    let n = |m: &std::collections::BTreeMap<&'static str, u64>, k: &str| -> u64 {
        m.get(k).copied().unwrap_or(0)
    };

    // ── a push into an empty repository ──────────────────────────────
    let c = rig.stage_commit(None, &[("a.txt", "one\n")], "first").await;
    store.reset_op_counts();
    let r = rig
        .run(vec![push(1, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: zero(),
            new_oid: c.clone(),
        }])])
        .await;
    assert!(is_ok(&r[0].results[0]));
    let m = store.op_counts();
    assert_eq!(n(&m, "put_whole"), 8, "push, first: {m:?}");
    assert!(n(&m, "epoch_renew") >= 1, "the batch renews before it commits: {m:?}");

    // ── a push into a repository that already has one ────────────────
    let c2 = rig.stage_commit(Some(&c), &[("b.txt", "two\n")], "second").await;
    store.reset_op_counts();
    let r = rig
        .run(vec![push(2, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: c.clone(),
            new_oid: c2.clone(),
        }])])
        .await;
    assert!(is_ok(&r[0].results[0]));
    let m = store.op_counts();
    assert_eq!(n(&m, "put_whole"), 5, "push, steady state: {m:?}");

    // ── a follower's first pass: the snapshot, and the files it names ─
    let mut b = Rig::with_store(store.clone(), "b").await;
    store.reset_op_counts();
    super::follow::warm(&mut b.sc).await.expect("cold warm");
    let m = store.op_counts();
    assert_eq!((n(&m, "get_whole"), n(&m, "get_range"), n(&m, "head"), n(&m, "list")),
               (1, 6, 6, 1), "warm follower, cold: {m:?}");

    // ── the idle poll. THE number: one GET that 404s. ────────────────
    store.reset_op_counts();
    super::follow::warm(&mut b.sc).await.expect("idle warm");
    let m = store.op_counts();
    assert_eq!(n(&m, "get_whole"), 1, "warm follower, idle: {m:?}");
    assert_eq!(n(&m, "get_range") + n(&m, "head") + n(&m, "list"), 0,
               "an idle poll reads NOTHING else — not the snapshot, not a listing: {m:?}");

    // ── one entry behind ─────────────────────────────────────────────
    let c3 = rig.stage_commit(Some(&c2), &[("c.txt", "three\n")], "third").await;
    let r = rig
        .run(vec![push(3, vec![RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: c2.clone(),
            new_oid: c3.clone(),
        }])])
        .await;
    assert!(is_ok(&r[0].results[0]));
    store.reset_op_counts();
    super::follow::warm(&mut b.sc).await.expect("one-entry warm");
    let m = store.op_counts();
    assert_eq!((n(&m, "get_whole"), n(&m, "get_range"), n(&m, "head")), (2, 3, 3),
               "warm follower, one entry behind: {m:?}");

    // ── a cold restore ───────────────────────────────────────────────
    let mut cold = Rig::with_store(store.clone(), "cold").await;
    store.reset_op_counts();
    restore::restore(&mut cold.sc).await.expect("restore");
    let m = store.op_counts();
    assert_eq!((n(&m, "get_whole"), n(&m, "get_range"), n(&m, "list")), (2, 9, 1),
               "restore, cold: {m:?}");
}

/// `git push --atomic` promises the client that every command lands or
/// none does. forge judges PER COMMAND by design, and
/// `receive.procReceiveRefs = refs/` puts every ref through
/// proc-receive — which is exactly the set git EXCLUDES from its own
/// atomic ref transaction. So git does not keep this contract for
/// forge, and until now neither did forge: the capability was read off
/// the wire and dropped, and a two-ref atomic push with one bad ref
/// landed the good one. Durably, into the snapshot.
///
/// The control is the SAME pair without `--atomic`, which must still
/// land the good ref. Without it, "neither landed" could mean the pair
/// was unpushable for some unrelated reason and the guarantee was never
/// exercised — which is the mistake the gitqual leg made.
#[tokio::test]
async fn an_atomic_push_with_one_bad_ref_lands_neither_and_the_control_lands_one() {
    let store = Arc::new(MemoryStore::new());
    let mut rig = Rig::with_store(store.clone(), "a").await;
    rig.start().await;

    // A ref that exists, so a stale old_oid against it is refused by
    // the SERVER — not by the client, which is not in this test at all.
    let base = rig.push_commit("refs/heads/main", None, "seed").await;
    let good = rig.stage_commit(Some(&base), &[("g.txt", "good\n")], "good").await;

    // ── the control: no --atomic, so the good half lands ─────────────
    let r = rig
        .run(vec![push(1, vec![
            RefUpdate { name: "refs/heads/ctl".into(), old_oid: zero(), new_oid: good.clone() },
            RefUpdate { name: "refs/heads/main".into(), old_oid: zero(), new_oid: good.clone() },
        ])])
        .await;
    assert!(is_ok(&r[0].results[0]), "the good ref lands without --atomic: {:?}", r[0].results[0]);
    assert!(!is_ok(&r[0].results[1]), "and the stale one is refused: {:?}", r[0].results[1]);
    assert_eq!(
        rig.sc.git.ref_oid("refs/heads/ctl").await.unwrap().as_deref(),
        Some(good.as_str()),
        "the control must actually land something, or the test below proves nothing"
    );

    // ── the contract: --atomic, and NEITHER may land ─────────────────
    let r = rig
        .run(vec![atomic_push(2, vec![
            RefUpdate { name: "refs/heads/at".into(), old_oid: zero(), new_oid: good.clone() },
            RefUpdate { name: "refs/heads/main".into(), old_oid: zero(), new_oid: good.clone() },
        ])])
        .await;
    assert!(
        r[0].results.iter().all(|x| !is_ok(x)),
        "one refused command refuses the whole atomic push: {:?}",
        r[0].results
    );
    assert_eq!(
        rig.sc.git.ref_oid("refs/heads/at").await.unwrap(),
        None,
        "the good ref of an atomic push must NOT be on disk"
    );
    let cell = snapshot::load(rig.store.as_ref(), &rig.sc.cfg).await.expect("snapshot");
    assert!(
        !cell.snap.refs.contains_key("refs/heads/at"),
        "and it must NOT be in the snapshot — the durable half is the one that matters: {:?}",
        cell.snap.refs
    );
}

// ── The file API's plumbing (docs/plans/forge-file-api-design.md §4.2) ──
//
// Every test here pins a behaviour that was MEASURED against real git
// during the design, not one that was assumed. Where the design names
// a mutation that must fail, the test runs it.

/// A bare repository with nothing in it, for the plumbing tests.
async fn bare_repo() -> (tempfile::TempDir, super::gitcmd::Git) {
    let dir = tempfile::tempdir().expect("tempdir");
    let git = super::gitcmd::Git::new(dir.path().join("repo.git"));
    git.init_bare("main", None).await.expect("init");
    (dir, git)
}

/// The bytes runner exists because the lossy one destroys content, and
/// this is the control that shows it: the SAME blob through
/// `cat_blob` is byte-identical and through the lossy path is not.
///
/// Without the second half this test would pass against a runner that
/// happened to be lossless for the bytes it was given — which is every
/// runner, for ASCII. The 256-value pattern is chosen so that the
/// lossy path MUST corrupt it.
#[tokio::test]
async fn a_blob_of_every_byte_value_round_trips_and_the_lossy_runner_would_not() {
    let (_d, git) = bare_repo().await;
    let content: Vec<u8> = (0..=255u8).cycle().take(1024).collect();

    let oid = git.hash_object(&content).await.expect("hash-object");
    let back = git.cat_blob(&oid).await.expect("cat-blob");
    assert_eq!(back, content, "cat_blob must return the bytes that went in");

    // The control: the same read through the lossy runner. If this ever
    // stops differing, `run_bytes` has stopped being necessary and this
    // test has stopped testing anything.
    let lossy = git
        .must(&["cat-file", "blob", &oid], None)
        .await
        .expect("cat-file via the lossy runner");
    assert_ne!(
        lossy.as_bytes(),
        content.as_slice(),
        "the lossy runner must still corrupt this, or the bytes runner is not load-bearing"
    );
}

/// An unborn branch has no tree to read, and `read-tree HEAD` on a
/// commitless repository is a fatal error — so `build_tree` uses git's
/// intrinsic empty tree and the first write takes the ordinary path.
/// The nested path also pins that `write-tree` invents the intermediate
/// trees: `a/` and `a/b/` are never named.
#[tokio::test]
async fn the_first_write_needs_no_parent_and_invents_the_intermediate_trees() {
    let (_d, git) = bare_repo().await;
    let oid = git.hash_object(b"hello\n").await.expect("blob");

    let tree = git
        .build_tree(
            None,
            &[super::gitcmd::IndexEdit::Set {
                path: "a/b/c.txt".into(),
                mode: "100644".into(),
                oid: oid.clone(),
            }],
        )
        .await
        .expect("build_tree on an unborn branch");

    let entries = git.ls_tree(&tree, "", true).await.expect("ls-tree");
    assert_eq!(entries.len(), 1, "one blob, got {entries:?}");
    assert_eq!(entries[0].path, "a/b/c.txt");
    assert_eq!(entries[0].oid, oid);
    assert_eq!(entries[0].size, Some(6));
}

/// The mode is an INPUT, never an inheritance — measured: naming the
/// wrong one silently demotes an executable. So the API must read the
/// existing mode and pass it back, and this pins both halves: the
/// entry the caller names takes the mode the caller gives, and every
/// OTHER entry keeps its own.
#[tokio::test]
async fn a_write_takes_the_mode_it_is_given_and_leaves_the_others_alone() {
    let (_d, git) = bare_repo().await;
    let script = git.hash_object(b"#!/bin/sh\n").await.expect("blob");
    let plain = git.hash_object(b"one\n").await.expect("blob");
    use super::gitcmd::IndexEdit;

    let base = git
        .build_tree(
            None,
            &[
                IndexEdit::Set { path: "run.sh".into(), mode: "100755".into(), oid: script },
                IndexEdit::Set { path: "a.txt".into(), mode: "100644".into(), oid: plain },
            ],
        )
        .await
        .expect("base");

    // Rewrite only a.txt; run.sh must keep 100755.
    let two = git.hash_object(b"two\n").await.expect("blob");
    let next = git
        .build_tree(
            Some(&base),
            &[IndexEdit::Set { path: "a.txt".into(), mode: "100644".into(), oid: two.clone() }],
        )
        .await
        .expect("update");

    let entries = git.ls_tree(&next, "", true).await.expect("ls-tree");
    let run = entries.iter().find(|e| e.path == "run.sh").expect("run.sh survived");
    assert_eq!(run.mode, "100755", "an untouched entry keeps its mode: {entries:?}");
    let a = entries.iter().find(|e| e.path == "a.txt").expect("a.txt");
    assert_eq!(a.oid, two, "the touched entry took the new content");

    // And the demotion the design warns about is real: name 100644 for
    // the executable and it becomes one.
    let demoted = git
        .build_tree(
            Some(&base),
            &[IndexEdit::Set { path: "run.sh".into(), mode: "100644".into(), oid: run.oid.clone() }],
        )
        .await
        .expect("demote");
    let e = git.ls_tree(&demoted, "", true).await.expect("ls-tree");
    let run = e.iter().find(|x| x.path == "run.sh").expect("run.sh");
    assert_eq!(run.mode, "100644", "the mode follows the caller, which is why it must be read first");
}

/// git's own `verify_path` is the reason the design chose a scratch
/// index over `mktree` — `mktree` accepts an entry named `.git`, and
/// forge's restore proof (`fsck --connectivity-only`) does not flag it.
///
/// The second half is the trap that makes this test worth more than
/// its first half: after a refused `--index-info`, `write-tree` exits 0
/// and returns the UNCHANGED tree. A `build_tree` that did not check
/// the status would answer with a valid oid and silently commit
/// nothing.
#[tokio::test]
async fn a_refused_path_is_an_error_and_never_the_unchanged_tree() {
    let (_d, git) = bare_repo().await;
    let blob = git.hash_object(b"x\n").await.expect("blob");
    use super::gitcmd::IndexEdit;

    let base = git
        .build_tree(
            None,
            &[IndexEdit::Set { path: "keep.txt".into(), mode: "100644".into(), oid: blob.clone() }],
        )
        .await
        .expect("base");

    for bad in [".git/config", ".GIT/hooks/pre-commit", "a/../b.txt", "/abs.txt", "./x.txt"] {
        let r = git
            .build_tree(
                Some(&base),
                &[IndexEdit::Set { path: bad.into(), mode: "100644".into(), oid: blob.clone() }],
            )
            .await;
        match r {
            Err(super::ForgeError::Refused(_)) => {}
            Err(other) => panic!("{bad}: wrong error kind: {other:?}"),
            Ok(tree) => panic!(
                "{bad}: accepted, tree {tree} — and note it equals the base ({}), which is \
                 exactly how an unchecked write-tree reports success having done nothing",
                tree == base
            ),
        }
    }
}

/// `--remove` and `--force-remove` are both "this operation must be run
/// in a work tree" in a bare repository — measured. Mode 0 through
/// `--index-info` is the only spelling that works, and a rename is a
/// Remove and a Set in ONE call, which is what makes it atomic.
#[tokio::test]
async fn delete_and_rename_go_through_index_info_and_rename_keeps_the_mode() {
    let (_d, git) = bare_repo().await;
    use super::gitcmd::IndexEdit;
    let script = git.hash_object(b"#!/bin/sh\n").await.expect("blob");
    let doomed = git.hash_object(b"bye\n").await.expect("blob");

    let base = git
        .build_tree(
            None,
            &[
                IndexEdit::Set { path: "bin/run.sh".into(), mode: "100755".into(), oid: script.clone() },
                IndexEdit::Set { path: "README.md".into(), mode: "100644".into(), oid: doomed },
            ],
        )
        .await
        .expect("base");

    let next = git
        .build_tree(
            Some(&base),
            &[
                IndexEdit::Remove { path: "README.md".into() },
                IndexEdit::Remove { path: "bin/run.sh".into() },
                IndexEdit::Set { path: "tools/run.sh".into(), mode: "100755".into(), oid: script },
            ],
        )
        .await
        .expect("delete + rename");

    let entries = git.ls_tree(&next, "", true).await.expect("ls-tree");
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, vec!["tools/run.sh"], "one entry left, got {entries:?}");
    assert_eq!(entries[0].mode, "100755", "a rename carries the mode the caller supplies");
    // The emptied directory is pruned by write-tree — the thing an
    // mktree implementation would have had to do by hand.
    let top = git.ls_tree(&next, "", false).await.expect("ls-tree root");
    let names: Vec<&str> = top.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(names, vec!["tools"], "the emptied `bin/` is gone, not an empty tree: {top:?}");
}

/// The classifier the API answers with. A symlink's content IS its
/// target and a gitlink's commit is not in this repository at all, so
/// neither may be served as file bytes.
#[tokio::test]
async fn ls_tree_classifies_every_entry_kind() {
    let (_d, git) = bare_repo().await;
    use super::gitcmd::IndexEdit;
    let plain = git.hash_object(b"plain\n").await.expect("blob");
    let target = git.hash_object(b"dir/target.txt").await.expect("blob");

    let tree = git
        .build_tree(
            None,
            &[
                IndexEdit::Set { path: "plain.txt".into(), mode: "100644".into(), oid: plain.clone() },
                IndexEdit::Set { path: "run.sh".into(), mode: "100755".into(), oid: plain.clone() },
                IndexEdit::Set { path: "link".into(), mode: "120000".into(), oid: target },
                IndexEdit::Set {
                    path: "vendor/mod".into(),
                    mode: "160000".into(),
                    // A gitlink may name a commit this repository does
                    // not hold; git accepts it and fsck --connectivity-only
                    // passes, which is why the API must refuse it by KIND
                    // rather than by trying to read it.
                    oid: "0123456789012345678901234567890123456789".into(),
                },
                IndexEdit::Set { path: "d/nested.txt".into(), mode: "100644".into(), oid: plain },
            ],
        )
        .await
        .expect("tree");

    let top = git.ls_tree(&tree, "", false).await.expect("ls-tree");
    let kind = |p: &str| {
        top.iter().find(|e| e.path == p).unwrap_or_else(|| panic!("{p} missing from {top:?}")).kind_name()
    };
    assert_eq!(kind("plain.txt"), "file");
    assert_eq!(kind("run.sh"), "executable");
    assert_eq!(kind("link"), "symlink");
    assert_eq!(kind("d"), "directory");

    let deep = git.ls_tree(&tree, "", true).await.expect("ls-tree -r");
    let sub = deep.iter().find(|e| e.path == "vendor/mod").expect("gitlink");
    assert_eq!(sub.kind_name(), "submodule");
    assert_eq!(sub.kind, "commit", "a gitlink reports type `commit`");
    assert!(sub.size.is_none(), "a gitlink has no size");
    assert!(!sub.is_regular_file(), "a submodule is never servable as file bytes");
}

/// A blob's size comes from `ls-tree` WITHOUT reading the blob. This is
/// the whole basis of the design's decision not to stream (§2.2): the
/// cap can be enforced before a single byte is allocated.
#[tokio::test]
async fn a_blobs_size_is_known_before_it_is_read() {
    let (_d, git) = bare_repo().await;
    let big = vec![b'z'; 3 * 1024 * 1024];
    let oid = git.hash_object(&big).await.expect("blob");
    let tree = git
        .build_tree(
            None,
            &[super::gitcmd::IndexEdit::Set {
                path: "big.bin".into(),
                mode: "100644".into(),
                oid,
            }],
        )
        .await
        .expect("tree");

    let e = git.tree_entry(&tree, "big.bin").await.expect("entry").expect("present");
    assert_eq!(e.size, Some(3 * 1024 * 1024), "the size is in the tree listing");
    assert!(e.is_regular_file());

    // And a path that is not there is None, not an error and not an
    // empty file — `ls-tree` exits 0 with empty output for a missing
    // path, which is a silence a caller must not read as success.
    assert!(git.tree_entry(&tree, "nosuch.txt").await.expect("query").is_none());
}

/// Concurrent builds must not see each other. A shared index gives
/// either a hard `index.lock` failure or a phantom write in which every
/// racer commits everyone's edits — both measured — so the index is per
/// call, and this runs enough of them at once to catch a regression to
/// a shared one.
#[tokio::test]
async fn concurrent_builds_on_one_repository_do_not_interfere() {
    let (_d, git) = bare_repo().await;
    use super::gitcmd::IndexEdit;
    let seed = git.hash_object(b"seed\n").await.expect("blob");
    let base = git
        .build_tree(
            None,
            &[IndexEdit::Set { path: "seed.txt".into(), mode: "100644".into(), oid: seed }],
        )
        .await
        .expect("base");

    let mut tasks = Vec::new();
    for i in 0..8 {
        let g = git.clone();
        let b = base.clone();
        tasks.push(tokio::spawn(async move {
            let oid = g.hash_object(format!("body {i}\n").as_bytes()).await.expect("blob");
            let tree = g
                .build_tree(
                    Some(&b),
                    &[IndexEdit::Set {
                        path: format!("f{i}.txt"),
                        mode: "100644".into(),
                        oid,
                    }],
                )
                .await
                .expect("build");
            g.ls_tree(&tree, "", true).await.expect("ls-tree")
        }));
    }

    for (i, t) in tasks.into_iter().enumerate() {
        let entries = t.await.expect("join");
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![format!("f{i}.txt").as_str(), "seed.txt"],
            "build {i} must see its own edit and the base only — a shared index would \
             show every racer's file here"
        );
    }
}

/// A file written over a directory REPLACES it, silently — exit 0, no
/// stderr, the whole subtree gone. `--cacheinfo` refuses this and
/// `--index-info` does not, so nothing in `build_tree` catches it and
/// the guard has to live where the target's kind is known.
///
/// This test does not assert that forge is safe. It asserts that the
/// primitive is DANGEROUS, so that the day someone writes a second
/// caller for `build_tree` there is a test standing between them and a
/// silent recursive delete.
#[tokio::test]
async fn build_tree_replaces_a_directory_with_a_file_and_says_nothing() {
    let (_d, git) = bare_repo().await;
    use super::gitcmd::IndexEdit;
    let blob = git.hash_object(b"x\n").await.expect("blob");

    let base = git
        .build_tree(
            None,
            &[
                IndexEdit::Set { path: "a/b/c.txt".into(), mode: "100644".into(), oid: blob.clone() },
                IndexEdit::Set { path: "a/b/d.txt".into(), mode: "100644".into(), oid: blob.clone() },
            ],
        )
        .await
        .expect("base");

    let after = git
        .build_tree(
            Some(&base),
            &[IndexEdit::Set { path: "a/b".into(), mode: "100644".into(), oid: blob }],
        )
        .await
        .expect("git accepts this — that is the point of the test");

    let entries = git.ls_tree(&after, "", true).await.expect("ls-tree");
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["a/b"],
        "two files were destroyed by one write and git reported success: {entries:?}"
    );

    // The kind check the API layer must make, shown here as the thing
    // that would have prevented it.
    let target = git.tree_entry(&base, "a/b").await.expect("query").expect("present");
    assert_eq!(target.kind_name(), "directory", "this is the refusal the API owes the caller");
}

/// The validator refuses what git would silently drop. Kept separate
/// from the `build_tree` test so a change to either is a change to one
/// test, and so the rules are readable as a list.
#[test]
fn the_path_validator_refuses_what_git_would_ignore() {
    use super::gitcmd::validate_tree_path as v;
    for bad in [
        "", "/abs.txt", "a//b.txt", "./x.txt", "a/../b.txt", "..", ".",
        ".git/config", ".GIT/hooks/pre-commit", ".Git/x", "a/.git/hooks/pre-commit",
        ".git./config", "git~1/x", "with\nnewline", "with\0nul",
    ] {
        assert!(v(bad).is_err(), "{bad:?} must be refused");
    }
    for ok in [
        "a.txt", "a/b/c.txt", "dir/file with spaces.txt", ".gitignore",
        ".gitmodules", "a/.gitkeep", "digit9/x", "UPPER/Case.MD",
    ] {
        assert!(v(ok).is_ok(), "{ok:?} must be accepted: {:?}", v(ok));
    }
}

// ── The file API's verbs (docs/plans/forge-file-api-design.md §3, §4) ──

use super::fileapi::{self, FileError};

/// A repository with one commit and a small tree, returning the tree oid.
async fn repo_with_tree() -> (tempfile::TempDir, super::gitcmd::Git, String) {
    let (dir, git) = bare_repo().await;
    use super::gitcmd::IndexEdit;
    let plain = git.hash_object(b"one\n").await.expect("blob");
    let script = git.hash_object(b"#!/bin/sh\n").await.expect("blob");
    let target = git.hash_object(b"d/nested.txt").await.expect("blob");
    let tree = git
        .build_tree(
            None,
            &[
                IndexEdit::Set { path: "a.txt".into(), mode: "100644".into(), oid: plain.clone() },
                IndexEdit::Set { path: "run.sh".into(), mode: "100755".into(), oid: script },
                IndexEdit::Set { path: "link".into(), mode: "120000".into(), oid: target },
                IndexEdit::Set { path: "d/nested.txt".into(), mode: "100644".into(), oid: plain },
                IndexEdit::Set {
                    path: "vendor/mod".into(),
                    mode: "160000".into(),
                    oid: "0123456789012345678901234567890123456789".into(),
                },
            ],
        )
        .await
        .expect("tree");
    (dir, git, tree)
}

/// The guard for the silent recursive delete. `build_tree` would accept
/// this write and remove everything under `d/` without a word — pinned
/// by `build_tree_replaces_a_directory_with_a_file_and_says_nothing`.
/// The API refuses it, and this is the test that says so.
#[tokio::test]
async fn a_write_over_a_directory_is_refused_not_performed() {
    let (_d, git, tree) = repo_with_tree().await;
    let e = fileapi::plan_put(&git, Some(&tree), "d", b"clobber\n", Some("*"), 1 << 20)
        .await
        .expect_err("must refuse");
    assert_eq!(e, FileError::WouldReplaceDirectory);
    assert_eq!(e.status(), 409);

    // And the subtree is still there, which is the property that
    // actually matters.
    let l = fileapi::list(&git, Some(&tree), "d").await.expect("still a directory");
    assert_eq!(l.entries.len(), 1);
    assert_eq!(l.entries[0].path, "d/nested.txt");
}

/// The rule that makes many writers safe. Two people editing one file
/// in a browser must not silently overwrite each other, so an existing
/// path REQUIRES a condition and a stale one is refused.
#[tokio::test]
async fn an_existing_file_needs_if_match_and_a_stale_one_is_refused() {
    let (_d, git, tree) = repo_with_tree().await;

    let bare = fileapi::plan_put(&git, Some(&tree), "a.txt", b"two\n", None, 1 << 20)
        .await
        .expect_err("unconditioned overwrite must be refused");
    assert_eq!(bare, FileError::PreconditionRequired);
    assert_eq!(bare.status(), 428);

    let stale = fileapi::plan_put(
        &git,
        Some(&tree),
        "a.txt",
        b"two\n",
        Some("dead00000000000000000000000000000000beef"),
        1 << 20,
    )
    .await
    .expect_err("a stale condition must be refused");
    assert_eq!(stale.status(), 412);
    assert_eq!(stale.reason(), "file-changed");

    // The current etag succeeds — and quoted, as a browser sends it.
    let (cur, _) = fileapi::stat(&git, Some(&tree), "a.txt").await.expect("stat");
    let quoted = format!("\"{}\"", cur.oid);
    let plan = fileapi::plan_put(&git, Some(&tree), "a.txt", b"two\n", Some(&quoted), 1 << 20)
        .await
        .expect("the current version is accepted");
    let after = git.tree_entry(&plan.tree, "a.txt").await.expect("q").expect("present");
    assert_eq!(after.oid, plan.etag, "the plan's etag is the new content");
    assert_ne!(after.oid, cur.oid, "and it actually changed");
}

/// A write must never move the executable bit. The mode is read from
/// the existing entry, because naming the wrong one silently demotes it
/// — pinned one layer down in
/// `a_write_takes_the_mode_it_is_given_and_leaves_the_others_alone`.
#[tokio::test]
async fn editing_an_executable_keeps_it_executable() {
    let (_d, git, tree) = repo_with_tree().await;
    let (cur, _) = fileapi::stat(&git, Some(&tree), "run.sh").await.expect("stat");
    assert_eq!(cur.mode, "100755");
    let plan = fileapi::plan_put(&git, Some(&tree), "run.sh", b"#!/bin/bash\n", Some(&cur.oid), 1 << 20)
        .await
        .expect("edit");
    let after = git.tree_entry(&plan.tree, "run.sh").await.expect("q").expect("present");
    assert_eq!(after.mode, "100755", "the executable bit survived an edit through the API");
}

/// A symlink's content is its target and a submodule's commit is not in
/// this repository. Neither may be served as file bytes, and the two
/// refusals must be distinguishable — a client that cannot tell them
/// apart cannot explain either to a user.
#[tokio::test]
async fn the_kinds_that_are_not_files_are_refused_distinctly() {
    let (_d, git, tree) = repo_with_tree().await;
    let cases = [
        ("d", 409, "is-a-directory"),
        ("link", 409, "not-a-file"),
        ("vendor/mod", 409, "not-a-file"),
    ];
    for (path, status, reason) in cases {
        let e = fileapi::read(&git, Some(&tree), path, 1 << 20).await.expect_err(path);
        assert_eq!(e.status(), status, "{path}: {e:?}");
        assert_eq!(e.reason(), reason, "{path}: {e:?}");
    }
    // The two `not-a-file`s still say WHICH in the human message.
    let link = fileapi::read(&git, Some(&tree), "link", 1 << 20).await.unwrap_err();
    let sub = fileapi::read(&git, Some(&tree), "vendor/mod", 1 << 20).await.unwrap_err();
    assert!(link.message().contains("symbolic link"), "{}", link.message());
    assert!(sub.message().contains("submodule"), "{}", sub.message());
}

/// The cap is decided from the size in the TREE listing, not from the
/// bytes — which is what lets an oversized object be refused without
/// being held in memory, and is the whole basis for not streaming.
///
/// **What this pins and what it does not.** It pins that the size is
/// available from `stat`, which never calls `cat_blob`, and that `read`
/// answers `413` from it. It does NOT prove the blob was never read: a
/// control that deleted the object failed, because
/// `ls-tree --format=%(objectsize)` needs the object too — git reads
/// its header for the size, so nothing MATERIALISES the content, but
/// the object must be present. The ordering is therefore structural,
/// and `the_cap_is_checked_before_the_read` below is the mutation that
/// catches a regression.
#[tokio::test]
async fn the_size_cap_is_decided_from_the_tree_listing() {
    let (_d, git) = bare_repo().await;
    let body = vec![b'z'; 4096];
    let oid = git.hash_object(&body).await.expect("blob");
    let tree = git
        .build_tree(
            None,
            &[super::gitcmd::IndexEdit::Set {
                path: "big.bin".into(),
                mode: "100644".into(),
                oid,
            }],
        )
        .await
        .expect("tree");

    // `stat` yields the size and reads no content.
    let (raw, rendered) = fileapi::stat(&git, Some(&tree), "big.bin").await.expect("stat");
    assert_eq!(raw.size, Some(4096));
    assert_eq!(rendered.size, Some(4096));

    let e = fileapi::read(&git, Some(&tree), "big.bin", 1024).await.expect_err("over cap");
    assert_eq!(e, FileError::TooLarge { size: 4096, cap: 1024 });
    assert_eq!(e.status(), 413);

    // Under the cap the same file reads back whole.
    let (_, bytes) = fileapi::read(&git, Some(&tree), "big.bin", 1 << 20).await.expect("under cap");
    assert_eq!(bytes.len(), 4096);
}

/// The mutation for the ordering above: at exactly the cap it reads, one
/// byte over it refuses. An implementation that read first and checked
/// after would pass the first half and fail nothing — so this asserts
/// the boundary, which is the part a reordering would move.
#[tokio::test]
async fn the_cap_is_checked_before_the_read() {
    let (_d, git) = bare_repo().await;
    let oid = git.hash_object(&vec![b'q'; 100]).await.expect("blob");
    let tree = git
        .build_tree(
            None,
            &[super::gitcmd::IndexEdit::Set {
                path: "f".into(),
                mode: "100644".into(),
                oid,
            }],
        )
        .await
        .expect("tree");
    assert!(fileapi::read(&git, Some(&tree), "f", 100).await.is_ok(), "at the cap it reads");
    assert_eq!(
        fileapi::read(&git, Some(&tree), "f", 99).await.unwrap_err().status(),
        413,
        "one byte over the cap it refuses"
    );
    // And a write is bounded by the same number, from the body length.
    assert_eq!(
        fileapi::plan_put(&git, Some(&tree), "g", &vec![b'x'; 101], None, 100)
            .await
            .unwrap_err()
            .status(),
        413
    );
}

/// **The cost claim, pinned.** A directory rename in forge moves names,
/// never content: every blob keeps its oid, so nothing is copied and
/// nothing is re-uploaded. This is the operation S3 charges for by the
/// byte — a folder rename there is a COPY of every object under it —
/// and it is the clearest thing the forge backend buys a file manager.
///
/// The assertion that carries it is the oid comparison. If a future
/// implementation rebuilt blobs instead of reusing them, every other
/// assertion here would still pass and this one would not.
#[tokio::test]
async fn a_directory_rename_copies_no_content() {
    let (_d, git) = bare_repo().await;
    use super::gitcmd::IndexEdit;
    let mut edits = Vec::new();
    let mut before = std::collections::BTreeMap::new();
    for i in 0..12 {
        let oid = git.hash_object(format!("body {i}\n").as_bytes()).await.expect("blob");
        let path = format!("src/pkg{}/f{i}.txt", i % 3);
        before.insert(format!("moved/pkg{}/f{i}.txt", i % 3), oid.clone());
        edits.push(IndexEdit::Set {
            path,
            mode: if i == 0 { "100755".into() } else { "100644".into() },
            oid,
        });
    }
    let tree = git.build_tree(None, &edits).await.expect("tree");

    let plan = fileapi::plan_move(&git, Some(&tree), "src", "moved", None)
        .await
        .expect("a directory rename is allowed");

    assert!(git.tree_entry(&plan.tree, "src").await.unwrap().is_none(), "the old name is gone");
    let after = git.ls_tree(&plan.tree, "", true).await.expect("ls-tree");
    assert_eq!(after.len(), 12, "every file moved: {after:?}");

    for e in &after {
        let want = before.get(&e.path).unwrap_or_else(|| panic!("unexpected path {}", e.path));
        assert_eq!(
            &e.oid, want,
            "{} was rebuilt rather than reused — a rename must copy no content",
            e.path
        );
    }
    let exec = after.iter().find(|e| e.path.ends_with("f0.txt")).expect("f0");
    assert_eq!(exec.mode, "100755", "modes survive a directory rename");
}

/// A directory rename is bounded by COUNT, not by bytes, so the refusal
/// names the count. The bound exists because each file is one index
/// line, not because any content is moved.
#[tokio::test]
async fn a_directory_rename_is_bounded_by_entry_count() {
    let (_d, git) = bare_repo().await;
    use super::gitcmd::IndexEdit;
    let oid = git.hash_object(b"x\n").await.expect("blob");
    let edits: Vec<IndexEdit> = (0..5)
        .map(|i| IndexEdit::Set {
            path: format!("d/f{i}.txt"),
            mode: "100644".into(),
            oid: oid.clone(),
        })
        .collect();
    let tree = git.build_tree(None, &edits).await.expect("tree");

    // Under the shipped bound it succeeds; the bound itself is pinned
    // by the constant rather than by building ten thousand files.
    assert!(fileapi::plan_move(&git, Some(&tree), "d", "e", None).await.is_ok());
    assert_eq!(
        super::fileapi::MAX_RENAME_ENTRIES, 10_000,
        "the bound is part of the contract; changing it is a decision"
    );
    let e = FileError::TooManyEntries { count: 10_001, cap: 10_000 };
    assert_eq!(e.status(), 413);
    assert!(e.message().contains("10001"), "the refusal names the count: {}", e.message());
}

/// A rename is one tree edit, so it is atomic — the property lite
/// cannot offer. It must carry the mode, refuse a destination that
/// exists, and refuse a move into its own subtree.
#[tokio::test]
async fn a_rename_is_one_edit_and_refuses_the_unsafe_shapes() {
    let (_d, git, tree) = repo_with_tree().await;
    let (src, _) = fileapi::stat(&git, Some(&tree), "run.sh").await.expect("stat");

    let plan = fileapi::plan_move(&git, Some(&tree), "run.sh", "tools/run.sh", Some(&src.oid))
        .await
        .expect("rename");
    assert!(git.tree_entry(&plan.tree, "run.sh").await.unwrap().is_none(), "source gone");
    let dst = git.tree_entry(&plan.tree, "tools/run.sh").await.unwrap().expect("destination");
    assert_eq!(dst.mode, "100755", "a rename carries the mode");
    assert_eq!(dst.oid, src.oid, "and the content is untouched");

    // A destination that exists is a refusal, not a silent replace.
    let occupied = fileapi::plan_move(&git, Some(&tree), "run.sh", "a.txt", Some(&src.oid))
        .await
        .expect_err("must refuse");
    assert_eq!(occupied.status(), 412, "{occupied:?}");

    // Into its own subtree, and onto a directory. A directory rename
    // itself is allowed — see `a_directory_rename_copies_no_content`.
    assert_eq!(
        fileapi::plan_move(&git, Some(&tree), "d", "d/inner", None).await.unwrap_err().reason(),
        "bad-path"
    );
    assert_eq!(
        fileapi::plan_move(&git, Some(&tree), "a.txt", "d", Some("*")).await.unwrap_err(),
        FileError::WouldReplaceDirectory
    );
}

/// Deleting a directory in git is always recursive, because a directory
/// only exists while it holds a file. Refused rather than performed.
#[tokio::test]
async fn deleting_a_directory_is_refused_and_a_file_is_not() {
    let (_d, git, tree) = repo_with_tree().await;
    let e = fileapi::plan_delete(&git, Some(&tree), "d", Some("*")).await.expect_err("refuse");
    assert_eq!(e, FileError::WouldDeleteDirectory);

    let (a, _) = fileapi::stat(&git, Some(&tree), "a.txt").await.expect("stat");
    let plan = fileapi::plan_delete(&git, Some(&tree), "a.txt", Some(&a.oid)).await.expect("delete");
    assert!(git.tree_entry(&plan.tree, "a.txt").await.unwrap().is_none());
    assert!(git.tree_entry(&plan.tree, "run.sh").await.unwrap().is_some(), "only the one file");
}

/// A listing reports paths from the root, not relative names, and
/// carries the directory's own oid as an ETag — which lite's file API
/// has no way to provide.
#[tokio::test]
async fn a_listing_is_rooted_and_carries_the_trees_own_etag() {
    let (_d, git, tree) = repo_with_tree().await;
    let root = fileapi::list(&git, Some(&tree), "").await.expect("root");
    assert_eq!(root.etag, tree, "the root listing's etag is the tree itself");
    let mut names: Vec<&str> = root.entries.iter().map(|e| e.path.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["a.txt", "d", "link", "run.sh", "vendor"]);

    let sub = fileapi::list(&git, Some(&tree), "d").await.expect("subdir");
    assert_eq!(sub.entries[0].path, "d/nested.txt", "rooted, not relative");
    assert_eq!(sub.entries[0].name, "nested.txt");
    assert_ne!(sub.etag, tree, "a subdirectory has its own etag");

    // Listing a file is a refusal, matching lite.
    assert_eq!(fileapi::list(&git, Some(&tree), "a.txt").await.unwrap_err().status(), 409);
}

/// An empty repository answers, rather than failing. The first write
/// needs no parent and is not a special case for the caller.
#[tokio::test]
async fn an_empty_repository_reads_as_empty_and_takes_a_first_write() {
    let (_d, git) = bare_repo().await;
    let tree = fileapi::tree_of(&git, "refs/heads/main").await.expect("query");
    assert!(tree.is_none(), "an unborn branch has no tree");

    assert_eq!(fileapi::list(&git, None, "").await.unwrap_err().reason(), "empty-repository");
    assert_eq!(fileapi::read(&git, None, "a.txt", 1 << 20).await.unwrap_err().status(), 404);

    let plan = fileapi::plan_put(&git, None, "a/b/c.txt", b"first\n", None, 1 << 20)
        .await
        .expect("the first write");
    let e = git.tree_entry(&plan.tree, "a/b/c.txt").await.unwrap().expect("present");
    assert_eq!(e.oid, plan.etag);
}

/// Every failure the taxonomy names must be distinguishable. If two
/// collapse to one reason a client cannot tell the user which happened,
/// and §4.6's whole contract is that it can.
#[test]
fn every_failure_has_its_own_reason_and_status() {
    let all = [
        FileError::BadPath("x".into()),
        FileError::NotFound,
        FileError::Unborn,
        FileError::IsADirectory,
        FileError::NotAFile("symbolic link"),
        FileError::WouldReplaceDirectory,
        FileError::WouldDeleteDirectory,
        FileError::TooLarge { size: 2, cap: 1 },
        FileError::PreconditionRequired,
        FileError::FileChanged { etag: "x".into() },
        FileError::Git("boom".into()),
    ];
    let mut reasons: Vec<&str> = all.iter().map(|e| e.reason()).collect();
    let n = reasons.len();
    reasons.sort();
    reasons.dedup();
    assert_eq!(reasons.len(), n, "two failures share a reason: {reasons:?}");
    for e in &all {
        assert!((400..=599).contains(&e.status()), "{e:?} -> {}", e.status());
        assert!(!e.message().is_empty(), "{e:?} has no message for a person");
        // The message must not leak git's own phrasing at a user.
        assert!(!e.message().contains("fatal:"), "{e:?}: {}", e.message());
    }
}

/// **The cost claim, MEASURED.** A rename uploads no content; an edit
/// of the same file uploads all of it. Both arms run the same
/// machinery — `pack_new_objects(tips, ^excludes)`, which is exactly
/// what the batch uploads — and differ in ONE dimension: whether the
/// blob changed.
///
/// This replaces an earlier assertion that compared blob oids, which
/// could not discriminate: git is content-addressed, so re-hashing the
/// same bytes yields the same oid and a "rebuilt" implementation would
/// have passed. Pack bytes cannot be faked that way.
#[tokio::test]
async fn a_rename_uploads_no_content_and_an_edit_uploads_all_of_it() {
    let (_d, git) = bare_repo().await;
    use super::gitcmd::IndexEdit;

    // Incompressible-ish, so the pack size tracks the content rather
    // than zlib's opinion of it. A deterministic LCG, not randomness:
    // a flaky size threshold would be worse than no test.
    let mut x: u32 = 0x1234_5678;
    let big: Vec<u8> = (0..(1 << 20))
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (x >> 16) as u8
        })
        .collect();

    let oid = git.hash_object(&big).await.expect("blob");
    let tree_a = git
        .build_tree(
            None,
            &[IndexEdit::Set { path: "src/big.bin".into(), mode: "100644".into(), oid }],
        )
        .await
        .expect("tree");
    let a = git.commit_tree(&tree_a, &[], "seed", "tester").await.expect("commit");
    // The base pack holds the blob, as it would after the first push.
    let base = git.pack_new_objects(&[a.clone()], &[]).await.expect("pack").expect("non-empty");
    let base_len = std::fs::metadata(git.pack_path(&base)).expect("stat").len();
    assert!(base_len > 900_000, "control: the base really carries the content ({base_len} B)");

    // ARM 1 — rename the directory holding it.
    let renamed = fileapi::plan_move(&git, Some(&tree_a), "src", "moved", None)
        .await
        .expect("rename");
    let b = git.commit_tree(&renamed.tree, &[a.clone()], "rename", "tester").await.expect("commit");
    let rename_pack = git
        .pack_new_objects(&[b], &[a.clone()])
        .await
        .expect("pack")
        .expect("non-empty");
    let rename_len = std::fs::metadata(git.pack_path(&rename_pack)).expect("stat").len();

    // ARM 2 — edit the same file instead. One dimension different.
    let (cur, _) = fileapi::stat(&git, Some(&tree_a), "src/big.bin").await.expect("stat");
    let mut edited = big.clone();
    edited[0] ^= 0xff;
    let put = fileapi::plan_put(&git, Some(&tree_a), "src/big.bin", &edited, Some(&cur.oid), 4 << 20)
        .await
        .expect("edit");
    let c = git.commit_tree(&put.tree, &[a.clone()], "edit", "tester").await.expect("commit");
    let edit_pack = git.pack_new_objects(&[c], &[a]).await.expect("pack").expect("non-empty");
    let edit_len = std::fs::metadata(git.pack_path(&edit_pack)).expect("stat").len();

    eprintln!("rename={rename_len} B  edit={edit_len} B  base={base_len} B");
    assert!(
        rename_len < 4096,
        "a rename must upload only trees and a commit, got {rename_len} B"
    );
    assert!(
        edit_len > 100_000,
        "control: an edit of the same file must upload content, got {edit_len} B"
    );
    assert!(
        edit_len > rename_len * 20,
        "the two arms must differ by orders of magnitude: rename={rename_len} edit={edit_len}"
    );
}

/// `update-ref --stdin` takes ONE entry per ref, and the batch must
/// hand it one however many commands moved that ref. The first entry's
/// `old_oid` is what the transaction still has to check, and the last
/// entry's `new_oid` is where the batch actually left the ref.
#[test]
fn coalescing_keeps_where_a_ref_started_and_where_it_ended() {
    let u = |name: &str, old: &str, new: &str| RefUpdate {
        name: name.into(),
        old_oid: old.into(),
        new_oid: new.into(),
    };
    let out = batch::coalesce_per_ref(&[
        u("refs/heads/main", "aaa", "bbb"),
        u("refs/heads/other", "111", "222"),
        u("refs/heads/main", "bbb", "ccc"),
        u("refs/heads/main", "ccc", "ddd"),
    ]);
    assert_eq!(out.len(), 2, "one entry per ref: {out:?}");
    let main = out.iter().find(|e| e.name == "refs/heads/main").expect("main");
    assert_eq!(main.old_oid, "aaa", "the transaction must still check where the ref STARTED");
    assert_eq!(main.new_oid, "ddd", "and land where the batch left it");
    // Untouched refs pass through, and ORDER is preserved — the control
    // that this is not simply returning the first entry.
    assert_eq!(out[0].name, "refs/heads/main");
    assert_eq!(out[1].name, "refs/heads/other");
    assert_eq!(out[1].old_oid, "111");
    assert_eq!(out[1].new_oid, "222");
    // A single update is unchanged.
    let one = batch::coalesce_per_ref(&[u("refs/heads/x", "0", "1")]);
    assert_eq!(one.len(), 1);
    assert_eq!((one[0].old_oid.as_str(), one[0].new_oid.as_str()), ("0", "1"));
}

/// DIRECTION 4 — the collector, and its control.
///
/// Builds a repository whose snapshot names three packs, two of which
/// are WHOLLY COVERED by the third, then asks the reclaim to collect
/// them. The control is the same setup with the flag off: without it,
/// "two packs went away" is also what a bug that unlinks indiscriminately
/// looks like.
async fn d4_rig(reclaim: bool) -> (Rig, restore::ReclaimReport, Vec<String>) {
    let mut rig = Rig::new().await;
    rig.sc.cfg.reclaim_at_rest = reclaim;
    rig.start().await;

    let c1 = rig.push_commit("refs/heads/main", None, "c1").await;
    let c2 = rig.push_commit("refs/heads/main", Some(&c1), "c2").await;

    // A third pack holding EVERYTHING reachable: no excludes, so
    // `pack-objects` writes the whole history. The two push packs are
    // now dead weight — every reachable object they hold, this one
    // holds too — which is exactly the shape the reclaim collects.
    rig.sc
        .git
        .pack_new_objects(std::slice::from_ref(&c2), &[])
        .await
        .expect("covering pack");

    // Name all three, as a batch with the directory rule would.
    let all = rig.sc.git.local_packs().expect("local packs");
    assert_eq!(all.len(), 3, "the setup must produce three packs");

    // UPLOAD BEFORE NAMING, which is what a batch does (step 4 before
    // step 5) and what the first cut of this setup skipped. A snapshot
    // naming a pack the bucket does not hold is a repository that
    // cannot be restored, and `restore` refuses to serve it — which is
    // how the omission surfaced: the reclaim itself was fine and the
    // SECOND start refused, naming the pack this test had never
    // uploaded.
    let epoch0 = rig.sc.lease().expect("lease").epoch;
    for pack in &all {
        for file in rig.sc.git.pack_siblings(pack) {
            let key = rig.sc.cfg.pack_key(&file);
            let path = rig.sc.git.pack_path(&file);
            super::packio::upload_file(rig.sc.store.as_ref(), &key, &path, epoch0, None)
                .await
                .expect("upload pack sibling");
        }
    }

    let cell = rig.sc.cell().expect("cell").clone();
    let mut next = cell.snap.clone();
    next.packs = all.clone();
    let epoch = rig.sc.lease().expect("lease").epoch;
    let writer = rig.sc.holder_id.clone();
    let new_cell =
        snapshot::cas(rig.sc.store.as_ref(), &rig.sc.cfg, &cell, next, epoch, &writer)
            .await
            .expect("cas");
    rig.sc.cell = Some(new_cell);

    // ASSERT THE PREMISE BEFORE MEASURING ANYTHING. This setup is only
    // a test of the collector if it actually built the shape it meant
    // to: one pack holding every reachable object, and two whose
    // reachable objects it subsumes. An earlier cut asserted only the
    // OUTCOME and produced 2 collected on one run and 1 on the next —
    // a flake that would have been read as a bug in the reclaim. If
    // git packs differently, this now says so in the setup rather than
    // reporting a wrong number from the thing under test.
    let dir = rig.sc.cfg.repo.join("objects/pack");
    let tips: Vec<String> = rig.sc.cell().expect("cell").snap.refs.values().cloned().collect();
    let reach: std::collections::HashSet<String> =
        rig.sc.git.reachable_from(&tips).await.expect("reach").into_iter().collect();
    let mut live: Vec<(String, std::collections::HashSet<String>)> = Vec::new();
    for p in &all {
        let stem = p.trim_end_matches(".pack");
        let ids = rig.sc.git.pack_object_ids(&dir.join(format!("{stem}.idx"))).await.expect("ids");
        live.push((p.clone(), ids.into_iter().filter(|o| reach.contains(o)).collect()));
    }
    let coverers: Vec<&String> =
        live.iter().filter(|(_, l)| l.len() == reach.len()).map(|(p, _)| p).collect();
    assert_eq!(
        coverers.len(),
        1,
        "setup premise broken: {} pack(s) hold all {} reachable objects, live sets {:?}",
        coverers.len(),
        reach.len(),
        live.iter().map(|(p, l)| (p, l.len())).collect::<Vec<_>>()
    );
    let coverer = coverers[0].clone();
    for (p, l) in &live {
        if p == &coverer {
            continue;
        }
        let covering: &std::collections::HashSet<String> =
            &live.iter().find(|(q, _)| q == &coverer).unwrap().1;
        assert!(
            l.iter().all(|o| covering.contains(o)),
            "setup premise broken: {p} is not subsumed by the coverer"
        );
    }

    let report = restore::reclaim_at_rest(&mut rig.sc, restore::AtRest::before_serving())
        .await
        .expect("reclaim");
    (rig, report, all)
}

#[tokio::test]
async fn direction_4_collects_a_wholly_covered_pack_and_unlinks_it() {
    let (rig, report, all) = d4_rig(true).await;

    assert_eq!(report.dropped, 2, "both covered packs should have been collected");
    assert!(report.bytes > 0, "a collected pack cannot be zero bytes");

    let named = &rig.sc.cell().expect("cell").snap.packs;
    assert_eq!(named.len(), 1, "the snapshot should name only the coverer, got {named:?}");

    // UNLINKED, not retained. The retain form is the refuted one: a
    // retained pack is excluded from the listing forever, so a retry
    // reusing its name lands with its objects unnamed.
    let dir = rig.sc.cfg.repo.join("objects/pack");
    let gone: Vec<&String> = all.iter().filter(|p| !named.contains(p)).collect();
    assert_eq!(gone.len(), 2);
    for p in &gone {
        assert!(!dir.join(p).exists(), "{p} was dropped from the snapshot but is still on disk");
    }
    assert!(rig.sc.retained.is_empty(), "the reclaim must UNLINK, never retain");

    // And the repository still stands: every ref resolves and the
    // connectivity is whole from the packs that remain.
    let out = rig.sc.git.must(&["fsck", "--connectivity-only", "--no-progress"], None).await;
    assert!(out.is_ok(), "fsck failed after the reclaim: {out:?}");

    // A SECOND START MUST COLLECT NOTHING. The reclaim runs on every
    // restore, so a rule that keeps finding something to drop would
    // shrink the repository a little on each restart until it had
    // dropped something it needed. Running the real startup pair again
    // — restore, then the window — is what says it converges.
    let mut rig = rig;
    restore::restore(&mut rig.sc).await.expect("second restore");
    let again = restore::reclaim_at_rest(&mut rig.sc, restore::AtRest::before_serving())
        .await
        .expect("second reclaim");
    assert_eq!(again.dropped, 0, "the reclaim is not idempotent — a restart would keep eating");
    assert_eq!(
        rig.sc.cell().expect("cell").snap.packs.len(),
        1,
        "the second start changed what the snapshot names"
    );
}

#[tokio::test]
async fn direction_4_collects_nothing_when_it_is_off() {
    let (rig, report, all) = d4_rig(false).await;

    assert_eq!(report.dropped, 0, "the control arm must collect nothing");
    assert_eq!(report.bytes, 0);
    let named = &rig.sc.cell().expect("cell").snap.packs;
    assert_eq!(named.len(), 3, "the control arm must still name all three packs");
    let dir = rig.sc.cfg.repo.join("objects/pack");
    for p in &all {
        assert!(dir.join(p).exists(), "the control arm unlinked {p}");
    }
}

/// A DECLINE AND A COMPLETE WALK ARE DIFFERENT STATES, and the empty
/// report used to be both. `reclaim_at_rest` has five early returns and
/// every one of them yielded `ReclaimReport::default()` — the same value
/// a full walk yields when nothing is collectable. So "dropped 0" could
/// mean "this repository is tidy" or "I could not reason about it", and
/// neither the operator's log line nor a test could tell which.
///
/// The control is the pair: the SAME assertion run against a rig that
/// really does walk. If `declined` were always `None` the first half
/// would pass on its own and mean nothing.
#[tokio::test]
async fn a_reclaim_that_declines_is_distinguishable_from_one_that_found_nothing() {
    // A repository with ONE named pack: nothing can cover it, so the
    // function declines rather than walking.
    let mut rig = Rig::new().await;
    rig.sc.cfg.reclaim_at_rest = true;
    rig.start().await;
    rig.push_commit("refs/heads/main", None, "c1").await;
    let one = restore::reclaim_at_rest(&mut rig.sc, restore::AtRest::before_serving())
        .await
        .expect("reclaim");
    assert_eq!(one.dropped, 0);
    assert_eq!(
        one.declined,
        Some("fewer than two named packs"),
        "a decline must NAME itself, not return a legal-looking zero"
    );

    // THE OTHER ARM: the d4 rig walks three packs to a verdict. Same
    // `dropped`-shaped report, opposite meaning.
    let (_rig, report, all) = d4_rig(true).await;
    assert_eq!(all.len(), 3);
    assert_eq!(report.declined, None, "this rig reasons; it must not report a decline");
    assert_eq!(report.considered, 3, "it examined every named pack");
    assert!(report.dropped > 0, "and it collected — otherwise the arms are not opposite");
}

// ─────────────────────────────────────────────────────────────────────
// THE COLLECTOR'S COST ON THE WAKE PATH
//
// `reclaim_at_rest` runs at `server.rs:247` — between the restore and
// `Phase::Serving`, so on every start AND every wake from idle, which
// is user-visible latency (runci woke a slept repository with a plain
// HTTP read in 7 s). It walks every ref, reads an `.idx` per named
// pack, and runs a greedy loop that re-tests every candidate against
// every kept pack. Every drill so far ran at <= 23 packs and a handful
// of refs, so what it costs on a repository of real size is UNMEASURED.
//
// This is a measurement, not an assertion. `#[ignore]`d, run by hand:
//
//   cargo test -p flint-forge --lib d4_scale -- --ignored --nocapture
// ─────────────────────────────────────────────────────────────────────

/// One rung. Builds `packs` packs (one push each, `files` files per
/// commit), adds `extra_refs` refs pointing at commits that already
/// exist, and times `reclaim_at_rest` over the result.
///
/// Returns (elapsed, named packs, refs, reachable objects).
async fn d4_scale_once(
    packs: usize,
    files: usize,
    extra_refs: usize,
    reclaim: bool,
) -> (std::time::Duration, usize, usize, usize) {
    let mut rig = Rig::new().await;
    rig.sc.cfg.reclaim_at_rest = reclaim;
    rig.start().await;

    let mut tip: Option<String> = None;
    let mut chain: Vec<String> = Vec::new();
    for i in 0..packs {
        let owned: Vec<(String, String)> = (0..files.max(1))
            .map(|f| (format!("f{f}.txt"), format!("c{i}-f{f}\n")))
            .collect();
        let spec: Vec<(&str, &str)> =
            owned.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let c = rig.stage_commit(tip.as_deref(), &spec, &format!("c{i}")).await;
        let old = tip.clone().unwrap_or_else(zero);
        let reports = rig
            .run(vec![push(
                i as u64 + 1,
                vec![RefUpdate { name: "refs/heads/main".into(), old_oid: old, new_oid: c.clone() }],
            )])
            .await;
        assert!(is_ok(&reports[0].results[0]), "push c{i}: {:?}", reports[0].results[0]);
        chain.push(c.clone());
        tip = Some(c);
    }

    // Refs pointing at commits that already exist: they add tips for
    // `reachable_from` to walk without adding objects, which is what
    // separates the ref dimension from the size dimension.
    if extra_refs > 0 {
        let cmds: Vec<RefUpdate> = (0..extra_refs)
            .map(|r| RefUpdate {
                name: format!("refs/heads/r{r}"),
                old_oid: zero(),
                new_oid: chain[r % chain.len()].clone(),
            })
            .collect();
        let reports = rig.run(vec![push(9_000, cmds)]).await;
        for res in &reports[0].results {
            assert!(is_ok(res), "extra ref push: {res:?}");
        }
    }

    // THE PREMISE, ASSERTED BEFORE ANYTHING IS TIMED. `reclaim_at_rest`
    // has five early returns and every one of them yields the SAME
    // empty report that a complete walk yields when there is nothing to
    // collect. A rung that tripped one would print a fast, flat,
    // meaningless number and read as "the collector is cheap". So the
    // conditions are checked here: a rung that cannot measure fails
    // loudly instead of timing a return.
    let cell = rig.sc.cell().expect("cell").clone();
    let named = cell.snap.packs.clone();
    let dir = rig.sc.cfg.repo.join("objects/pack");
    assert!(named.len() >= 2, "only {} named pack(s): reclaim returns early", named.len());
    for p in &named {
        let stem = p.trim_end_matches(".pack");
        assert!(dir.join(format!("{stem}.idx")).exists(), "{p}: no .idx, reclaim returns early");
        assert!(dir.join(p).exists(), "{p}: not on disk, reclaim returns early");
    }

    let tips: Vec<String> = cell.snap.refs.values().cloned().collect();
    let reach = rig.sc.git.reachable_from(&tips).await.expect("reach").len();

    let t0 = std::time::Instant::now();
    restore::reclaim_at_rest(&mut rig.sc, restore::AtRest::before_serving())
        .await
        .expect("reclaim");
    let dt = t0.elapsed();
    (dt, named.len(), cell.snap.refs.len(), reach)
}

/// The ladder. The OFF column is the control: with the flag down the
/// function returns on its first line, so it prices the CALL and not
/// the rig — if ON and OFF were both ~0 the ladder would be measuring
/// the harness, and the table would say so.
#[tokio::test]
#[ignore = "a measurement, not an assertion — run by hand with --nocapture"]
async fn d4_scale_ladder() {
    println!(
        "\n{:>6} {:>6} {:>6} {:>9} {:>11} {:>9}",
        "packs", "files", "refs", "objects", "ON (ms)", "OFF (ms)"
    );
    // The pack dimension alone, for a quick cross-platform reading: it
    // is the only term that matters (objects ~5 us each, refs free) and
    // it is the cheap half of the ladder to build.
    if std::env::var("D4_SCALE_PACKS_ONLY").is_ok() {
        for (packs, files, extra) in [(8usize, 1usize, 0usize), (16, 1, 0), (32, 1, 0), (64, 1, 0)] {
            let (on, np, nr, obj) = d4_scale_once(packs, files, extra, true).await;
            let (off, _, _, _) = d4_scale_once(packs, files, extra, false).await;
            println!(
                "{np:6} {files:6} {nr:6} {obj:9} {:11.1} {:9.3}",
                on.as_secs_f64() * 1000.0,
                off.as_secs_f64() * 1000.0
            );
        }
        println!();
        return;
    }
    let rungs: [(usize, usize, usize); 11] = [
        // the pack dimension, at one file per commit
        (8, 1, 0),
        (16, 1, 0),
        (32, 1, 0),
        (64, 1, 0),
        // the size dimension, at the production pack cap (fold_max_packs)
        (64, 8, 0),
        (64, 32, 0),
        (64, 128, 0),
        (64, 512, 0),
        // the ref dimension, size held down
        (64, 8, 64),
        (64, 8, 512),
        (64, 8, 2048),
    ];
    for (packs, files, extra) in rungs {
        let (on, np, nr, obj) = d4_scale_once(packs, files, extra, true).await;
        let (off, _, _, _) = d4_scale_once(packs, files, extra, false).await;
        println!(
            "{np:6} {files:6} {nr:6} {obj:9} {:11.1} {:9.3}",
            on.as_secs_f64() * 1000.0,
            off.as_secs_f64() * 1000.0
        );
    }
    println!();
}
