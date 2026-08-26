//! The lean battery: every leg maps to a formal-model invariant or a
//! confirmed review finding (docs/plans/flint-lean-plan.md §10). Runs
//! against MemoryStore's full conditional semantics — 412 on both put
//! flavors, epoch CAS — so these are protocol tests, not stubs.

use std::sync::Arc;

use bytes::Bytes;

use flint_store::memory::MemoryStore;
use flint_store::{crc64_nvme, GenerationStamps, ObjectStore, PutCondition};

use super::inbox::{self, InboxEntry};
use super::lease::{self, ClaimOutcome};
use super::manifest;
use super::state::SidecarState;
use super::{now_unix, LeanConfig, LeanError, Sidecar};

const PREFIX: &str = "tenant/proj1";

fn cfg_for(root: &std::path::Path) -> LeanConfig {
    LeanConfig::new(PREFIX, root)
}

async fn sidecar(store: &Arc<MemoryStore>, root: &std::path::Path) -> Sidecar {
    let cfg = cfg_for(root);
    let state = SidecarState::open(cfg.state_dir()).unwrap();
    Sidecar { store: store.clone() as Arc<dyn ObjectStore>, cfg, state, lease: None }
}

/// Claim, looping claim_step (a fresh or released cell claims on the
/// first step; a foreign one needs the quiet polls).
async fn claim_until_held(sc: &mut Sidecar, max_steps: u32) -> bool {
    for _ in 0..max_steps {
        match lease::claim_step(sc).await.unwrap() {
            ClaimOutcome::Claimed(_) => return true,
            ClaimOutcome::Waiting { .. } => {}
        }
    }
    false
}

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn read(root: &std::path::Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(root.join(rel)).ok()
}

/// Bump a file's mtime past the 1-second stat granularity so the scan
/// sees the change without sleeping.
fn backdate_baseline(sc: &Sidecar, rel: &str) {
    let mut b = sc.state.load_baseline().unwrap();
    if let Some(e) = b.entries.get_mut(rel) {
        e.mtime_unix -= 10;
    }
    sc.state.save_baseline(&b).unwrap();
}

/// Simulate the GATEWAY's HITL write: object PUT first (fresh read →
/// If-Match current / If-None-Match for a create), then the inbox entry.
async fn hitl_write(
    store: &Arc<MemoryStore>,
    cfg: &LeanConfig,
    path: &str,
    content: &str,
    author: &str,
) -> Result<String, LeanError> {
    let key = cfg.file_key(path);
    let cond = match store.head(&key).await {
        Ok(meta) => PutCondition::IfMatch(meta.etag),
        Err(_) => PutCondition::IfNoneMatchAny,
    };
    let body = Bytes::from(content.to_string());
    let crc = crc64_nvme(&body);
    let stamps = GenerationStamps {
        generation: 0,
        epoch: 0,
        flush_uuid: format!("gateway-{author}"),
        boundary_source: None,
        posix: None,
    };
    let meta = store.put_whole(&key, body, &cond, &stamps, crc).await?;
    inbox::gateway_append(
        store.as_ref(),
        cfg,
        InboxEntry {
            path: path.to_string(),
            etag: meta.etag.clone(),
            author: author.to_string(),
            added_unix: now_unix(),
        },
    )
    .await?;
    Ok(meta.etag)
}

// ── the battery ──────────────────────────────────────────────────────

/// Publish → fresh checkout materializes byte-identically; a delete
/// takes TWO scans to publish (the two-consecutive-scans rule) and the
/// GC removes the object only then.
#[tokio::test]
async fn checkout_publish_roundtrip_and_two_scan_delete() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap(); // empty subtree
    write(dir_a.path(), "src/main.rs", "fn main() {}");
    write(dir_a.path(), "README.md", "hello");
    let r = a.run_barrier().await.unwrap();
    assert_eq!(r.uploaded.len(), 2);

    // Fresh pod elsewhere: checkout sees both files.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    let cr = b.checkout().await.unwrap();
    assert_eq!(cr.materialized, 2);
    assert_eq!(read(dir_b.path(), "src/main.rs").unwrap(), "fn main() {}");

    // Delete: barrier 1 = first absence (still cited), barrier 2 = gone.
    std::fs::remove_file(dir_a.path().join("README.md")).unwrap();
    let r1 = a.run_barrier().await.unwrap();
    assert!(r1.deleted.is_empty(), "first absence must not delete");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    assert!(m.manifest.entries.contains_key("README.md"));
    let r2 = a.run_barrier().await.unwrap();
    assert_eq!(r2.deleted, vec!["README.md".to_string()]);
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    assert!(!m.manifest.entries.contains_key("README.md"));
    assert!(store.head(&a.cfg.file_key("README.md")).await.is_err());
}

/// The review's worst finding, as a drill leg: a HITL upload with NO
/// sync must survive any number of barriers — consumed into the tree,
/// re-cited by the manifest, present in a fresh checkout.
#[tokio::test]
async fn hitl_upload_survives_two_barriers_without_sync() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "agent.txt", "agent work");
    sc.run_barrier().await.unwrap();

    // The user uploads a NEW file mid-session via the UI.
    hitl_write(&store, &sc.cfg, "docs/upload.pdf", "user bytes", "dilip").await.unwrap();

    // Two automatic barriers with unrelated agent activity.
    write(dir.path(), "agent.txt", "agent work v2 — longer");
    sc.run_barrier().await.unwrap();
    sc.run_barrier().await.unwrap();

    // Consumed into the live tree...
    assert_eq!(read(dir.path(), "docs/upload.pdf").unwrap(), "user bytes");
    // ...cited by the manifest...
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert!(m.manifest.entries.contains_key("docs/upload.pdf"), "amputated!");
    // ...and materialized by a fresh checkout.
    let dir2 = tempfile::tempdir().unwrap();
    let mut sc2 = sidecar(&store, dir2.path()).await;
    sc2.checkout().await.unwrap();
    assert_eq!(read(dir2.path(), "docs/upload.pdf").unwrap(), "user bytes");
}

/// UI edit + agent edit of ONE path: both versions recoverable, a
/// conflict surfaced, never a silent winner (the drill leg pinned in
/// plan Phase 6).
#[tokio::test]
async fn ui_edit_vs_agent_edit_never_a_silent_winner() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "notes.md", "v1");
    sc.run_barrier().await.unwrap();

    // Concurrent edits: the user via the UI, the agent locally.
    hitl_write(&store, &sc.cfg, "notes.md", "user version", "dilip").await.unwrap();
    write(dir.path(), "notes.md", "agent version");
    backdate_baseline(&sc, "notes.md"); // make the local edit scan-visible

    sc.run_barrier().await.unwrap();

    // Locally-dirty wins the tree and the manifest...
    assert_eq!(read(dir.path(), "notes.md").unwrap(), "agent version");
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    let cited = &m.manifest.entries["notes.md"];
    let (_, body) = store.get_whole(&cited.key, Some(&cited.etag)).await.unwrap();
    assert_eq!(&body[..], b"agent version");

    // ...but the conflict is surfaced and the USER's bytes were
    // preserved at the conflict key before being superseded.
    let conflicts = sc.state.load_conflicts().unwrap();
    let c = conflicts.iter().find(|c| c.kind == "consume-dirty").expect("conflict surfaced");
    assert_eq!(c.path, "notes.md");
    let preserved = c.preserved_key.as_ref().expect("foreign bytes preserved");
    let (_, body) = store.get_whole(preserved, None).await.unwrap();
    assert_eq!(&body[..], b"user version");
}

/// Restart matrix, marker-present row: a container restart over a live
/// tree must NOT re-materialize — an unpublished local delete must not
/// resurrect (LeanRematerialize.cfg's counterexample).
#[tokio::test]
async fn container_restart_never_resurrects_unpublished_delete() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "keep.txt", "keep");
    write(dir.path(), "gone.txt", "delete me");
    sc.run_barrier().await.unwrap();

    // The agent deletes; the container restarts BEFORE any barrier.
    std::fs::remove_file(dir.path().join("gone.txt")).unwrap();
    drop(sc);
    let mut sc = sidecar(&store, dir.path()).await; // same emptyDir
    let cr = sc.checkout().await.unwrap();
    assert!(cr.resumed_live_tree, "marker present ⇒ live-tree row");
    assert_eq!(cr.materialized, 0, "must not re-materialize");
    assert!(read(dir.path(), "gone.txt").is_none(), "delete resurrected!");

    // The lease self-recognizes via the persisted incarnation id —
    // immediately, no quiet-poll wait.
    assert!(claim_until_held(&mut sc, 1).await, "self-recognition must not wait");

    // And the delete still publishes (two scans later).
    sc.run_barrier().await.unwrap();
    sc.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert!(!m.manifest.entries.contains_key("gone.txt"));
}

/// Takeover: the successor rotates the manifest BEFORE serving, so the
/// deposed straggler's next barrier is fenced — its CAS can never land
/// (Inv_NoStragglerInstall).
#[tokio::test]
async fn takeover_rotation_fences_the_straggler() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir_a.path(), "f.txt", "from A");
    a.run_barrier().await.unwrap();
    let seq_before = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;

    // A stalls (stops renewing). B replaces it: fresh emptyDir, fresh
    // identity ⇒ the foreign-holder path, quiet polls, then takeover.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    assert!(
        !claim_until_held(&mut b, 3).await,
        "a fresh replacement must NOT claim instantly over a live-looking lease"
    );
    assert!(claim_until_held(&mut b, 10).await, "quiet polls exhausted ⇒ takeover");
    let rotated = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap();
    assert_eq!(rotated.manifest.seq, seq_before + 1, "rotation: seq++, content-identical");
    assert_eq!(rotated.manifest.entries.len(), 1);
    b.checkout().await.unwrap();
    assert_eq!(read(dir_b.path(), "f.txt").unwrap(), "from A");

    // A thaws mid-work and tries to publish: fenced, and the bucket is
    // untouched by it.
    write(dir_a.path(), "f.txt", "stale straggler bytes");
    backdate_baseline(&a, "f.txt");
    let err = a.run_barrier().await.unwrap_err();
    assert!(matches!(err, LeanError::Fenced(_)), "straggler must fence, got: {err:?}");
    let m = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap();
    assert_eq!(m.manifest.seq, rotated.manifest.seq, "straggler CAS landed!");
    let cited = &m.manifest.entries["f.txt"];
    let (_, body) = store.get_whole(&cited.key, Some(&cited.etag)).await.unwrap();
    assert_eq!(&body[..], b"from A", "straggler bytes reached a cited object");
}

/// The 412 AdoptOwn arm: a crashed/torn earlier PUT (our flush_uuid,
/// same bytes) is recognized and cited without a conflict and without
/// a blind overwrite.
#[tokio::test]
async fn adopt_own_412_converges_without_conflict() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();

    // A "previous incarnation's" torn barrier: the PUT landed, the
    // response was lost, the baseline was never advanced. Its uuid is
    // in our intent history (the persisted journal).
    write(dir.path(), "big.bin", "payload");
    let crashed_uuid = "crashed-barrier-uuid".to_string();
    let mut intent = sc.state.load_intent().unwrap();
    intent.flush_uuid = crashed_uuid.clone();
    intent.keys = vec![sc.cfg.file_key("big.bin")];
    sc.state.save_intent(&intent).unwrap();
    sc.state.clear_intent_keys().unwrap(); // uuid moves into history
    let body = Bytes::from("payload");
    let crc = crc64_nvme(&body);
    let stamps = GenerationStamps {
        generation: 1,
        epoch: 1,
        flush_uuid: crashed_uuid,
        boundary_source: None,
        posix: None,
    };
    store
        .put_whole(&sc.cfg.file_key("big.bin"), body, &PutCondition::IfNoneMatchAny, &stamps, crc)
        .await
        .unwrap();

    // The restarted barrier: If-None-Match 412s (object exists), HEAD
    // recognizes our uuid + crc ⇒ adopt and cite.
    let r = sc.run_barrier().await.unwrap();
    assert_eq!(r.uploaded, vec!["big.bin".to_string()]);
    assert!(r.parked.is_empty());
    assert!(sc.state.load_conflicts().unwrap().is_empty(), "AdoptOwn must not conflict");
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert!(m.manifest.entries.contains_key("big.bin"));
}

/// A foreign 412 parks the path — the inherited LOCAL-WINS overwrite is
/// exactly what lean must NOT do (LeanLocalWins.cfg's counterexample).
#[tokio::test]
async fn foreign_412_parks_never_overwrites() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();

    // An unknown writer's object sits at the key (no inbox entry — a
    // mixed-writer bucket, or a write our consume missed).
    let body = Bytes::from("foreign bytes");
    let crc = crc64_nvme(&body);
    let stamps = GenerationStamps {
        generation: 9,
        epoch: 0,
        flush_uuid: "someone-else".into(),
        boundary_source: None,
        posix: None,
    };
    let foreign =
        store.put_whole(&sc.cfg.file_key("f.txt"), body, &PutCondition::IfNoneMatchAny, &stamps, crc)
            .await
            .unwrap();

    write(dir.path(), "f.txt", "agent bytes");
    let r = sc.run_barrier().await.unwrap();
    assert_eq!(r.parked, vec!["f.txt".to_string()]);
    assert!(r.uploaded.is_empty());

    // The foreign bytes are UNTOUCHED and the conflict is surfaced.
    let (meta, body) = store.get_whole(&sc.cfg.file_key("f.txt"), None).await.unwrap();
    assert_eq!(meta.etag, foreign.etag);
    assert_eq!(&body[..], b"foreign bytes");
    let conflicts = sc.state.load_conflicts().unwrap();
    assert!(conflicts.iter().any(|c| c.kind == "upload-412-parked" && c.path == "f.txt"));
    // And the manifest does not lie about the path (no citation of a
    // generation we never published).
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert!(!m.manifest.entries.contains_key("f.txt"));
}

/// The GC HEAD-guard: a delete-eligible key whose current ETag the
/// sidecar does not recognize is NEVER deleted (LeanGCUnguarded.cfg's
/// counterexample — the HITL re-create).
#[tokio::test]
async fn gc_refuses_unrecognized_etag() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "doc.txt", "v1");
    sc.run_barrier().await.unwrap();

    // The agent deletes; absence ages through one scan.
    std::fs::remove_file(dir.path().join("doc.txt")).unwrap();
    sc.run_barrier().await.unwrap(); // first absence

    // A UI write re-creates the path AFTER our consume window — model
    // it as a direct foreign PUT (etag the sidecar never learned).
    let body = Bytes::from("user re-created");
    let crc = crc64_nvme(&body);
    let stamps = GenerationStamps {
        generation: 0,
        epoch: 0,
        flush_uuid: "gateway-late".into(),
        boundary_source: None,
        posix: None,
    };
    let cur = store.head(&sc.cfg.file_key("doc.txt")).await.unwrap();
    store
        .put_whole(
            &sc.cfg.file_key("doc.txt"),
            body,
            &PutCondition::IfMatch(cur.etag),
            &stamps,
            crc,
        )
        .await
        .unwrap();

    // Second-absence barrier: the manifest uncites, but the DELETE must
    // refuse the unrecognized ETag.
    let r = sc.run_barrier().await.unwrap();
    assert!(r.deleted.is_empty(), "GC deleted a foreign re-create!");
    let (_, body) = store.get_whole(&sc.cfg.file_key("doc.txt"), None).await.unwrap();
    assert_eq!(&body[..], b"user re-created");
    let conflicts = sc.state.load_conflicts().unwrap();
    assert!(conflicts.iter().any(|c| c.kind == "gc-skip" && c.path == "doc.txt"));
}

/// Delete/modify across writers: a local delete loses to a foreign
/// manifest change — the entry is preserved, queued for consume, and
/// the object survives GC (the model's merge counterexample).
#[tokio::test]
async fn local_delete_loses_to_foreign_modify() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "shared.txt", "v1");
    sc.run_barrier().await.unwrap();

    // A second writer edits the object AND re-cites it in the manifest
    // (a hub-style writer or a future gateway manifest reconciler).
    let body = Bytes::from("their v2");
    let crc = crc64_nvme(&body);
    let stamps = GenerationStamps {
        generation: 2,
        epoch: 0,
        flush_uuid: "other-writer".into(),
        boundary_source: None,
        posix: None,
    };
    let cur = store.head(&sc.cfg.file_key("shared.txt")).await.unwrap();
    let newmeta = store
        .put_whole(&sc.cfg.file_key("shared.txt"), body, &PutCondition::IfMatch(cur.etag), &stamps, crc)
        .await
        .unwrap();
    let loaded = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    let mut theirs = loaded.manifest.clone();
    theirs.seq += 1;
    theirs.entries.get_mut("shared.txt").unwrap().etag = newmeta.etag.clone();
    manifest::cas_write(store.as_ref(), &sc.cfg, &theirs, Some(&loaded.etag), 0, "other-writer")
        .await
        .unwrap();

    // The agent deletes locally; the FIRST barrier sees first-absence
    // AND an un-consumed foreign manifest change: the merge must
    // PRESERVE the foreign entry and queue it (never blind-delete —
    // the model's GC-vs-merge counterexample).
    std::fs::remove_file(dir.path().join("shared.txt")).unwrap();
    let r1 = sc.run_barrier().await.unwrap();
    assert!(r1.deleted.is_empty());
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert_eq!(m.manifest.entries["shared.txt"].etag, newmeta.etag, "foreign entry dropped!");
    let (_, body) = store.get_whole(&sc.cfg.file_key("shared.txt"), None).await.unwrap();
    assert_eq!(&body[..], b"their v2");
    let ib = inbox::load(store.as_ref(), &sc.cfg).await.unwrap();
    assert!(ib.doc.entries.iter().any(|e| e.path == "shared.txt" && e.etag == newmeta.etag));

    // The SECOND barrier consumes the queued foreign edit against the
    // local delete: the decided policy is locally-dirty wins WITH the
    // conflict surfaced and the foreign bytes preserved first — the
    // delete then publishes. Never a silent winner.
    let r2 = sc.run_barrier().await.unwrap();
    assert_eq!(r2.deleted, vec!["shared.txt".to_string()]);
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert!(!m.manifest.entries.contains_key("shared.txt"));
    let conflicts = sc.state.load_conflicts().unwrap();
    let c = conflicts
        .iter()
        .find(|c| c.kind == "consume-dirty" && c.path == "shared.txt")
        .expect("delete-vs-edit conflict must surface");
    let preserved = c.preserved_key.as_ref().expect("foreign bytes preserved");
    let (_, body) = store.get_whole(preserved, None).await.unwrap();
    assert_eq!(&body[..], b"their v2", "the edit must stay recoverable after the delete wins");
}

/// The window cell: a gateway replica must refuse a UI write while a
/// live barrier window is open, and admit it again after the clear —
/// and an expired window never wedges HITL.
#[tokio::test]
async fn window_refuses_hitl_and_expiry_unwedges() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let sc = sidecar(&store, dir.path()).await;
    let entry = |p: &str| InboxEntry {
        path: p.into(),
        etag: "e".into(),
        author: "dilip".into(),
        added_unix: now_unix(),
    };

    inbox::open_window(store.as_ref(), &sc.cfg, 1, now_unix() + 300).await.unwrap();
    let err = inbox::gateway_append(store.as_ref(), &sc.cfg, entry("a.txt")).await.unwrap_err();
    assert!(matches!(err, LeanError::State(_)), "live window must refuse");

    inbox::clear_window(store.as_ref(), &sc.cfg, 1, &[]).await.unwrap();
    inbox::gateway_append(store.as_ref(), &sc.cfg, entry("a.txt")).await.unwrap();

    // A dead sidecar's window (deadline in the past) does not wedge.
    inbox::open_window(store.as_ref(), &sc.cfg, 1, now_unix() - 10).await.unwrap();
    inbox::gateway_append(store.as_ref(), &sc.cfg, entry("b.txt")).await.unwrap();
}

/// Files over whole_put_max go through the streaming multipart compose
/// (never put_whole): publish, guarded update, and roundtrip must all
/// hold on that path. whole_put_max is shrunk so a small file takes the
/// large-file road against MemoryStore's real MPU semantics.
#[tokio::test]
async fn large_file_publishes_via_compose_and_roundtrips() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    sc.cfg.whole_put_max = 8; // 8 bytes: everything bigger composes
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();

    let big_v1: String = (0..200).map(|i| format!("line {i}\n")).collect();
    write(dir.path(), "model.bin", &big_v1);
    let r = sc.run_barrier().await.unwrap();
    assert_eq!(r.uploaded, vec!["model.bin".to_string()]);
    assert!(r.deferred.is_empty());

    // Roundtrip through a fresh checkout.
    let dir2 = tempfile::tempdir().unwrap();
    let mut sc2 = sidecar(&store, dir2.path()).await;
    sc2.checkout().await.unwrap();
    assert_eq!(read(dir2.path(), "model.bin").unwrap(), big_v1);

    // Guarded update on the compose path (If-Match the prior etag).
    let big_v2 = format!("{big_v1}and more\n");
    write(dir.path(), "model.bin", &big_v2);
    backdate_baseline(&sc, "model.bin");
    let r = sc.run_barrier().await.unwrap();
    assert_eq!(r.uploaded, vec!["model.bin".to_string()]);
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    let cited = &m.manifest.entries["model.bin"];
    let (_, body) = store.get_whole(&cited.key, Some(&cited.etag)).await.unwrap();
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), big_v2);
}

/// The occupancy lock: a second sidecar over the SAME workspace tree
/// must refuse to start — self-recognition of the lease is only sound
/// because the previous process is provably gone (observed live on the
/// 0b rig: a concurrent process deposed a live sibling and both wrote
/// the tree).
#[tokio::test]
async fn second_sidecar_on_one_tree_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let _held = SidecarState::open(cfg.state_dir()).unwrap();
    let Err(err) = SidecarState::open(cfg.state_dir()) else {
        panic!("second open over a held workspace must refuse");
    };
    assert!(matches!(err, LeanError::State(_)));
}

/// Checkout budgets refuse BEFORE materializing; no marker is written.
#[tokio::test]
async fn checkout_budget_refuses_before_first_byte() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "big.txt", "0123456789012345678901234567890123456789");
    sc.run_barrier().await.unwrap();

    let dir2 = tempfile::tempdir().unwrap();
    let mut sc2 = sidecar(&store, dir2.path()).await;
    sc2.cfg.max_bytes = 10;
    let err = sc2.checkout().await.unwrap_err();
    assert!(matches!(err, LeanError::Budget(_)));
    assert!(!sc2.state.marker_present(), "budget refusal must not gate-open the agent");
    assert!(read(dir2.path(), "big.txt").is_none());
}

// ── the gateway verbs (Phase 3) ──────────────────────────────────────

const GW_TOKEN: &str = "test-bearer-0123456789abcdef";

fn gw_core(store: &Arc<MemoryStore>) -> Arc<super::gateway::GatewayCore> {
    let mut workspaces = std::collections::BTreeMap::new();
    workspaces.insert("proj1".to_string(), PREFIX.to_string());
    Arc::new(super::gateway::GatewayCore {
        store: store.clone() as Arc<dyn ObjectStore>,
        workspaces,
        token: GW_TOKEN.to_string(),
        max_put_bytes: 8 * 1024 * 1024,
    })
}

fn gw_req() -> warp::test::RequestBuilder {
    warp::test::request().header("authorization", format!("Bearer {GW_TOKEN}"))
}

/// Auth + tenancy: wrong bearer 401; unknown workspace 404; reserved
/// and traversal paths refused.
#[tokio::test]
async fn gateway_auth_tenancy_and_path_hygiene() {
    let store = Arc::new(MemoryStore::new());
    let routes = super::gateway::routes(gw_core(&store));

    let res = warp::test::request()
        .method("GET")
        .path("/lean/v1/proj1/status")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 401);

    let res = gw_req().method("GET").path("/lean/v1/nope/status").reply(&routes).await;
    assert_eq!(res.status(), 404);

    for bad in ["../../etc/passwd", ".flint/lean/manifest", ".flint-sync/baseline.json"] {
        let res = gw_req()
            .method("PUT")
            .path(&format!("/lean/v1/proj1/files/{bad}"))
            .body("x")
            .reply(&routes)
            .await;
        assert_eq!(res.status(), 400, "path {bad:?} must be refused");
    }
}

/// The full HITL flow THROUGH the gateway: PUT lands object + inbox
/// entry, the sidecar's next barrier consumes and cites it, and the
/// gateway serves it back — first from the inbox fallback, then from
/// the manifest citation.
#[tokio::test]
async fn gateway_hitl_put_consumed_and_cited_by_barrier() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();

    let routes = super::gateway::routes(gw_core(&store));
    let res = gw_req()
        .method("PUT")
        .path("/lean/v1/proj1/files/docs/spec.md")
        .header("x-flint-author", "dilip")
        .body("user upload via gateway")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200, "{:?}", res.body());

    // Readable immediately via the inbox fallback (uncited yet).
    let res = gw_req().method("GET").path("/lean/v1/proj1/files/docs/spec.md").reply(&routes).await;
    assert_eq!(res.status(), 200);
    assert_eq!(&res.body()[..], b"user upload via gateway");

    // The barrier consumes + cites; the file lands in the agent tree.
    sc.run_barrier().await.unwrap();
    assert_eq!(read(dir.path(), "docs/spec.md").unwrap(), "user upload via gateway");
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert!(m.manifest.entries.contains_key("docs/spec.md"));

    // Still readable — now via the citation.
    let res = gw_req().method("GET").path("/lean/v1/proj1/files/docs/spec.md").reply(&routes).await;
    assert_eq!(res.status(), 200);
}

/// The window gate: a PUT during a live barrier window is refused with
/// Retry-After; an expired window admits (the dead-sidecar unwedge).
#[tokio::test]
async fn gateway_put_refused_while_window_open() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let sc = sidecar(&store, dir.path()).await;
    let routes = super::gateway::routes(gw_core(&store));

    inbox::open_window(store.as_ref(), &sc.cfg, 1, now_unix() + 120).await.unwrap();
    let res = gw_req()
        .method("PUT")
        .path("/lean/v1/proj1/files/a.txt")
        .body("x")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 409);
    assert!(res.headers().contains_key("retry-after"));

    inbox::clear_window(store.as_ref(), &sc.cfg, 1, &[]).await.unwrap();
    let res = gw_req()
        .method("PUT")
        .path("/lean/v1/proj1/files/a.txt")
        .body("x")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200);

    inbox::open_window(store.as_ref(), &sc.cfg, 1, now_unix() - 5).await.unwrap();
    let res = gw_req()
        .method("PUT")
        .path("/lean/v1/proj1/files/b.txt")
        .body("y")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200, "an expired window must not wedge HITL");
}

/// P5's teeth: the manifest CAS verb validates the claimed epoch
/// against the cell PER REQUEST — a deposed epoch is 403 even with a
/// correct CAS token (the LeanEpochOnlyHolds arm, now enforced).
#[tokio::test]
async fn gateway_manifest_cas_rejects_stale_epoch() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await); // epoch 1
    sc.checkout().await.unwrap();
    write(dir.path(), "f.txt", "v1");
    sc.run_barrier().await.unwrap();
    let loaded = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();

    // A successor deposes the cell to epoch 2.
    let state = store.epoch_read(&sc.cfg.epoch_key()).await.unwrap().unwrap();
    store.epoch_acquire(&sc.cfg.epoch_key(), "successor", Some(&state)).await.unwrap();

    let routes = super::gateway::routes(gw_core(&store));
    let mut doc = loaded.manifest.clone();
    doc.seq += 1;
    let body = serde_json::json!({
        "manifest": doc,
        "expected_etag": loaded.etag,
        "epoch": 1u64, // the deposed writer's claim
        "flush_uuid": "straggler",
    });
    let res = gw_req().method("POST").path("/lean/v1/proj1/manifest").json(&body).reply(&routes).await;
    assert_eq!(res.status(), 403, "stale epoch must be refused: {:?}", res.body());
    let after = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert_eq!(after.manifest.seq, loaded.manifest.seq, "the straggler CAS landed!");

    // The CURRENT epoch with the right token succeeds.
    let body = serde_json::json!({
        "manifest": doc,
        "expected_etag": loaded.etag,
        "epoch": 2u64,
        "flush_uuid": "successor",
    });
    let res = gw_req().method("POST").path("/lean/v1/proj1/manifest").json(&body).reply(&routes).await;
    assert_eq!(res.status(), 200, "{:?}", res.body());

    // And a CAS miss reports 409 with the current etag, never blind
    // re-seed semantics.
    let res = gw_req().method("POST").path("/lean/v1/proj1/manifest").json(&body).reply(&routes).await;
    assert_eq!(res.status(), 409);
}

/// status + snapshot surface the RPO/observability facts.
#[tokio::test]
async fn gateway_status_and_snapshot() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "f.txt", "v1");
    sc.run_barrier().await.unwrap();
    hitl_write(&store, &sc.cfg, "pending.txt", "queued", "dilip").await.unwrap();

    let routes = super::gateway::routes(gw_core(&store));
    let res = gw_req().method("GET").path("/lean/v1/proj1/status").reply(&routes).await;
    assert_eq!(res.status(), 200);
    let v: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(v["seq"], 1);
    assert_eq!(v["inbox_depth"], 1);
    assert_eq!(v["epoch"], 1);
    assert!(v["window"].is_null());

    let res = gw_req().method("GET").path("/lean/v1/proj1/snapshot").reply(&routes).await;
    assert_eq!(res.status(), 200);
    let v: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert!(v["manifest"]["entries"]["f.txt"].is_object());
    assert_eq!(v["inbox"]["entries"][0]["path"], "pending.txt");
}

/// The sync verb: begins with a scan; locally-dirty wins over a remote
/// delete (the review's steady-state destruction finding), locally-clean
/// applies remote adds/changes.
#[tokio::test]
async fn sync_scan_first_dirty_wins_clean_applies() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir_a.path(), "shared.txt", "v1");
    write(dir_a.path(), "mine.txt", "agent latest — NEVER destroy");
    a.run_barrier().await.unwrap();

    // Remote truth moves: a HITL edit of shared.txt lands in the inbox.
    hitl_write(&store, &a.cfg, "shared.txt", "user v2", "dilip").await.unwrap();
    // And the agent rewrites mine.txt AFTER the last barrier (un-scanned
    // latest work — sync must judge dirt by its OWN scan).
    write(dir_a.path(), "mine.txt", "agent latest v2");
    backdate_baseline(&a, "mine.txt");
    // Meanwhile someone removed mine.txt from the manifest remotely.
    let loaded = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    let mut theirs = loaded.manifest.clone();
    theirs.seq += 1;
    theirs.entries.remove("mine.txt");
    manifest::cas_write(store.as_ref(), &a.cfg, &theirs, Some(&loaded.etag), 0, "remote-delete")
        .await
        .unwrap();

    let r = a.sync().await.unwrap();

    // Clean path: the user's edit applied.
    assert_eq!(read(dir_a.path(), "shared.txt").unwrap(), "user v2");
    assert!(r.applied.contains(&"shared.txt".to_string()));
    // Dirty path: the remote delete did NOT destroy un-scanned work.
    assert_eq!(read(dir_a.path(), "mine.txt").unwrap(), "agent latest v2");
    assert!(r.conflicts.contains(&"mine.txt".to_string()));
    assert!(r.deleted.is_empty());
}

// ---------------------------------------------------------------------
// Boundary verbs (docs/plans/flint-lean-boundary-verbs-plan.md).
//
// Phase 0 — `.flint/` namespace reservation + capability marker + the
// pre-existing-data pre-flight (D0, D11).
// ---------------------------------------------------------------------

use super::control;
use super::sentinel::{Due, Verb};

fn touch_sentinel(root: &std::path::Path, name: &str, body: &str) {
    let dir = root.join(super::CONTROL_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), body).unwrap();
}

fn control_exists(root: &std::path::Path, name: &str) -> bool {
    root.join(super::CONTROL_DIR).join(name).exists()
}

/// D0.1 — the keystone. The RED form named in §5: before the scan
/// exclusion, `.flint/publish` is an ordinary regular file, so it gets
/// scanned and PUBLISHED to `<prefix>/files/.flint/publish` — the
/// sentinel is live ammunition on an old sidecar. This asserts the
/// hazard is gone: no `.flint/` key ever appears in the manifest or the
/// bucket, and the scan never yields the path.
#[tokio::test]
async fn flint_dir_never_scanned() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();

    write(dir.path(), "real.txt", "data");
    touch_sentinel(dir.path(), control::PUBLISH, "");
    touch_sentinel(dir.path(), control::REMOTE_SEQ, "{}");

    let scanned = super::scan::scan(dir.path()).unwrap();
    assert!(scanned.contains_key("real.txt"));
    assert!(
        !scanned.keys().any(|k| k.starts_with(".flint")),
        "the scan yielded a control path: {:?}",
        scanned.keys().collect::<Vec<_>>()
    );

    a.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("real.txt"));
    assert!(
        !m.entries.keys().any(|k| k.starts_with(".flint")),
        "a control path was CITED: {:?}",
        m.entries.keys().collect::<Vec<_>>()
    );
    assert!(store.head(&a.cfg.file_key(".flint/publish")).await.is_err());
}

/// D0.2 — an upgrade must never delete data. A workspace that legally
/// published `files/.flint/legacy.txt` under a pre-D0 sidecar has it in
/// the baseline; the new scan skips it, so the two-consecutive-scans
/// rule would otherwise classify it absent twice and publish its
/// DELETION. It is carried forward frozen.
#[tokio::test]
async fn legacy_flint_citation_survives_upgrade() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "keep.txt", "v1");
    a.run_barrier().await.unwrap();

    // Manufacture the pre-D0 state: a cited `.flint/` path in both the
    // manifest and our baseline, as an old sidecar would have left it.
    let key = a.cfg.file_key(".flint/legacy.txt");
    let body = Bytes::from_static(b"legacy payload");
    let crc = crc64_nvme(&body);
    let stamps = GenerationStamps {
        generation: 1,
        epoch: 0,
        flush_uuid: "legacy".into(),
        boundary_source: None,
        posix: None,
    };
    let meta = store
        .put_whole(&key, body, &PutCondition::IfNoneMatchAny, &stamps, crc)
        .await
        .unwrap();
    let loaded = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    let mut m = loaded.manifest.clone();
    m.seq += 1;
    m.entries.insert(
        ".flint/legacy.txt".into(),
        manifest::LeanEntry {
            key: key.clone(),
            etag: meta.etag.clone(),
            crc64_b64: meta.crc64_b64.clone(),
            size: meta.size,
            mode: 0o644,
            mtime_unix: 0,
            generation: 1,
            epoch: 0,
            version_id: meta.version_id.clone(),
        },
    );
    let installed =
        manifest::cas_write(store.as_ref(), &a.cfg, &m, Some(&loaded.etag), 0, "legacy")
            .await
            .unwrap();
    let mut b = a.state.load_baseline().unwrap();
    b.entries.insert(
        ".flint/legacy.txt".into(),
        super::state::BaselineEntry {
            etag: meta.etag.clone(),
            generation: 1,
            size: meta.size,
            mtime_unix: 0,
            version_id: None,
        },
    );
    b.inst_base.insert(".flint/legacy.txt".into(), meta.etag.clone());
    b.prev_scan.insert(".flint/legacy.txt".into());
    b.seq = m.seq;
    b.manifest_etag = Some(installed.etag.clone());
    a.state.save_baseline(&b).unwrap();

    // Anti-vacuity: the citation and the object genuinely exist first.
    assert!(store.head(&key).await.is_ok());

    // Two barriers — exactly what the two-scan delete rule needs.
    a.run_barrier().await.unwrap();
    a.run_barrier().await.unwrap();

    let after = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(
        after.entries.contains_key(".flint/legacy.txt"),
        "an upgrade DELETED a legacy citation"
    );
    assert!(store.head(&key).await.is_ok(), "an upgrade GC'd the legacy object");
}

/// D0.3 — a legacy `files/.flint/...` citation is never materialized
/// into the local control dir (it would collide with the control
/// files); it stays cited, with a conflict record naming it.
#[tokio::test]
async fn checkout_never_materializes_control_citation() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir_a.path(), "real.txt", "v1");
    a.run_barrier().await.unwrap();

    let key = a.cfg.file_key(".flint/legacy.txt");
    let body = Bytes::from_static(b"legacy");
    let crc = crc64_nvme(&body);
    let meta = store
        .put_whole(
            &key,
            body,
            &PutCondition::IfNoneMatchAny,
            &GenerationStamps {
                generation: 1,
                epoch: 0,
                flush_uuid: "legacy".into(),
                boundary_source: None,
                posix: None,
            },
            crc,
        )
        .await
        .unwrap();
    let loaded = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    let mut m = loaded.manifest.clone();
    m.seq += 1;
    m.entries.insert(
        ".flint/legacy.txt".into(),
        manifest::LeanEntry {
            key,
            etag: meta.etag.clone(),
            crc64_b64: meta.crc64_b64.clone(),
            size: meta.size,
            mode: 0o644,
            mtime_unix: 0,
            generation: 1,
            epoch: 0,
            version_id: meta.version_id.clone(),
        },
    );
    manifest::cas_write(store.as_ref(), &a.cfg, &m, Some(&loaded.etag), 0, "legacy")
        .await
        .unwrap();

    // A FRESH pod checks the same subtree out.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    let r = b.checkout().await.unwrap();
    assert_eq!(read(dir_b.path(), "real.txt").unwrap(), "v1");
    assert!(
        !dir_b.path().join(".flint/legacy.txt").exists(),
        "checkout materialized into the reserved control namespace"
    );
    assert_eq!(r.refused, 1);
    assert!(b
        .state
        .load_conflicts()
        .unwrap()
        .iter()
        .any(|c| c.path == ".flint/legacy.txt" && c.kind.starts_with("checkout-refused")));
}

/// D11 — the marker must be written on the LIVE-TREE restart row, not
/// only inside a fresh checkout. `checkout()` returns at
/// `marker_present()` without reaching its body, so a sidecar upgrade
/// over live workspaces would otherwise leave sentinels dead on exactly
/// the pods the upgrade targeted.
#[tokio::test]
async fn capabilities_written_on_live_tree_restart() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "f.txt", "v1");
    a.run_barrier().await.unwrap();

    // The pre-D11 state, constructed as what it actually is: a live
    // tree checked out by an OLD binary that never wrote a marker, onto
    // which the new image is dropped in place. (Checkout writes the
    // marker itself now — that is D11's other half — so simply calling
    // it cannot produce this state any more.)
    std::fs::remove_file(dir.path().join(super::CONTROL_DIR).join(control::CAPABILITIES))
        .unwrap();
    // Anti-vacuity: this IS the live-tree row — the marker is present,
    // so checkout returns early without reaching its body.
    assert!(a.state.marker_present());
    assert!(!control_exists(dir.path(), control::CAPABILITIES));
    let r = a.checkout().await.unwrap();
    assert!(r.resumed_live_tree);
    assert!(!control_exists(dir.path(), control::CAPABILITIES));

    // The startup write is what closes it.
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    let caps = a.read_capabilities().unwrap();
    assert_eq!(caps.protocol, super::SENTINEL_PROTOCOL);
    assert_eq!(caps.state, "live");
    assert!(caps.verbs.iter().any(|v| v == "publish"));
    assert!(caps.verbs.iter().any(|v| v == "sync"));
    assert_eq!(caps.boundary_mode, "hybrid");
}

/// D0.4 — reserving `.flint/` is a BREAKING change for a workspace
/// already using it as data: a file literally named `.flint/publish`
/// would be CONSUMED (renamed away) by the poll — a data grab from a
/// non-participating workspace. The pre-flight disables the verbs
/// instead, fleet-visibly, and the file is left byte-identical.
#[tokio::test]
async fn preexisting_flint_disables_sentinels() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    // The app owned `.flint/publish` BEFORE any protocol-aware sidecar
    // ran here — recorded bytes, per the drill's anti-vacuity rule.
    touch_sentinel(dir.path(), control::PUBLISH, "app-owned payload");
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);

    let posture = a.sentinel_preflight().unwrap();
    assert!(!posture.enabled);
    assert_eq!(posture.reason.as_deref(), Some("preexisting-flint-paths"));
    a.write_capabilities(&posture, false).unwrap();
    let caps = a.read_capabilities().unwrap();
    assert!(caps.verbs.is_empty());
    assert_eq!(caps.reason.as_deref(), Some("preexisting-flint-paths"));

    // The poll arm never arms: the file is NOT consumed, NOT published,
    // still present and byte-identical.
    a.checkout().await.unwrap();
    let acks = a.sentinel_tick().await.unwrap();
    assert!(acks.is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".flint/publish")).unwrap(),
        "app-owned payload"
    );
    assert!(a.load_pending(Verb::Publish).unwrap().is_none());

    // And the verdict is STICKY: a second startup, now that the marker
    // exists, must not silently re-enable what it disabled.
    let again = a.sentinel_preflight().unwrap();
    assert!(!again.enabled);
}

// ---------------------------------------------------------------------
// Phase 1 — publish sentinel, ack, coalescing, the work meter, the
// refused-fenced path (D1, D2, D3, D3.1, D12).
// ---------------------------------------------------------------------

/// Zero the min-interval clock so a test can honor back-to-back without
/// sleeping. (The interval itself is exercised by
/// `min_interval_coalesces_into_one_barrier`.)
fn clear_min_interval(sc: &Sidecar) {
    let mut b = sc.load_budget().unwrap();
    b.last_honor_unix = 0;
    let bytes = serde_json::to_vec(&b).unwrap();
    std::fs::write(sc.cfg.state_dir().join("sentinel-budget.json"), bytes).unwrap();
}

/// D1/D2 — the verb end to end: a sentinel publishes when cadence has
/// not, and the ack names the seq the honoring barrier installed.
#[tokio::test]
async fn publish_sentinel_honored_and_acked() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    a.checkout().await.unwrap();
    a.run_barrier().await.unwrap();
    let before = manifest::load(store.as_ref(), &a.cfg).await.unwrap().map(|l| l.manifest.seq);

    // The agent's logical change, then its declared coherent point.
    write(dir.path(), "model.json", "{}");
    write(dir.path(), "model.json.index", "idx");
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"n-1","note":"step 1"}"#);

    let acks = a.sentinel_tick().await.unwrap();
    assert_eq!(acks.len(), 1, "the sentinel was not honored");
    let ack = &acks[0];
    assert_eq!(ack.status, "ok");
    assert_eq!(ack.boundary, "sentinel");
    assert_eq!(ack.nonces, vec!["n-1".to_string()]);
    assert!(ack.sentinel_mtime_unix_ns > 0);

    // Both files of the logical change are cited by the SAME boundary.
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("model.json"));
    assert!(m.entries.contains_key("model.json.index"));
    assert_eq!(ack.seq, Some(m.seq));
    assert_ne!(before, Some(m.seq), "the manifest did not advance");

    // Consume/retire discipline: the sentinel is gone from the agent's
    // view, the pending record is retired, the ack is on disk.
    assert!(!control_exists(dir.path(), control::PUBLISH));
    assert!(a.load_pending(Verb::Publish).unwrap().is_none());
    assert!(control_exists(dir.path(), control::PUBLISH_ACK));
    assert_eq!(a.read_ack(Verb::Publish).unwrap().nonces, vec!["n-1".to_string()]);
}

/// D2 — the ack carries EVERY coalesced nonce. Under coalescing an
/// agent whose nonce rode behind a later touch would otherwise never
/// see it and would re-touch in a loop, feeding the storm the rate
/// limit exists to prevent.
///
/// Anti-vacuity: a MID-storm nonce (not the last) must appear.
#[tokio::test]
async fn sentinel_ack_echoes_covered_nonces() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    a.checkout().await.unwrap();

    write(dir.path(), "f.txt", "v1");
    for i in 0..5 {
        touch_sentinel(dir.path(), control::PUBLISH, &format!(r#"{{"nonce":"n-{i}"}}"#));
        // Consume only — honoring is held off by the min-interval below.
        a.poll_sentinels().unwrap();
    }
    let pending = a.load_pending(Verb::Publish).unwrap().unwrap();
    assert_eq!(pending.nonces.len(), 5, "touches did not coalesce into one record");

    clear_min_interval(&a);
    let ack = a.honor_pending(Verb::Publish, false).await.unwrap().unwrap();
    for i in 0..5 {
        assert!(
            ack.nonces.contains(&format!("n-{i}")),
            "nonce n-{i} was orphaned by coalescing: {:?}",
            ack.nonces
        );
    }
    // The mid-storm nonce specifically (the guard the drill leg names).
    assert!(ack.nonces.contains(&"n-2".to_string()));
}

/// D3 — the min-interval is a COALESCING window, not a drop: touches
/// inside it produce ONE barrier, and the ack covers every one of them.
#[tokio::test]
async fn min_interval_coalesces_into_one_barrier() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    a.cfg.sentinel_min_interval_secs = 3600; // the 1-hour-floor trick, applied to the interval
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    a.checkout().await.unwrap();

    write(dir.path(), "f.txt", "v1");
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"first"}"#);
    let acks = a.sentinel_tick().await.unwrap();
    assert_eq!(acks.len(), 1, "the first honor must be prompt");
    let seq_after_first = acks[0].seq.unwrap();

    // Inside the interval now: further touches consume but do NOT honor.
    write(dir.path(), "g.txt", "v1");
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"second"}"#);
    let acks = a.sentinel_tick().await.unwrap();
    assert!(acks.is_empty(), "the min-interval did not hold the second honor");
    assert_eq!(a.sentinel_due().unwrap(), Due::MinInterval);
    // Anti-vacuity: the touch WAS consumed — it is waiting, not lost.
    assert_eq!(
        a.load_pending(Verb::Publish).unwrap().unwrap().nonces,
        vec!["second".to_string()]
    );
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.seq, seq_after_first, "a held honor still advanced the manifest");

    // The floor tick picks it up: the boundary is honored by a REAL
    // barrier (contents are never thinned — D1's corollary).
    let out = a.floor_tick().await.unwrap();
    assert_eq!(out.acks.len(), 1);
    assert_eq!(out.acks[0].nonces, vec!["second".to_string()]);
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("g.txt"));
}

/// D3.1 — THE HOT-LOOPS NO-REGRESSION RULE. The budget meters work, not
/// calls. Red against a per-call counter: at the same touch rate, a
/// storm publishing an over-`whole_put_max` file must exhaust the
/// budget in ~2 honors while a small-file storm must not be throttled
/// at all.
#[tokio::test]
async fn budget_meters_bytes_not_calls() {
    // Arm (a): the large-file storm.
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    a.cfg.whole_put_max = 1024; // a small ceiling keeps the test fast
    a.cfg.sentinel_hourly_budget = 8; // ⇒ 2 honors of a 4 KiB file
    a.cfg.sentinel_min_interval_secs = 0;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    a.checkout().await.unwrap();

    let mut honors_large = 0;
    let mut deferred_acks = 0;
    for i in 0..6 {
        write(dir.path(), "big.bin", &"x".repeat(4096 + i));
        backdate_baseline(&a, "big.bin");
        touch_sentinel(dir.path(), control::PUBLISH, &format!(r#"{{"nonce":"L{i}"}}"#));
        let acks = a.sentinel_tick().await.unwrap();
        honors_large += acks.len();
        if acks.is_empty() && a.load_pending(Verb::Publish).unwrap().is_some() {
            // The budget held it: the floor tick honors it, deferred.
            let out = a.floor_tick().await.unwrap();
            deferred_acks += out
                .acks
                .iter()
                .filter(|x| x.boundary == "sentinel-deferred")
                .count();
        }
    }
    assert!(
        honors_large <= 3,
        "a 4 KiB-over-ceiling storm was NOT throttled: {honors_large} prompt honors"
    );
    assert!(deferred_acks >= 1, "no ack was stamped sentinel-deferred");
    assert_eq!(a.sentinel_due().unwrap(), Due::BudgetDeferred);

    // Arm (b): the SAME touch rate on a small file must NOT throttle.
    let store2 = Arc::new(MemoryStore::new());
    let dir2 = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store2, dir2.path()).await;
    b.cfg.whole_put_max = 1024;
    b.cfg.sentinel_hourly_budget = 8;
    b.cfg.sentinel_min_interval_secs = 0;
    assert!(claim_until_held(&mut b, 3).await);
    let posture = b.sentinel_preflight().unwrap();
    b.write_capabilities(&posture, false).unwrap();
    b.checkout().await.unwrap();

    let mut honors_small = 0;
    for i in 0..6 {
        write(dir2.path(), "small.txt", &format!("v{i}"));
        backdate_baseline(&b, "small.txt");
        touch_sentinel(dir2.path(), control::PUBLISH, &format!(r#"{{"nonce":"S{i}"}}"#));
        honors_small += b.sentinel_tick().await.unwrap().len();
    }
    assert_eq!(
        honors_small, 6,
        "the small-file storm was throttled — the meter is counting CALLS, not work"
    );
    assert!(b
        .read_ack(Verb::Publish)
        .map(|a| a.boundary == "sentinel")
        .unwrap_or(false));
    // The claim the whole rule rests on: same touch rate, different verdict.
    assert!(honors_small > honors_large);
}

/// D3.1's third consequence — a no-diff sentinel storm stays free: the
/// budget exists to bound WORK, and a no-diff honor does none. Its only
/// bound is the min-interval.
#[tokio::test]
async fn no_diff_sentinel_honor_costs_no_budget() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    a.cfg.sentinel_hourly_budget = 2;
    a.cfg.sentinel_min_interval_secs = 0;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    a.checkout().await.unwrap();
    write(dir.path(), "f.txt", "v1");
    a.run_barrier().await.unwrap();

    for i in 0..10 {
        touch_sentinel(dir.path(), control::PUBLISH, &format!(r#"{{"nonce":"q{i}"}}"#));
        let acks = a.sentinel_tick().await.unwrap();
        assert_eq!(acks.len(), 1, "a no-diff honor was throttled at touch {i}");
        assert!(acks[0].report.no_change, "the tree was not actually quiet");
    }
    assert_eq!(a.load_budget().unwrap().spent(super::now_unix()), 0);
}

/// D2's uniform crash rule. Pending-present-and-no-matching-ack is the
/// SAME observable state for crash-before-CAS and crash-after-step-7,
/// so acking from persisted state would assert publication of writes
/// that never uploaded. The rule: ALWAYS re-run a full barrier, and ack
/// with THAT barrier's install.
#[tokio::test]
async fn crash_between_consume_and_ack_reruns_barrier() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    a.checkout().await.unwrap();
    a.run_barrier().await.unwrap();
    let seq_before = a.state.load_baseline().unwrap().seq;

    // The crash shape: the sentinel was consumed, the barrier never ran.
    write(dir.path(), "late.txt", "written before the boundary");
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"crashed"}"#);
    a.poll_sentinels().unwrap();
    // Anti-vacuity: pending present, ack absent — the exact crash state.
    assert!(a.load_pending(Verb::Publish).unwrap().is_some());
    assert!(a.read_ack(Verb::Publish).is_none());

    // Restart.
    a.settle_pending_at_startup().await.unwrap();

    let ack = a.read_ack(Verb::Publish).unwrap();
    assert_eq!(ack.status, "ok");
    assert!(ack.nonces.contains(&"crashed".to_string()));
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    // The ack names the RE-RUN barrier's install, never the pre-crash
    // baseline seq — and the file that had never uploaded is cited.
    assert_eq!(ack.seq, Some(m.seq));
    assert!(m.seq > seq_before);
    assert!(m.entries.contains_key("late.txt"));
    assert!(a.load_pending(Verb::Publish).unwrap().is_none());
}

/// D2 settle-before-consume: a surviving pending must be honored, acked
/// and retired FIRST. A fresh consume that clobbered it would orphan its
/// nonces forever.
#[tokio::test]
async fn restart_settles_pending_before_new_consume() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    a.cfg.sentinel_min_interval_secs = 0;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    a.checkout().await.unwrap();

    write(dir.path(), "a.txt", "v1");
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"old"}"#);
    a.poll_sentinels().unwrap();
    // A NEW touch arrives while the old pending still stands.
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"new"}"#);
    a.poll_sentinels().unwrap();

    // The old nonce was not clobbered — it coalesced.
    let pending = a.load_pending(Verb::Publish).unwrap().unwrap();
    assert!(pending.nonces.contains(&"old".to_string()));
    assert!(pending.nonces.contains(&"new".to_string()));

    let ack = a.honor_pending(Verb::Publish, false).await.unwrap().unwrap();
    assert!(ack.nonces.contains(&"old".to_string()));
    assert!(ack.nonces.contains(&"new".to_string()));
}

/// D2's torn-body rule: an unparsable or oversize body is honored as a
/// bare-touch boundary with a warning conflict record — never a wedge,
/// never a silent drop.
#[tokio::test]
async fn torn_pending_body_honored_as_bare_touch() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    a.checkout().await.unwrap();

    write(dir.path(), "f.txt", "v1");
    // A plain open+write racing the consume rename leaves exactly this.
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"half-writ"#);
    let acks = a.sentinel_tick().await.unwrap();
    assert_eq!(acks.len(), 1, "a torn body wedged the verb");
    assert_eq!(acks[0].status, "ok");
    assert!(acks[0].nonces.is_empty());
    assert!(acks[0].sentinel_mtime_unix_ns > 0, "the bare-touch mtime must still be covered");
    assert!(a
        .state
        .load_conflicts()
        .unwrap()
        .iter()
        .any(|c| c.kind == "sentinel-torn-body"));
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("f.txt"));
}

/// A FIFO at the sentinel path would block the body read forever. Type
/// check first: skipped with a warning record, never consumed.
#[tokio::test]
#[cfg(unix)]
async fn fifo_sentinel_skipped() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    a.checkout().await.unwrap();

    let ctl = dir.path().join(super::CONTROL_DIR);
    std::fs::create_dir_all(&ctl).unwrap();
    let fifo = ctl.join(control::PUBLISH);
    let cpath = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a path we own, in a temp dir.
    let rc = unsafe { libc::mkfifo(cpath.as_ptr(), 0o644) };
    assert_eq!(rc, 0, "could not create the FIFO fixture");
    // Anti-vacuity: it really is a FIFO, and it really is at the
    // sentinel path.
    assert!(!std::fs::symlink_metadata(&fifo).unwrap().is_file());

    let acks = a.sentinel_tick().await.unwrap();
    assert!(acks.is_empty());
    assert!(a.load_pending(Verb::Publish).unwrap().is_none());
    assert!(fifo.exists(), "the FIFO was consumed");
    assert!(a
        .state
        .load_conflicts()
        .unwrap()
        .iter()
        .any(|c| c.kind == "sentinel-not-regular-file"));
}

/// D2's largest protocol hole in the draft: deposal stranded sentinels
/// forever. A fenced honor writes `refused-fenced` naming the observed
/// epoch, flips the marker to fenced with no verbs, and retires the
/// pending — the agent is answered, and stops touching a zombie.
#[tokio::test]
async fn fenced_honor_writes_refused_ack() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    a.checkout().await.unwrap();
    write(dir_a.path(), "f.txt", "v1");
    a.run_barrier().await.unwrap();

    // A pending sentinel stands...
    write(dir_a.path(), "g.txt", "v1");
    touch_sentinel(dir_a.path(), control::PUBLISH, r#"{"nonce":"stranded"}"#);
    a.poll_sentinels().unwrap();
    assert!(a.load_pending(Verb::Publish).unwrap().is_some());

    // ...and a successor deposes us. Anti-vacuity: the takeover is real.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    assert!(claim_until_held(&mut b, 12).await);
    let our_epoch = a.lease.as_ref().unwrap().epoch;
    let their_epoch = b.lease.as_ref().unwrap().epoch;
    assert!(their_epoch > our_epoch, "no takeover happened");

    let err = a.sentinel_tick().await.unwrap_err();
    assert!(matches!(err, LeanError::Fenced(_)));

    let ack = a.read_ack(Verb::Publish).unwrap();
    assert_eq!(ack.status, "refused-fenced");
    assert!(ack.nonces.contains(&"stranded".to_string()));
    assert_eq!(ack.observed_epoch, Some(their_epoch));
    assert!(ack.seq.is_none());
    assert!(a.load_pending(Verb::Publish).unwrap().is_none());

    // The marker stops the agent from touching a zombie.
    let caps = a.read_capabilities().unwrap();
    assert_eq!(caps.state, "fenced");
    assert!(caps.verbs.is_empty());
}

/// D2 — the in-loop SYNC honor must never apply the successor's
/// manifest onto a zombie tree. `Sidecar::sync` has no lease/epoch
/// check of its own, so before this tranche a straggler consuming a
/// sync sentinel between deposal and its next cooperative fence would
/// have done exactly that, and acked SUCCESS.
///
/// **Correction to D2's framing, recorded because the mutation matrix
/// found it:** removing `verify_not_deposed` from the sync honor does
/// NOT turn this test red — `honor_pending` renews the lease first
/// (D12) and the renew 412s on deposal, so the renew is the
/// load-bearing fence and the explicit check is the narrower guard for
/// the window between renew and apply. Both are kept; the test asserts
/// the property (tree unmutated, ack refused), and
/// `deposed_sidecar_fails_the_explicit_epoch_check` covers the guard
/// itself so it is not untested code.
#[tokio::test]
async fn fenced_sync_honor_refused_and_tree_unmutated() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    a.checkout().await.unwrap();
    write(dir_a.path(), "shared.txt", "zombie view");
    a.run_barrier().await.unwrap();

    // The successor takes over and publishes something the zombie's
    // sync WOULD apply if it ran.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    assert!(claim_until_held(&mut b, 12).await);
    b.checkout().await.unwrap();
    write(dir_b.path(), "shared.txt", "successor view");
    backdate_baseline(&b, "shared.txt");
    b.run_barrier().await.unwrap();

    let before = std::fs::read_to_string(dir_a.path().join("shared.txt")).unwrap();
    touch_sentinel(dir_a.path(), control::SYNC, r#"{"nonce":"zombie-sync"}"#);
    let err = a.sentinel_tick().await.unwrap_err();
    assert!(matches!(err, LeanError::Fenced(_)));

    let ack = a.read_ack(Verb::Sync).unwrap();
    assert_eq!(ack.status, "refused-fenced");
    assert_eq!(
        std::fs::read_to_string(dir_a.path().join("shared.txt")).unwrap(),
        before,
        "a fenced sync honor MUTATED the zombie's tree"
    );
    assert_ne!(before, "successor view", "the fixture never diverged");
}

/// The explicit guard of the previous test, isolated: on a deposed
/// sidecar the epoch check itself fences, independently of the renew.
#[tokio::test]
async fn deposed_sidecar_fails_the_explicit_epoch_check() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    // Anti-vacuity: it passes while we hold the lease.
    a.verify_not_deposed_pub().await.unwrap();

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    assert!(claim_until_held(&mut b, 12).await);
    assert!(b.lease.as_ref().unwrap().epoch > a.lease.as_ref().unwrap().epoch);

    assert!(matches!(
        a.verify_not_deposed_pub().await.unwrap_err(),
        LeanError::Fenced(_)
    ));
}

// ---------------------------------------------------------------------
// Phase 2 — sync sentinel + scope (D4) + remote.seq (D5) + write
// containment (§2.2 security gate).
// ---------------------------------------------------------------------

/// D4 — THE correctness rule, not an optimization. A scoped sync must
/// advance `inst_base` only for what it applied in scope. `inst_base`
/// is the three-way MERGE BASE: if a scoped sync advanced it wholesale
/// to bucket-current, `manifest::merge` would compute
/// `changed = base != theirs` as FALSE for every out-of-scope foreign
/// entry, never queue it, and the change would be silently lost from
/// the inbox flow forever.
///
/// The foreign change here is a MANIFEST install by another writer, not
/// a HITL inbox entry — a first draft of this test used an inbox entry
/// and passed even with the hazard reintroduced, because
/// `consume_inbox` integrates queued entries regardless of `inst_base`.
/// The loss only runs through the merge.
///
/// Anti-vacuity (the drill leg's three-part guard): the out-of-scope
/// change existed pre-sync, was absent at ack time, and is present
/// after the barriers that integrate it.
#[tokio::test]
async fn scoped_sync_preserves_out_of_scope_foreign_flow() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir_a.path(), "inputs/data.txt", "v1");
    write(dir_a.path(), "outputs/result.txt", "v1");
    a.run_barrier().await.unwrap();

    // A sibling writer installs new generations of BOTH paths directly
    // into the manifest.
    let loaded = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    let mut theirs = loaded.manifest.clone();
    theirs.seq += 1;
    for (path, content) in
        [("inputs/data.txt", "foreign inputs v2"), ("outputs/result.txt", "foreign outputs v2")]
    {
        let key = a.cfg.file_key(path);
        let body = Bytes::from(content.to_string());
        let crc = crc64_nvme(&body);
        let cur = store.head(&key).await.unwrap();
        let meta = store
            .put_whole(
                &key,
                body,
                &PutCondition::IfMatch(cur.etag),
                &GenerationStamps {
                    generation: 2,
                    epoch: 0,
                    flush_uuid: "sibling".into(),
                    boundary_source: None,
                    posix: None,
                },
                crc,
            )
            .await
            .unwrap();
        let e = theirs.entries.get_mut(path).unwrap();
        e.etag = meta.etag.clone();
        e.crc64_b64 = meta.crc64_b64.clone();
        e.size = meta.size;
        e.generation = 2;
    }
    manifest::cas_write(store.as_ref(), &a.cfg, &theirs, Some(&loaded.etag), 0, "sibling")
        .await
        .unwrap();
    let foreign_out_etag = theirs.entries["outputs/result.txt"].etag.clone();

    // (1) the out-of-scope change genuinely existed pre-sync.
    assert_ne!(
        foreign_out_etag,
        a.state.load_baseline().unwrap().inst_base["outputs/result.txt"]
    );

    let r = a.sync_scoped(Some(vec!["inputs/".into()])).await.unwrap();

    // In scope: applied now.
    assert_eq!(read(dir_a.path(), "inputs/data.txt").unwrap(), "foreign inputs v2");
    assert!(r.applied.contains(&"inputs/data.txt".to_string()));
    // (2) out of scope: absent at ack time, and COUNTED as deferred.
    assert_eq!(read(dir_a.path(), "outputs/result.txt").unwrap(), "v1");
    assert!(r.out_of_scope_foreign >= 1);
    // The merge base was NOT advanced for it — this is the whole rule.
    assert_ne!(
        a.state.load_baseline().unwrap().inst_base["outputs/result.txt"],
        foreign_out_etag,
        "a scoped sync advanced the MERGE BASE for an out-of-scope path"
    );
    // A scoped sync leaves seq/manifest_etag alone.
    assert_eq!(r.seq, a.state.load_baseline().unwrap().seq);

    // (3) present after the normal merge → inbox → consume flow: the
    // first barrier's merge queues it, the second consumes it.
    a.run_barrier().await.unwrap();
    a.run_barrier().await.unwrap();
    assert_eq!(
        read(dir_a.path(), "outputs/result.txt").unwrap(),
        "foreign outputs v2",
        "the out-of-scope foreign change was LOST from the inbox flow"
    );
}

/// §2.2 — scope matches on COMPONENT boundaries. `"in"` must never
/// match `internal/`.
#[test]
fn scope_matches_on_component_boundary() {
    let s = super::sync::Scope::new(&["in".to_string(), "inputs/".to_string()]);
    assert!(s.covers("in"));
    assert!(s.covers("in/x.txt"));
    assert!(s.covers("inputs/a.txt"));
    assert!(!s.covers("internal/secret.txt"));
    assert!(!s.covers("inputsX/a.txt"));
    // `..` and absolute entries are dropped, not honored.
    let s = super::sync::Scope::new(&["../etc".to_string()]);
    assert!(s.is_empty());
}

/// §2.2 — the write path becomes containment-safe. The shipped
/// `write_file_atomic` did `create_dir_all(parent)` + write with no
/// `O_NOFOLLOW` and no root check, while the scanner SKIPS symlinks — so
/// an unprivileged app that plants `inputs -> /root/.aws`, lands an
/// object at `inputs/<path>` and drops a scoped sync turns the
/// credential-holding sidecar into an arbitrary-file-write primitive
/// outside the workspace.
#[tokio::test]
#[cfg(unix)]
async fn write_file_atomic_refuses_symlink_escape() {
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "ORIGINAL CREDENTIALS").unwrap();

    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();

    // The planted symlink. Anti-vacuity: it really escapes the root,
    // and the scanner really cannot see it.
    std::os::unix::fs::symlink(outside.path(), dir.path().join("inputs")).unwrap();
    assert!(dir.path().join("inputs/secret.txt").exists());
    let scanned = super::scan::scan(dir.path()).unwrap();
    assert!(!scanned.keys().any(|k| k.starts_with("inputs")));

    // Every workspace write path refuses it.
    assert!(super::barrier::contained_path(dir.path(), "inputs/secret.txt").is_err());
    assert!(super::barrier::contained_path(dir.path(), "../escape.txt").is_err());
    assert!(super::barrier::contained_path(dir.path(), "/etc/passwd").is_err());
    assert!(super::barrier::contained_path(dir.path(), ".flint/publish").is_err());
    // A legitimate nested path still works.
    assert!(super::barrier::contained_path(dir.path(), "ok/nested/f.txt").is_ok());

    // And the end-to-end shape: a foreign object at the planted path is
    // surfaced as a conflict, never written through the symlink.
    hitl_write(&store, &a.cfg, "inputs/secret.txt", "ATTACKER PAYLOAD", "attacker")
        .await
        .unwrap();
    a.run_barrier().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&secret).unwrap(),
        "ORIGINAL CREDENTIALS",
        "the sidecar wrote THROUGH a planted symlink, outside the workspace"
    );
    assert!(a
        .state
        .load_conflicts()
        .unwrap()
        .iter()
        .any(|c| c.kind.starts_with("consume-refused-containment")));
}

/// §2.2's phantom-conflict rule. `sync` saves the baseline only at the
/// end, so a crash mid-apply followed by a re-honor makes already-
/// applied paths scan dirty against the stale baseline: the ack would
/// report a conflict for a path whose local bytes ARE the remote bytes,
/// and the path would then re-publish as a spurious generation bump.
#[tokio::test]
async fn sync_rehonor_no_phantom_conflicts() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "shared.txt", "v1");
    a.run_barrier().await.unwrap();

    // A foreign change lands and IS applied...
    hitl_write(&store, &a.cfg, "shared.txt", "foreign v2", "ci").await.unwrap();
    let r = a.sync().await.unwrap();
    assert!(r.applied.contains(&"shared.txt".to_string()));

    // ...but the crash shape: `sync` saves the baseline only at the end,
    // so a crash mid-apply leaves the WHOLE entry stale — etag included.
    // The path now scans DIRTY against that stale baseline while its
    // bytes are byte-identical to the remote's.
    let mut b = a.state.load_baseline().unwrap();
    let stale = b.entries.get_mut("shared.txt").unwrap();
    stale.etag = "\"stale-pre-sync-etag\"".into();
    stale.mtime_unix -= 10;
    stale.size = 1;
    a.state.save_baseline(&b).unwrap();
    let scanned = super::scan::scan(dir.path()).unwrap();
    let c = super::scan::classify(&scanned, &a.state.load_baseline().unwrap());
    assert!(c.uploads.contains("shared.txt"), "the fixture is not actually dirty");

    let r = a.sync().await.unwrap();
    assert!(
        !r.conflicts.contains(&"shared.txt".to_string()),
        "a phantom conflict was reported for byte-identical content"
    );
    assert!(r.applied.contains(&"shared.txt".to_string()));
}

/// D5 — the news ticker is fed from information the barrier already
/// has: ZERO added bucket requests. `updated_unix` heartbeats on every
/// tick (so an agent can tell "no news" from "sidecar dead");
/// `observed_seq` moves only when it moves.
#[tokio::test]
async fn remote_seq_ticks_without_added_requests() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir_a.path(), "f.txt", "v1");
    a.floor_tick().await.unwrap();

    let t0 = a.load_remote_seq();
    assert!(t0.updated_unix > 0);
    assert_eq!(t0.observed_seq, t0.integrated_seq, "no news, yet the ticker claims some");

    // A foreign install advances the bucket.
    let loaded = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    let mut theirs = loaded.manifest.clone();
    theirs.seq += 1;
    manifest::cas_write(store.as_ref(), &a.cfg, &theirs, Some(&loaded.etag), 0, "foreign")
        .await
        .unwrap();

    // Anti-vacuity: the ticker had NOT moved before the foreign install.
    assert_eq!(t0.observed_seq, loaded.manifest.seq);

    a.floor_tick().await.unwrap();
    let t1 = a.load_remote_seq();
    assert!(
        t1.observed_seq >= theirs.seq,
        "the ticker missed a foreign install: {} vs {}",
        t1.observed_seq,
        theirs.seq
    );
    assert!(t1.updated_unix >= t0.updated_unix);
}

/// D14 — a gateway sync request is CARRIED, never executed. The
/// asymmetry with a boundary request is blast radius: a boundary
/// publishes what is already on disk and touches no local file, whereas
/// `sync` re-derives the tree against the current remote manifest and
/// DELETES local files for remotely-deleted paths.
///
/// Failing control, house style: the tree hash is taken before and
/// after, and the test FAILS if the sidecar mutated anything.
#[tokio::test]
async fn sync_request_is_carried_never_executed() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "keep.txt", "agent bytes");
    a.run_barrier().await.unwrap();

    // Remote truth diverges in a way a sync WOULD apply (a deletion —
    // the destructive half of the verb).
    let loaded = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    let mut theirs = loaded.manifest.clone();
    theirs.seq += 1;
    theirs.entries.remove("keep.txt");
    manifest::cas_write(store.as_ref(), &a.cfg, &theirs, Some(&loaded.etag), 0, "remote-delete")
        .await
        .unwrap();

    let tree_before = std::fs::read_to_string(dir.path().join("keep.txt")).unwrap();
    a.carry_sync_request(super::now_unix(), "ci@example").unwrap();
    // Several ticks: the sidecar must move the ticker and NOTHING else.
    for _ in 0..3 {
        let _ = a.sentinel_tick().await.unwrap();
    }
    let t = a.load_remote_seq();
    assert_eq!(t.sync_requested_by.as_deref(), Some("ci@example"));
    assert!(t.sync_requested_unix.is_some());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("keep.txt")).unwrap(),
        tree_before,
        "the sidecar acted on a remote's sync request"
    );

    // The agent's OWN touch is what performs it — and then the request
    // is stale and self-clears.
    touch_sentinel(dir.path(), control::SYNC, r#"{"nonce":"mine"}"#);
    let acks = a.sentinel_tick().await.unwrap();
    assert_eq!(acks.len(), 1);
    assert!(!dir.path().join("keep.txt").exists(), "the agent's own sync did not run");
}

// ---------------------------------------------------------------------
// Phase 3 — gated advance (D6, D7, D8, D13) + the flint-store version
// surface.
// ---------------------------------------------------------------------

use super::gated::CitationSource;

fn gated(sc: &mut Sidecar) {
    sc.cfg.boundary_mode = super::BoundaryMode::Gated;
    sc.cfg.visibility_lag_bound_secs = Some(3600); // the 1-hour-floor trick
    sc.cfg.quiesce_bound_secs = 3600;
}

/// D7 — the premise the whole switch rests on: on a versioned bucket an
/// in-place PUT DESTROYS NOTHING. The cited generation survives as a
/// version and stays byte-fetchable by id.
#[tokio::test]
async fn staging_put_never_destroys_the_cited_version() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "model.json", "BOUNDARY 1");
    a.run_barrier().await.unwrap();

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let cited = m.entries["model.json"].clone();
    let cited_vid = cited.version_id.clone().expect("a versioned store cites a version");

    // Now stage a new generation in place.
    gated(&mut a);
    write(dir.path(), "model.json", "MID-LOGICAL-CHANGE");
    backdate_baseline(&a, "model.json");
    let lane = a.upload_lane().await.unwrap();
    assert_eq!(lane.staged, vec!["model.json".to_string()]);

    // Anti-vacuity: the staging PUT really moved `current`.
    let key = a.cfg.file_key("model.json");
    let (_, current) = store.get_whole(&key, None).await.unwrap();
    assert_eq!(&current[..], b"MID-LOGICAL-CHANGE");
    // Visibility withheld: the manifest has NOT advanced.
    let m2 = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m2.seq, m.seq, "the upload lane advanced the manifest");
    // Durability without destruction: the CITED version is intact.
    let (_, still) = store.get_version(&key, &cited_vid).await.unwrap();
    assert_eq!(&still[..], b"BOUNDARY 1", "an in-place staging PUT destroyed cited data");
}

/// D13 — the rule the design is INCOHERENT without. Red against the
/// shipped S3-wins arm: with staged (uncited) bytes current, a gated
/// checkout must materialize the cited version, never adopt the
/// mid-logical-change current one.
///
/// This is leg B9's core: the probe checkout must COMPLETE and
/// materialize exactly the pre-boundary cited set.
#[tokio::test]
async fn pinned_reads_never_adopts_current() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);

    // Boundary 1: both files of a logical change, cited together.
    write(dir_a.path(), "model.json", "A1");
    write(dir_a.path(), "model.json.index", "B1");
    a.upload_lane().await.unwrap();
    let cite = a.citation_pass(CitationSource::Sentinel).await.unwrap();
    assert_eq!(cite.cited, 2);
    let cited = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(cited.pinned_reads, "a gated citation did not stamp pinned_reads");
    assert_eq!(cited.boundary_source.as_deref(), Some("sentinel"));

    // Now write A2 and stage it — the mid-logical-change state: A is
    // new, B is not yet written.
    write(dir_a.path(), "model.json", "A2-MID-CHANGE");
    backdate_baseline(&a, "model.json");
    a.upload_lane().await.unwrap();

    // Anti-vacuity, both halves of the drill leg's guard:
    // (1) the uncited version EXISTS and is CURRENT at the real key...
    let key = a.cfg.file_key("model.json");
    let (_, raw) = store.get_whole(&key, None).await.unwrap();
    assert_eq!(&raw[..], b"A2-MID-CHANGE", "nothing was actually staged");
    // (2) ...while the manifest seq is unchanged.
    let now = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(now.seq, cited.seq);

    // A second pod checks out between the staging and the boundary.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    let r = b.checkout().await.unwrap();
    // The probe checkout must COMPLETE (a wedged probe fails the leg).
    assert_eq!(r.materialized, 2);
    assert_eq!(
        read(dir_b.path(), "model.json").unwrap(),
        "A1",
        "a gated checkout S3-WINS-ADOPTED its own staged bytes"
    );
    assert_eq!(read(dir_b.path(), "model.json.index").unwrap(), "B1");
}

/// §3 residual 11, as a REQUIRED-REACHABLE probe rather than an
/// assumption. The raw-key exposure is real and permanent: a reader
/// that does not resolve through the manifest sees mid-logical-change
/// bytes. Proving invisibility without proving this would be proving a
/// guarantee by the attack's absence — the house rule forbids it.
#[tokio::test]
async fn raw_key_reader_sees_uncited_bytes() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);
    write(dir.path(), "f.txt", "CITED");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();

    write(dir.path(), "f.txt", "UNCITED");
    backdate_baseline(&a, "f.txt");
    a.upload_lane().await.unwrap();

    // The documented exposure, demonstrated: `aws s3 cp` equivalent.
    let (_, raw) = store.get_whole(&a.cfg.file_key("f.txt"), None).await.unwrap();
    assert_eq!(&raw[..], b"UNCITED");
    // And the guarantee that IS promised still holds for a
    // manifest-resolving reader.
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let vid = m.entries["f.txt"].version_id.clone().unwrap();
    let (_, cited) = store.get_version(&a.cfg.file_key("f.txt"), &vid).await.unwrap();
    assert_eq!(&cited[..], b"CITED");
}

/// §2.4.1's stage-diff base. Diffing against the baseline ALONE — which
/// gated mode leaves un-advanced until citation — would re-PUT every
/// staged-but-quiet file on every tick: 50 quiet files × 60 ticks =
/// 3,000 PUTs/hour vs today's 50, which would falsify the plan's own
/// economics. The base is baseline ∪ PENDING.
#[tokio::test]
async fn quiet_staged_file_is_not_restaged() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);

    write(dir.path(), "quiet.txt", "v1");
    let first = a.upload_lane().await.unwrap();
    assert_eq!(first.staged, vec!["quiet.txt".to_string()], "nothing staged on the first tick");

    for tick in 0..5 {
        let r = a.upload_lane().await.unwrap();
        assert!(
            r.staged.is_empty(),
            "tick {tick} re-staged a quiet file: the stage-diff base is the baseline alone"
        );
    }
    // One version, not six.
    assert_eq!(store.version_count(&a.cfg.file_key("quiet.txt")), 1);
}

/// §2.4.1 — the citation lane owns the window; the upload lane opens
/// NONE. A lane with no CAS that opened a 180 s-deadline window every
/// 60 s would refuse HITL admission essentially forever between
/// citations, and it would protect nothing.
#[tokio::test]
async fn hitl_admitted_between_citations() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);
    write(dir.path(), "seed.txt", "v1");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();

    for i in 0..3 {
        write(dir.path(), &format!("churn{i}.txt"), "x");
        let lane = a.upload_lane().await.unwrap();
        // Anti-vacuity: the lane really ran and really staged.
        assert!(!lane.staged.is_empty(), "no lane tick happened at {i}");
        // No citation is due (1-hour caps), so this is squarely
        // between citations.
        assert!(a.citation_due(false).unwrap().is_none());
        hitl_write(&store, &a.cfg, &format!("hitl{i}.txt"), "user bytes", "dilip")
            .await
            .unwrap_or_else(|e| panic!("HITL refused between citations at tick {i}: {e}"));
    }
}

/// §2.4.1 — quiescence must actually FIRE. The draft's definition ("no
/// scan diff vs baseline") could never fire once anything was pending,
/// since pending paths stay classified-changed until citation:
/// quiescence would have been dead code and every citation would ride
/// the lag cap. The definition is scan-to-scan STABILITY.
///
/// The leg FAILS if the source reads `forced-lag-cap` — the exact dead
/// -code shape the finding named.
#[tokio::test]
async fn quiescence_fires_and_is_not_the_lag_cap() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);
    a.cfg.quiesce_bound_secs = 0; // fire as soon as stability is observed
    a.cfg.visibility_lag_bound_secs = Some(3600); // the cap CANNOT be the cause

    write(dir.path(), "one.txt", "written once, then silence");
    a.upload_lane().await.unwrap();
    // The first tick establishes the fingerprint; the second observes
    // it unchanged — that is the stability the rule is about.
    assert!(a.citation_due(false).unwrap().is_none(), "quiescence fired before any stability");
    a.upload_lane().await.unwrap();

    let source = a.citation_due(false).unwrap();
    assert_eq!(
        source,
        Some(CitationSource::Quiescence),
        "quiescence did not fire — got {source:?} (dead-code shape if forced-lag-cap)"
    );
    let r = a.citation_pass(source.unwrap()).await.unwrap();
    assert_eq!(r.source.as_deref(), Some("quiescence"));
    // And the source is readable FROM THE BUCKET alone, by HEAD.
    let head = store.head(&a.cfg.manifest_key()).await.unwrap();
    let stamps = GenerationStamps::from_meta(&head.meta).unwrap();
    assert_eq!(stamps.boundary_source.as_deref(), Some("quiescence"));
}

/// §2.4.1 — the lag cap forces a citation even mid-change, and stamps
/// its provenance so a downstream consumer can tell a declared-coherent
/// boundary from a forced possibly-torn one.
///
/// Anti-vacuity: the writer must actually be writing during every
/// quiesce window — otherwise quiescence fired and the leg proved
/// nothing about the cap.
#[tokio::test]
async fn lag_cap_forces_citation_and_stamps_the_source() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);
    a.cfg.visibility_lag_bound_secs = Some(0); // the cap is due immediately
    a.cfg.quiesce_bound_secs = 3600; // quiescence CANNOT be the cause

    let mut wrote = 0;
    for i in 0..3 {
        write(dir.path(), &format!("hot{i}.txt"), &format!("v{i}"));
        wrote += 1;
        a.upload_lane().await.unwrap();
    }
    assert_eq!(wrote, 3, "the writer was not writing");
    let source = a.citation_due(false).unwrap();
    assert_eq!(source, Some(CitationSource::ForcedLagCap));
    let r = a.citation_pass(source.unwrap()).await.unwrap();
    assert_eq!(r.source.as_deref(), Some("forced-lag-cap"));
    let head = store.head(&a.cfg.manifest_key()).await.unwrap();
    assert_eq!(
        GenerationStamps::from_meta(&head.meta).unwrap().boundary_source.as_deref(),
        Some("forced-lag-cap"),
        "the forced source is not readable from the bucket manifest meta alone"
    );
}

/// D8 — EXACT version reclamation is flint's job; lifecycle is only the
/// backstop. Steady-state churn ⇒ after each citation the key holds ONE
/// live version.
///
/// Anti-vacuity (the drill leg's guard): the key must carry more than
/// one version mid-leg before asserting it drains, and the retention
/// backstop must NOT be creditable for the work.
#[tokio::test]
async fn version_reclamation_returns_to_one_per_key() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);
    let key = a.cfg.file_key("churn.txt");

    write(dir.path(), "churn.txt", "g1");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();
    assert_eq!(store.version_count(&key), 1);

    // Churn across SEPARATE citations so the cited version of each
    // becomes noncurrent under the next.
    for g in 2..6 {
        write(dir.path(), "churn.txt", &format!("g{g}"));
        backdate_baseline(&a, "churn.txt");
        a.upload_lane().await.unwrap();
        // Mid-leg the key genuinely carries more than one version.
        assert!(
            store.version_count(&key) >= 2,
            "generation {g} did not create a second version"
        );
        a.citation_pass(CitationSource::Sentinel).await.unwrap();
        assert_eq!(
            store.version_count(&key),
            1,
            "exact per-citation GC did not drain generation {g}"
        );
    }
    // The backstop could not have done it: nothing is old enough.
    assert!(store.expire_noncurrent(86_400).is_empty());
}

/// D8's abandoned-mid-stage endgame — stated in the plan because it is
/// WORSE than the copy design's. Gated staging makes the CITED version
/// noncurrent, so the noncurrent backstop runs a clock against live
/// cited data. When it reaps one, the manifest dangles and checkout
/// must REFUSE loudly rather than serve a hole.
///
/// Anti-vacuity: the checkout must genuinely fail on the dangling
/// citation before any recovery claim is made.
#[tokio::test]
async fn dangling_citation_refuses_rather_than_serving_a_hole() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);
    write(dir.path(), "abandoned.txt", "CITED WORK");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();

    // Stage newer work and then abandon the workspace: the cited
    // version is now NONCURRENT under the uncited current one.
    write(dir.path(), "abandoned.txt", "UNCITED WORK");
    backdate_baseline(&a, "abandoned.txt");
    a.upload_lane().await.unwrap();
    let key = a.cfg.file_key("abandoned.txt");
    assert_eq!(store.version_count(&key), 2);

    // The backstop fires past retention (shortened to seconds on the
    // rig) and reaps the CITED noncurrent version — the inversion.
    let reaped = store.expire_noncurrent(0);
    assert!(
        reaped.iter().any(|(k, _)| k == &key),
        "the backstop reaped nothing — the endgame fixture never armed"
    );

    // A fresh pod's checkout now dangles, and must refuse.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    let err = b.checkout().await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("recover-staged") && msg.contains("version"),
        "checkout did not refuse loudly on a dangling citation: {msg}"
    );
    assert!(
        !dir_b.path().join("abandoned.txt").exists(),
        "checkout served a partial tree past a dangling citation"
    );
    // The bytes are NOT lost: the surviving current version is the
    // newer work, which `recover-staged` re-cites FORWARD.
    let (_, survivor) = store.get_whole(&key, None).await.unwrap();
    assert_eq!(&survivor[..], b"UNCITED WORK");
}

/// D8's conformance probe: a backend that cannot express the version
/// surface must be REFUSED, never silently degraded into etag
/// semantics on a key whose current version is uncited — which is
/// precisely the torn view gated mode exists to prevent.
#[tokio::test]
async fn versioning_conformance_probe_passes_on_a_versioned_store() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.versioning_conformance().await.expect("a versioned store must pass the probe");
}

// ---------------------------------------------------------------------
// Phase 3 remainder — the run-loop wiring, the conformance wedge, and
// `recover-staged` (D9). Up to here `gated.rs` was reachable only from
// tests: `boundaryMode: gated` was a knob that parsed, validated, and
// then ran the fused cadence barrier like every other mode.
// ---------------------------------------------------------------------

/// D6 — the mode's whole content, at the arm that actually runs in
/// production. A gated floor tick must make bytes DURABLE and leave
/// them INVISIBLE; the shipped loop ran `run_barrier`, which cites.
#[tokio::test]
async fn gated_floor_tick_stages_without_citing() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);

    write(dir.path(), "model.json", "MID-LOGICAL-CHANGE");
    let out = a.floor_tick().await.unwrap();

    // Durability: RPO for bytes stays at the floor.
    let key = a.cfg.file_key("model.json");
    let (_, current) = store.get_whole(&key, None).await.unwrap();
    assert_eq!(&current[..], b"MID-LOGICAL-CHANGE", "the gated floor tick published nothing");
    assert!(
        a.load_stage().unwrap().entries.contains_key("model.json"),
        "the floor tick did not run the upload lane"
    );
    // Visibility: withheld until a coherent point.
    let cited = manifest::load(store.as_ref(), &a.cfg)
        .await
        .unwrap()
        .map(|l| l.manifest.entries.contains_key("model.json"))
        .unwrap_or(false);
    assert!(!cited, "the gated floor tick advanced the manifest — visibility is not gated");
    assert_eq!(out.seq, None, "a staging-only tick reported a citation seq");
}

/// Anti-vacuity for the leg above: the SAME floor tick, with the lag
/// cap due, must cite — otherwise "did not cite" would be proving that
/// gated mode does nothing at all.
#[tokio::test]
async fn gated_floor_tick_cites_at_the_lag_cap() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);
    a.cfg.visibility_lag_bound_secs = Some(0); // the cap is always due

    write(dir.path(), "model.json", "COHERENT");
    let out = a.floor_tick().await.unwrap();

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("model.json"), "the lag cap did not force a citation");
    assert!(m.pinned_reads, "a gated citation did not stamp pinned_reads");
    assert_eq!(m.boundary_source.as_deref(), Some("forced-lag-cap"));
    assert_eq!(out.seq, Some(m.seq));
    assert!(
        a.load_stage().unwrap().entries.is_empty(),
        "the citation left the staged set uncleared"
    );
}

/// D1 × D6 — a publish sentinel in gated mode is a CITATION SOURCE, not
/// a fused barrier. The ack must name the installed boundary, and the
/// manifest must carry the sentinel stamp.
#[tokio::test]
async fn gated_publish_sentinel_cites_the_whole_stage() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);

    // Two halves of one logical change, staged across SEPARATE lane
    // passes — the citation must install both in one boundary.
    write(dir.path(), "model.json", "A1");
    a.upload_lane().await.unwrap();
    write(dir.path(), "model.json.index", "B1");
    a.upload_lane().await.unwrap();
    assert_eq!(a.load_stage().unwrap().entries.len(), 2);

    touch_sentinel(dir.path(), super::control::PUBLISH, r#"{"nonce":"n-1"}"#);
    let acks = a.sentinel_tick().await.unwrap();
    assert_eq!(acks.len(), 1, "the sentinel was not honored");
    assert_eq!(acks[0].status, "ok");
    assert_eq!(acks[0].boundary, "sentinel");

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("model.json") && m.entries.contains_key("model.json.index"));
    assert_eq!(m.boundary_source.as_deref(), Some("sentinel"));
    assert!(m.pinned_reads);
    assert_eq!(acks[0].seq, Some(m.seq), "the ack did not name the boundary it installed");
}

/// D10 rule 1 × D6 — the preStop drain cites everything staged, as one
/// flagged boundary, WITHOUT moving data: the versions already exist.
/// The shipped drain ran the fused barrier, so the final boundary of a
/// gated workspace's life carried neither the `drain` stamp nor
/// `pinned_reads`, and left the pending record naming versions that had
/// already been cited by another route.
#[tokio::test]
async fn gated_drain_cites_the_staged_versions_in_place() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);

    write(dir.path(), "a.txt", "A1");
    write(dir.path(), "b.txt", "B1");
    a.upload_lane().await.unwrap();
    let stage = a.load_stage().unwrap();
    let staged_a = stage.entries["a.txt"].version_id.clone().unwrap();
    let staged_b = stage.entries["b.txt"].version_id.clone().unwrap();

    a.drain().await.unwrap();

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.boundary_source.as_deref(), Some("drain"));
    assert!(m.pinned_reads, "the last boundary of a gated workspace was not pinned");
    assert_eq!(
        m.entries["a.txt"].version_id.as_deref(),
        Some(staged_a.as_str()),
        "the drain cited a version the lane had not staged — it moved data"
    );
    assert_eq!(m.entries["b.txt"].version_id.as_deref(), Some(staged_b.as_str()));
    assert!(
        a.load_stage().unwrap().entries.is_empty(),
        "the drain left staged work in the pending record"
    );
}

/// The conformance probe's own crash window. The probe writes its
/// object `If-None-Match: *` and deletes both versions at the end; a
/// crash (or one failed cleanup DELETE) in between leaves the object
/// behind, and every subsequent probe then 412s on its FIRST write and
/// refuses. On a gated workspace that is a permanent startup wedge from
/// a transient error — the probe must clean up after itself.
#[tokio::test]
async fn versioning_conformance_survives_a_leftover_probe_object() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);

    // Exactly what a crashed probe leaves behind.
    let key = format!("{}/{}/probe/versioning", a.cfg.prefix, super::LEAN_DIR);
    let stamps = GenerationStamps {
        generation: 0,
        epoch: 0,
        flush_uuid: "leftover".into(),
        boundary_source: None,
        posix: None,
    };
    store
        .put_whole(
            &key,
            Bytes::from_static(b"crashed-probe"),
            &PutCondition::IfNoneMatchAny,
            &stamps,
            crc64_nvme(b"crashed-probe"),
        )
        .await
        .unwrap();

    a.versioning_conformance()
        .await
        .expect("a leftover probe object wedged the conformance probe");
    // And it left nothing behind for the next run either.
    assert!(store.get_whole(&key, None).await.is_err(), "the probe did not clean up");
}

// A backend that answers every version-scoped call but strips
// `x-amz-version-id` from PUT responses: the project-scoped proxy the
// plan says must be REFUSED rather than silently degraded into etag
// semantics on a key whose current version is uncited (leg B24's
// control arm).
struct VersionStripping(Arc<MemoryStore>);

#[async_trait::async_trait]
impl ObjectStore for VersionStripping {
    async fn put_whole(
        &self,
        key: &str,
        body: Bytes,
        cond: &PutCondition,
        stamps: &GenerationStamps,
        crc: u64,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        let mut m = self.0.put_whole(key, body, cond, stamps, crc).await?;
        m.version_id = None;
        Ok(m)
    }
    async fn compose_generation(
        &self,
        spec: &flint_store::ComposeSpec<'_>,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.0.compose_generation(spec).await
    }
    async fn head(&self, key: &str) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.0.head(key).await
    }
    async fn get_whole(
        &self,
        key: &str,
        if_match: Option<&str>,
    ) -> flint_store::StoreResult<(flint_store::ObjectMeta, Bytes)> {
        self.0.get_whole(key, if_match).await
    }
    async fn get_range(
        &self,
        key: &str,
        off: u64,
        len: u64,
        if_match: &str,
    ) -> flint_store::StoreResult<Bytes> {
        self.0.get_range(key, off, len, if_match).await
    }
    fn min_part_size(&self) -> u64 {
        self.0.min_part_size()
    }
    fn max_parts(&self) -> usize {
        self.0.max_parts()
    }
    async fn list(&self, prefix: &str) -> flint_store::StoreResult<Vec<flint_store::ListedObject>> {
        self.0.list(prefix).await
    }
    async fn delete(&self, key: &str) -> flint_store::StoreResult<()> {
        self.0.delete(key).await
    }
    async fn head_version(
        &self,
        key: &str,
        v: &str,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.0.head_version(key, v).await
    }
    async fn get_version(
        &self,
        key: &str,
        v: &str,
    ) -> flint_store::StoreResult<(flint_store::ObjectMeta, Bytes)> {
        self.0.get_version(key, v).await
    }
    async fn delete_version(&self, key: &str, v: &str) -> flint_store::StoreResult<()> {
        self.0.delete_version(key, v).await
    }
    async fn list_versions(
        &self,
        prefix: &str,
    ) -> flint_store::StoreResult<Vec<flint_store::ListedVersion>> {
        self.0.list_versions(prefix).await
    }
    async fn list_uploads(
        &self,
        prefix: &str,
    ) -> flint_store::StoreResult<Vec<flint_store::PendingUpload>> {
        self.0.list_uploads(prefix).await
    }
    async fn abort_upload(&self, key: &str, id: &str) -> flint_store::StoreResult<()> {
        self.0.abort_upload(key, id).await
    }
    async fn bootstrap(
        &self,
        prefix: &str,
    ) -> flint_store::StoreResult<flint_store::BootstrapReport> {
        self.0.bootstrap(prefix).await
    }
    async fn epoch_read(
        &self,
        key: &str,
    ) -> flint_store::StoreResult<Option<flint_store::EpochState>> {
        self.0.epoch_read(key).await
    }
    async fn epoch_acquire(
        &self,
        key: &str,
        holder: &str,
        observed: Option<&flint_store::EpochState>,
    ) -> flint_store::StoreResult<flint_store::EpochLease> {
        self.0.epoch_acquire(key, holder, observed).await
    }
    async fn epoch_renew(
        &self,
        key: &str,
        lease: &flint_store::EpochLease,
    ) -> flint_store::StoreResult<flint_store::EpochLease> {
        self.0.epoch_renew(key, lease).await
    }
    async fn epoch_release(
        &self,
        key: &str,
        lease: &flint_store::EpochLease,
    ) -> flint_store::StoreResult<()> {
        self.0.epoch_release(key, lease).await
    }
}

/// D8/D11 — the startup gate. Gated mode over a version-stripping proxy
/// must REFUSE at startup, before a single byte is staged; and the same
/// gate must be inert in `hybrid`, which needs no version surface at
/// all. Without the second half the gate would take every default
/// workspace down with it.
#[tokio::test]
async fn gated_startup_refuses_a_version_stripping_backend() {
    let inner = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let state = SidecarState::open(cfg.state_dir()).unwrap();
    let mut a = Sidecar {
        store: Arc::new(VersionStripping(inner.clone())) as Arc<dyn ObjectStore>,
        cfg,
        state,
        lease: None,
    };
    assert!(claim_until_held(&mut a, 3).await);

    // hybrid (the default) does not care.
    a.gated_startup_check().await.expect("the startup gate fired outside gated mode");

    gated(&mut a);
    let err = a.gated_startup_check().await.unwrap_err().to_string();
    assert!(
        err.contains("versioning conformance"),
        "gated mode started over a stripping proxy: {err}"
    );
}

/// D9 — `recover-staged` after the routine pure-spot event: the pod is
/// replaced, the emptyDir (and with it the pending record) is gone, and
/// the last lane pass's work is durable-but-uncited. Recovery re-cites
/// the surviving current versions as ONE flagged boundary — a manifest
/// CAS, no data movement — and rolls FORWARD onto the newer work.
#[tokio::test]
async fn recover_staged_recites_uncited_work_forward() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);

    // One cited boundary, then uncited work on top of it: an edit to a
    // cited path AND a brand-new path the manifest has never seen.
    write(dir.path(), "cited.txt", "V1");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();
    let cited_seq = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;

    write(dir.path(), "cited.txt", "V2-UNCITED");
    backdate_baseline(&a, "cited.txt");
    write(dir.path(), "brand-new.txt", "NEW-UNCITED");
    a.upload_lane().await.unwrap();
    drop(a); // the pod goes away; the emptyDir with it

    // A replacement pod: fresh emptyDir, no pending record at all.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    b.cfg.boundary_mode = super::BoundaryMode::Gated;
    b.cfg.visibility_lag_bound_secs = Some(3600);
    assert!(claim_until_held(&mut b, 12).await);

    // Anti-vacuity: before recovery the newer work is genuinely invisible.
    let before = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    assert!(!before.entries.contains_key("brand-new.txt"));

    let r = b.recover_staged().await.unwrap();
    assert_eq!(r.recited.len(), 2, "recovery re-cited {:?}", r.recited);

    let m = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.boundary_source.as_deref(), Some("recovered"));
    assert!(m.seq > cited_seq);
    assert!(m.entries.contains_key("brand-new.txt"));

    // Rolls FORWARD: a checkout of the recovered boundary yields the
    // newer bytes, not the last cited ones.
    let dir_c = tempfile::tempdir().unwrap();
    let mut c = sidecar(&store, dir_c.path()).await;
    c.checkout().await.unwrap();
    assert_eq!(read(dir_c.path(), "cited.txt").as_deref(), Some("V2-UNCITED"));
    assert_eq!(read(dir_c.path(), "brand-new.txt").as_deref(), Some("NEW-UNCITED"));
    // The recovery is surfaced, never silent.
    assert!(
        b.state.load_conflicts().unwrap().iter().any(|c| c.kind == "recovered-staged"),
        "recovery re-cited foreign-generation bytes without a conflict record"
    );
}

/// D8's abandoned-mid-stage endgame, closed. The backstop reaped the
/// CITED version, checkout refuses (proved in the leg above) — and
/// `recover-staged` is what makes the workspace usable again, by
/// re-citing the surviving current version.
#[tokio::test]
async fn recover_staged_repairs_a_dangling_citation() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);
    write(dir.path(), "abandoned.txt", "CITED WORK");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();
    write(dir.path(), "abandoned.txt", "UNCITED WORK");
    backdate_baseline(&a, "abandoned.txt");
    a.upload_lane().await.unwrap();
    let key = a.cfg.file_key("abandoned.txt");
    assert!(!store.expire_noncurrent(0).is_empty(), "the endgame fixture never armed");

    // The dangling state, confirmed before the repair.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    assert!(b.checkout().await.is_err());

    let dir_r = tempfile::tempdir().unwrap();
    let mut r = sidecar(&store, dir_r.path()).await;
    r.cfg.boundary_mode = super::BoundaryMode::Gated;
    r.cfg.visibility_lag_bound_secs = Some(3600);
    assert!(claim_until_held(&mut r, 12).await);
    let rep = r.recover_staged().await.unwrap();
    assert_eq!(rep.dangling.len(), 1, "the dangling citation was not recognized");

    // Now a fresh checkout completes, on the surviving newer bytes.
    let dir_c = tempfile::tempdir().unwrap();
    let mut c = sidecar(&store, dir_c.path()).await;
    c.checkout().await.expect("checkout still refuses after recover-staged");
    assert_eq!(read(dir_c.path(), "abandoned.txt").as_deref(), Some("UNCITED WORK"));
    let _ = key;
}

/// Recovery must not invent work: over a workspace whose every cited
/// version IS the current version, `recover-staged` re-cites nothing
/// and does not advance the manifest. (Without this the leg above
/// passes for a recovery that blindly re-cites the whole tree.)
#[tokio::test]
async fn recover_staged_is_a_no_op_when_nothing_is_uncited() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);
    write(dir.path(), "quiet.txt", "V1");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();
    let seq = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;

    let r = a.recover_staged().await.unwrap();
    assert!(r.recited.is_empty() && r.dangling.is_empty());
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.seq, seq, "a no-op recovery still advanced the manifest");
}

// ---------------------------------------------------------------------
// Phase 3 observability minimum (§2.6, review ledger OF-6): "why is the
// manifest not advancing" must be answerable from inside the pod,
// before Phase 6's metrics stack exists and without spelunking the
// emptyDir.
// ---------------------------------------------------------------------

/// The gated withheld state, gauged. A tick that made bytes durable and
/// cited nothing is the mode WORKING — but it is indistinguishable from
/// a wedged loop unless something says why.
#[tokio::test]
async fn gauges_name_the_reason_visibility_is_withheld() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);

    write(dir.path(), "big.bin", "STAGED BUT UNCITED");
    a.floor_tick().await.unwrap();

    let g = a.load_gauges().unwrap();
    assert_eq!(g.state, "live");
    assert_eq!(g.boundary_mode, "gated");
    assert_eq!(g.staged_uncited_count, 1);
    assert_eq!(g.staged_uncited_bytes, "STAGED BUT UNCITED".len() as u64);
    assert_eq!(
        g.withheld_reason.as_deref(),
        Some("quiesce-pending"),
        "a withheld tick did not name its reason"
    );
    assert!(g.last_boundary.is_none(), "nothing was cited, yet a boundary is claimed");
    // The gauge that watches D8's inversion: a cited version went
    // noncurrent the moment the lane staged over it, and the retention
    // backstop's clock is now running against live cited data.
    assert!(g.cited_noncurrent_age_max_secs < 5);
    assert_eq!(g.sentinel_budget_remaining, a.cfg.sentinel_hourly_budget);
}

/// The forced-citation counter and the boundary stamp — OF-5's "a
/// forced possibly-torn citation must be visible downstream", on the
/// local side. A fleet that forces every citation is a fleet whose
/// coherence contract is void, and the count is how anyone notices.
#[tokio::test]
async fn gauges_count_forced_citations_and_name_the_last_boundary() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);
    a.cfg.visibility_lag_bound_secs = Some(0);

    for g in 1..4 {
        write(dir.path(), "churn.txt", &format!("g{g}"));
        backdate_baseline(&a, "churn.txt");
        a.floor_tick().await.unwrap();
    }

    let g = a.load_gauges().unwrap();
    assert_eq!(g.forced_citation_count, 3, "forced citations were not counted");
    let lb = g.last_boundary.expect("a citation installed, but no boundary recorded");
    assert_eq!(lb.source, "forced-lag-cap");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(lb.seq, m.seq);
    // Nothing is staged any more, so nothing is withheld.
    assert_eq!(g.staged_uncited_count, 0);
    assert_eq!(g.withheld_reason, None);
}

/// A deposed sidecar's gauges must say `fenced`, exactly as
/// `capabilities.json` does. An agent that reads only the gauges (the
/// operational file) must not conclude a zombie is healthy — the two
/// surfaces cannot be allowed to disagree about liveness.
#[tokio::test]
async fn a_fenced_sidecar_gauges_itself_fenced() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir_a.path(), "work.txt", "v1");
    a.floor_tick().await.unwrap();
    assert_eq!(a.load_gauges().unwrap().state, "live");

    // A successor takes over.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    assert!(claim_until_held(&mut b, 12).await);

    write(dir_a.path(), "work.txt", "v2");
    backdate_baseline(&a, "work.txt");
    let err = a.floor_tick().await.unwrap_err();
    assert!(matches!(err, LeanError::Fenced(_)), "the straggler was not fenced: {err}");

    assert_eq!(a.read_capabilities().unwrap().state, "fenced");
    assert_eq!(
        a.load_gauges().unwrap().state,
        "fenced",
        "capabilities say fenced but the gauges still say live"
    );
}

/// `flint-sync status` is the exec surface for a workspace whose
/// sidecar is DEAD or deposed — so it must render with no lease held
/// and no claim attempted. A status verb that claims the lease would
/// depose the very sidecar being diagnosed.
#[tokio::test]
async fn status_renders_without_claiming_the_lease() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    // The mode is env-stamped at pod creation, before anything runs.
    gated(&mut a);
    a.checkout().await.unwrap();
    write(dir.path(), "held.txt", "x");
    a.floor_tick().await.unwrap();

    // A second process over the SAME tree cannot even open the state
    // dir (the occupancy flock), so status reads the files directly.
    let s = super::status_report(&cfg_for(dir.path())).unwrap();
    assert_eq!(s.gauges.unwrap().staged_uncited_count, 1);
    assert_eq!(s.capabilities.unwrap().boundary_mode, "gated");
    assert_eq!(s.pending_stage_entries, 1);
    assert!(s.incarnation_epoch.is_some());
    // Rendering it must not have touched the lease.
    assert_eq!(
        store.epoch_read(&a.cfg.epoch_key()).await.unwrap().unwrap().epoch,
        a.lease.as_ref().unwrap().epoch,
        "status rotated the epoch it was supposed to observe"
    );
}

/// **Found by the formal model (tranche 3 product 2), in shipped code.**
///
/// The gated upload lane opens no HITL window — deliberately, because a
/// lane that fenced HITL out every floor tick would refuse admission
/// essentially forever between citations. So a UI write can land on a
/// path the lane has already staged. The citation lane's base-version
/// re-validation cannot see it: that check reads the BASELINE, and the
/// citation lane consumes nothing, so the baseline has not moved.
///
/// The citation then cites our staged version — and the exact version
/// reaper, whose rule was "delete every version of a touched key except
/// the one the installed manifest cites", deleted the user's version.
/// It was CURRENT. The inbox entry then 412s on its next consume and is
/// dropped as superseded: an acked write, gone silently.
///
/// Two rules close it, and both are asserted here: the reaper never
/// reclaims the CURRENT version, and a staged path with a live inbox
/// entry is dropped from the citation rather than cited over.
#[tokio::test]
async fn citation_never_reaps_a_hitl_write_that_landed_mid_stage() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);

    write(dir.path(), "shared.txt", "AGENT V1");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();

    // The lane stages the agent's next generation...
    write(dir.path(), "shared.txt", "AGENT V2");
    backdate_baseline(&a, "shared.txt");
    a.upload_lane().await.unwrap();
    let staged = a.load_stage().unwrap().entries["shared.txt"].version_id.clone().unwrap();

    // ...and THEN a UI write lands on the same path. The lane opens no
    // window, so this is admitted, and it is acked to the user.
    hitl_write(&store, &a.cfg, "shared.txt", "USER EDIT", "ui@example").await.unwrap();
    let key = a.cfg.file_key("shared.txt");
    let (hitl_meta, _) = store.get_whole(&key, None).await.unwrap();
    let hitl_version = hitl_meta.version_id.clone().unwrap();
    assert_ne!(hitl_version, staged, "the fixture never armed: no foreign version landed");

    a.citation_pass(CitationSource::Sentinel).await.unwrap();

    // The user's bytes are still THERE. This is the half that was
    // destroyed: the reaper deleted the current version.
    let (_, still) = store
        .get_version(&key, &hitl_version)
        .await
        .expect("the citation reaped the user's CURRENT version");
    assert_eq!(&still[..], b"USER EDIT");
    // ...and still current, so a plain read serves them.
    let (_, current) = store.get_whole(&key, None).await.unwrap();
    assert_eq!(&current[..], b"USER EDIT");

    // And the citation did not cite OUR older generation over them: an
    // in-flight inbox entry drops the path from the boundary.
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_ne!(
        m.entries["shared.txt"].version_id.as_deref(),
        Some(staged.as_str()),
        "the citation cited bytes that PREDATE the user's write, over it"
    );
    assert!(
        a.state.load_conflicts().unwrap().iter().any(|c| c.kind.contains("hitl-inflight")),
        "the dropped path was not surfaced"
    );
    // The entry is still queued, so the next lane integrates it normally.
    let ib = inbox::load(store.as_ref(), &a.cfg).await.unwrap();
    assert!(ib.doc.entries.iter().any(|e| e.path == "shared.txt"));
}

/// The companion to the leg above, and the reason the keep-current rule
/// is not redundant with the in-flight inbox check.
///
/// The inbox only sees writers who go through the gateway. §3 residual
/// 11 says out loud that others exist — an import tool, `aws s3 cp`, a
/// human with credentials — and under gated they write to a key whose
/// current version is uncited. The citation cannot know about them
/// (that is the residual, not a bug), but the reaper must still not
/// DELETE them: the difference between "your write is not cited yet"
/// and "your write is gone" is the whole of it.
///
/// Both the formal model and the Rust battery needed two arms to see
/// this: with the inbox guard in place, removing the keep-current rule
/// changes nothing on the HITL path. This is the arm that isolates it.
#[tokio::test]
async fn the_reaper_never_takes_an_out_of_band_writers_current_version() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);

    write(dir.path(), "shared.txt", "AGENT V1");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();
    write(dir.path(), "shared.txt", "AGENT V2");
    backdate_baseline(&a, "shared.txt");
    a.upload_lane().await.unwrap();

    // An out-of-band write: no inbox entry, no gateway, no window check.
    let key = a.cfg.file_key("shared.txt");
    let (cur, _) = store.get_whole(&key, None).await.unwrap();
    let stamps = GenerationStamps {
        generation: 0,
        epoch: 0,
        flush_uuid: "out-of-band".into(),
        boundary_source: None,
        posix: None,
    };
    let foreign = store
        .put_whole(
            &key,
            Bytes::from_static(b"OUT OF BAND"),
            &PutCondition::IfMatch(cur.etag.clone()),
            &stamps,
            crc64_nvme(b"OUT OF BAND"),
        )
        .await
        .unwrap();
    let foreign_version = foreign.version_id.clone().unwrap();
    assert!(inbox::load(store.as_ref(), &a.cfg).await.unwrap().doc.entries.is_empty());

    a.citation_pass(CitationSource::Sentinel).await.unwrap();

    // Not cited — that is the documented residual, and it is fine.
    // Not DELETED — that is the rule.
    let (_, still) = store
        .get_version(&key, &foreign_version)
        .await
        .expect("the reaper deleted an out-of-band writer's CURRENT version");
    assert_eq!(&still[..], b"OUT OF BAND");
}
