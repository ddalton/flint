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
    Sidecar {
        store: store.clone() as Arc<dyn ObjectStore>,
        cfg,
        state,
        lease: None,
        noted_not_regular: Default::default(),
    }
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
    manifest::cas_write(store.as_ref(), &sc.cfg, &theirs, Some(&loaded.handle()), 0, "other-writer")
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
    manifest::cas_write(store.as_ref(), &a.cfg, &theirs, Some(&loaded.handle()), 0, "remote-delete")
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
        manifest::cas_write(store.as_ref(), &a.cfg, &m, Some(&loaded.handle()), 0, "legacy")
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
    manifest::cas_write(store.as_ref(), &a.cfg, &m, Some(&loaded.handle()), 0, "legacy")
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
    manifest::cas_write(store.as_ref(), &a.cfg, &theirs, Some(&loaded.handle()), 0, "sibling")
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
    manifest::cas_write(store.as_ref(), &a.cfg, &theirs, Some(&loaded.handle()), 0, "foreign")
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
    manifest::cas_write(store.as_ref(), &a.cfg, &theirs, Some(&loaded.handle()), 0, "remote-delete")
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

/// U8 — the last open corner of the protocol, and the one the bucket
/// drill hit from the other side ("a one-shot blocks in `claim` FOREVER").
///
/// A sidecar SIGKILLed mid-honor runs no cooperative fence path, so its
/// pending sentinel is never settled. The kubelet restarts it over the
/// surviving emptyDir while a successor holds the lease; it blocks in
/// `claim` (the successor's token keeps advancing, so quiet polls never
/// accumulate) and `settle_pending_at_startup` is unreachable, because
/// that runs only AFTER claim returns. The agent is left polling an ack
/// that will never come, behind a marker that still says `live`.
#[tokio::test]
async fn a_restarted_claimant_that_can_never_honor_says_so_instead_of_stranding() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();

    // The agent declares a boundary; the sidecar consumes it into a
    // pending record and is SIGKILLed before honoring.
    touch_sentinel(dir_a.path(), control::PUBLISH, r#"{"nonce":"task-42"}"#);
    assert!(a.consume_sentinel(Verb::Publish).unwrap(), "the touch was not consumed");
    assert!(a.load_pending(Verb::Publish).unwrap().is_some(), "no pending record to owe");
    assert!(a.read_ack(Verb::Publish).is_none(), "an ack already exists — fixture not armed");

    // A successor takes the lease.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    assert!(claim_until_held(&mut b, 10).await, "quiet polls exhausted ⇒ takeover");

    // A restarts over its surviving emptyDir and lands in Waiting: the
    // live successor means it can never claim, so it can never honor.
    let outcome = lease::claim_step(&mut a).await.unwrap();
    assert!(
        matches!(outcome, lease::ClaimOutcome::Waiting { .. }),
        "the fixture never armed — A claimed over a live successor"
    );

    let answered = a.refuse_what_this_incarnation_can_never_honor().await.unwrap();
    assert!(answered, "nothing was owed — the fixture never armed");

    // THE ASSERTIONS. The agent gets an answer...
    let ack = a.read_ack(Verb::Publish).expect("the agent is still stranded: no ack");
    assert_eq!(ack.status, "refused-fenced");
    assert!(ack.nonces.contains(&"task-42".to_string()), "the ack does not cover the agent's nonce");
    assert_eq!(ack.observed_epoch, Some(b.lease.as_ref().unwrap().epoch), "the ack names no fencer");

    // ...and the marker stops advertising verbs a zombie cannot serve.
    let caps: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir_a.path().join(".flint").join("capabilities.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(caps["state"], "fenced", "capabilities.json still says the zombie is live");
}

/// The other half of the same rule: a FRESH replacement pod waiting out
/// its 60 s of quiet polls is healthy, not fenced. Nothing is owed in a
/// fresh emptyDir, so it must take none of this — or every rolling
/// restart would mark itself fenced on the way up.
#[tokio::test]
async fn a_healthy_replacement_waiting_out_quiet_polls_is_not_marked_fenced() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    let posture = b.sentinel_preflight().unwrap();
    b.write_capabilities(&posture, false).unwrap();
    // B is fresh and A still holds: B waits.
    assert!(
        matches!(lease::claim_step(&mut b).await.unwrap(), lease::ClaimOutcome::Waiting { .. }),
        "the fixture never armed — B claimed instantly"
    );

    let answered = b.refuse_what_this_incarnation_can_never_honor().await.unwrap();
    assert!(!answered, "a healthy replacement marked itself fenced on the way up");
    assert!(b.read_ack(Verb::Publish).is_none(), "a refused ack appeared with nothing owed");
    let caps: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir_b.path().join(".flint").join("capabilities.json")).unwrap(),
    )
    .unwrap();
    assert_ne!(caps["state"], "fenced", "a healthy replacement was marked fenced");
    // And it still takes over on schedule.
    assert!(claim_until_held(&mut b, 10).await, "the refusal path blocked a legitimate takeover");
}

/// U22 — a STALE ack must not retire a FRESH request.
///
/// The restart rule ("crash after ack before retire ⇒ retire on
/// restart") decided whether a boundary had already run by comparing
/// the agent's own sentinel file mtime. That value is not monotone even
/// without an adversary — `touch -t`, a clock step, a restored file, a
/// tar extract all move it backwards — and for a BARE touch the nonce
/// test is vacuously true over an empty set, so the mtime was the whole
/// test. A boundary then gets retired having never run.
#[tokio::test]
async fn a_stale_ack_never_retires_a_fresh_bare_touch() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();

    // Boundary 1: a bare touch, honored, acked.
    write(dir.path(), "w.txt", "one");
    touch_sentinel(dir.path(), control::PUBLISH, "");
    let acks = a.sentinel_tick().await.unwrap();
    assert_eq!(acks.len(), 1, "the first bare touch was not honored");
    let first = a.read_ack(Verb::Publish).expect("no ack from the first honor");
    assert_eq!(first.status, "ok");

    // Boundary 2: the agent asks again, and the sentinel's mtime lands
    // at or BEFORE the first one's — the non-monotone case.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write(dir.path(), "w.txt", "two");
    touch_sentinel(dir.path(), control::PUBLISH, "");
    let path = dir.path().join(super::CONTROL_DIR).join(control::PUBLISH);
    let backdated = std::time::SystemTime::UNIX_EPOCH
        + std::time::Duration::from_nanos((first.sentinel_mtime_unix_ns as u64).saturating_sub(5));
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(backdated)).unwrap();
    drop(f);

    assert!(a.consume_sentinel(Verb::Publish).unwrap(), "the second touch was not consumed");
    let pending = a.load_pending(Verb::Publish).unwrap().expect("no pending record");
    assert!(pending.nonces.is_empty(), "the fixture needs a BARE touch");
    assert!(
        pending.consumed_mtime_unix_ns <= first.sentinel_mtime_unix_ns,
        "the mtime did not go backwards — the fixture never armed"
    );

    // THE ASSERTION. The standing ack answers an older request; it must
    // not be read as answering this one.
    assert!(
        !a.ack_matches(Verb::Publish, &pending),
        "a stale ack retired a fresh boundary — it will never run"
    );

    // And the boundary does then actually run, with a new ack. (Forced:
    // the first honor just set the min-interval, which is not what this
    // leg is about.)
    let ack = a
        .honor_pending(Verb::Publish, true)
        .await
        .unwrap()
        .expect("the second boundary never ran");
    assert_eq!(ack.status, "ok");
    let second = a.read_ack(Verb::Publish).expect("no ack from the second honor");
    assert!(second.completed_unix > first.completed_unix, "the ack was not re-minted");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let e = &m.entries["w.txt"];
    let (_, body) = store.get_whole(&e.key, Some(&e.etag)).await.unwrap();
    assert_eq!(&body[..], b"two", "the second boundary was retired without publishing");
}

/// U23 — the FIFO wedge, at the syscall.
///
/// `consume_sentinel` lstats the path, checks `is_file()`, then opens it
/// BY PATH — a second resolution. Swap a FIFO in between and the open
/// blocks forever waiting for a writer, taking the poll arm and every
/// boundary behind it. The type check cannot close this by itself; the
/// open has to not block.
///
/// The timeout is the assertion. Without `O_NONBLOCK` this test does not
/// fail — it HANGS, which is exactly the production symptom.
#[tokio::test]
async fn a_fifo_at_the_sentinel_path_never_wedges_the_poll_arm() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();

    // A writer-less FIFO exactly where the agent's touch would go.
    let ctl = dir.path().join(super::CONTROL_DIR);
    std::fs::create_dir_all(&ctl).unwrap();
    let path = ctl.join(control::PUBLISH);
    let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0, "mkfifo failed");
    assert!(
        !std::fs::metadata(&path).unwrap().is_file(),
        "the fixture never armed — that is not a FIFO"
    );

    // NOTE: do not "prove" the wedge here by racing a plain File::open
    // against a timeout. The open never returns, and tokio waits for
    // blocking tasks at teardown — so the probe hangs the test it was
    // meant to make honest. The anti-vacuity that matters is above (it
    // really is a FIFO); that the old open blocked is proven by
    // reverting read_bounded, where this leg HANGS rather than fails.

    // THE TOCTOU ITSELF. The lstat above catches a FIFO that is already
    // in place; the window the finding names is a FIFO swapped in AFTER
    // the check, and the only thing that closes it is the open not
    // blocking. Call the reader directly, which is where the fix lives.
    //
    // Under a blocking open this call NEVER RETURNS: the leg does not
    // fail, the test binary hangs. That is the production symptom, and
    // it is why the fix is O_NONBLOCK rather than a tighter check.
    let r = super::sentinel::read_bounded(&path);
    assert!(r.is_err(), "a writer-less FIFO read as a body instead of erroring");

    // A symlink swapped in for the same purpose is refused too.
    let target = ctl.join("elsewhere");
    std::fs::write(&target, "not the agent's").unwrap();
    let link = ctl.join("publish.link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(
        super::sentinel::read_bounded(&link).is_err(),
        "a symlink at the sentinel path was followed"
    );

    // And the tick returns promptly with the FIFO in place.
    let ticked = tokio::time::timeout(std::time::Duration::from_secs(5), a.sentinel_tick()).await;
    let acks = ticked.expect("THE POLL ARM WEDGED on a FIFO at the sentinel path").unwrap();
    assert!(acks.is_empty(), "a FIFO was honored as a boundary");

    // And it is recorded rather than silently skipped...
    let conflicts = a.state.load_conflicts().unwrap();
    assert!(
        conflicts.iter().any(|c| c.kind == "sentinel-not-regular-file"),
        "the non-regular sentinel left no conflict record"
    );

    // ...ONCE, not once per tick. A parked FIFO is a standing condition
    // and the poll arm sees it every 10 s; `load_conflicts` parses the
    // whole file twice per sync honor, so a per-tick record turns a
    // wedge into a growing O(n) parse for as long as it stands.
    for _ in 0..5 {
        a.sentinel_tick().await.unwrap();
    }
    let after = a.state.load_conflicts().unwrap();
    assert_eq!(
        after.iter().filter(|c| c.kind == "sentinel-not-regular-file").count(),
        1,
        "the standing condition was re-recorded on every poll tick"
    );

    // A recurrence after the condition clears IS recorded again.
    std::fs::remove_file(&path).unwrap();
    touch_sentinel(dir.path(), control::PUBLISH, "");
    a.sentinel_tick().await.unwrap();
    let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0, "second mkfifo failed");
    a.sentinel_tick().await.unwrap();
    assert_eq!(
        a.state.load_conflicts().unwrap().iter()
            .filter(|c| c.kind == "sentinel-not-regular-file")
            .count(),
        2,
        "a RECURRENCE of the condition went unrecorded — the latch never clears"
    );
}

/// U38 — an `ok` ack must name a seq. A gated no-diff honor installed
/// nothing and returned `seq: null` under `status: "ok"`, which breaks
/// the ack schema and §1.2's authoritative-durability recipe: read the
/// ack's seq, then confirm that seq in the bucket. Nothing was
/// published, but the agent's boundary IS satisfied — by the boundary
/// already standing. The ack names that one.
#[tokio::test]
async fn a_gated_no_diff_honor_still_names_the_boundary_it_is_satisfied_by() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);

    write(dir.path(), "w.txt", "ONE");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();
    let installed = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;

    // A second boundary with NOTHING changed since the first.
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"n-nodiff"}"#);
    assert!(a.consume_sentinel(Verb::Publish).unwrap());
    let ack = a
        .honor_pending(Verb::Publish, true)
        .await
        .unwrap()
        .expect("the no-diff honor produced no ack at all");

    assert_eq!(ack.status, "ok");
    assert_eq!(ack.report.uploaded, 0, "the fixture published something — not a no-diff honor");
    let seq = ack.seq.expect("an ok ack with seq: null — §1.2's recipe has nothing to confirm");
    assert_eq!(seq, installed, "the ack named a boundary that is not the standing one");

    // The recipe actually works: that seq is in the bucket.
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.seq, seq, "the ack's seq is not the manifest's");
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

/// U2 / §8 Q2's PREMISE. A deposed straggler must not destroy the
/// SUCCESSOR's cited data.
///
/// The old reaper rule was "delete every version of a touched key that
/// is neither `keep` nor `is_current`". Its `is_current` guard protects
/// exactly one version, on the assumption that at most one foreign
/// generation can appear between the lane and the citation. A successor
/// in gated mode does not stop at one — its cadence is stage → cite →
/// stage — so a straggler resuming inside the reaper finds the
/// successor's CITED version sitting noncurrent-and-not-`keep`, and
/// deletes it.
///
/// The plan asserts the opposite in four places ("they destroy
/// nothing", "non-destructive stragglers"), and it is the premise §8 Q2
/// chose versioned staging on. Drill leg B12 cannot see this: it freezes
/// the straggler INSIDE THE UPLOAD LOOP, which is the arm that really is
/// non-destructive. The reaper arm is never frozen.
#[tokio::test]
async fn a_deposed_stragglers_reaper_never_deletes_the_successors_cited_version() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);

    // A cites G1. This is the manifest A is still holding when it stalls
    // inside its own reaper.
    write(dir_a.path(), "w.txt", "G1-FROM-A");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();
    let a_installed = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let key = a.cfg.file_key("w.txt");
    let v1 = a_installed.entries["w.txt"].version_id.clone().unwrap();

    // A stalls. B takes over.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    assert!(claim_until_held(&mut b, 10).await, "quiet polls exhausted ⇒ takeover");
    b.checkout().await.unwrap();
    gated(&mut b);

    // B cites G2 — the version whose survival is the whole point.
    write(dir_b.path(), "w.txt", "G2-CITED-BY-B");
    b.upload_lane().await.unwrap();
    b.citation_pass(CitationSource::Sentinel).await.unwrap();
    let b_installed = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    let v2 = b_installed.entries["w.txt"].version_id.clone().unwrap();
    assert_ne!(v2, v1, "B cited the same version A did — the fixture never armed");

    // ...and then stages G3, which is the ordinary gated steady state
    // and is what pushes B's CITED version off `current`.
    write(dir_b.path(), "w.txt", "G3-STAGED-BY-B");
    backdate_baseline(&b, "w.txt");
    b.upload_lane().await.unwrap();

    // ANTI-VACUITY: the fixture must really put B's cited version in the
    // old rule's kill zone — present, not current, and not A's `keep`.
    let versions = store.list_versions(&key).await.unwrap();
    let cited_by_b = versions
        .iter()
        .find(|v| v.version_id == v2)
        .expect("B's cited version vanished before the reaper ran");
    assert!(!cited_by_b.is_current, "B's cited version is still current — the kill zone is empty");
    assert!(
        versions.iter().any(|v| v.is_current && v.version_id != v2 && v.version_id != v1),
        "nothing newer than B's citation is current — the fixture never armed"
    );

    // A thaws INSIDE its reaper, holding its own pass's state.
    let stage_a = a.load_stage().unwrap();
    let mut upserts = std::collections::BTreeMap::new();
    upserts.insert("w.txt".to_string(), a_installed.entries["w.txt"].clone());
    let mut report = Default::default();
    let _ = a.reclaim_superseded(&upserts, &a_installed, &stage_a, &mut report).await;

    // THE ASSERTION. B's cited version must still be fetchable and
    // byte-identical: a deposed straggler destroys nothing.
    let (_, body) = store
        .get_version(&key, &v2)
        .await
        .expect("the straggler's reaper DELETED the successor's cited version");
    assert_eq!(&body[..], b"G2-CITED-BY-B", "the successor's cited bytes changed");

    // And the successor can still serve its own manifest end to end.
    let cited = &b_installed.entries["w.txt"];
    let (_, via_manifest) = store.get_version(&cited.key, cited.version_id.as_ref().unwrap()).await
        .expect("the successor's manifest dangles after the straggler reaped");
    assert_eq!(&via_manifest[..], b"G2-CITED-BY-B");
}

/// The narrowing, stated on its own and without a deposal: the reaper
/// reclaims what THIS workspace superseded and nothing else. A version
/// it cannot name in its own pending record is a crash remnant or
/// somebody else's work, and both belong to the noncurrent backstop.
#[tokio::test]
async fn the_reaper_reclaims_only_the_version_its_own_record_names() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);

    write(dir.path(), "n.txt", "V1");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();
    let key = a.cfg.file_key("n.txt");
    let v1 = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries
        ["n.txt"]
        .version_id
        .clone()
        .unwrap();

    // ARM 1 — the ordinary supersede cycle. Anti-vacuity on the fix
    // itself: the narrowed reaper must still do its actual job.
    write(dir.path(), "n.txt", "V2");
    backdate_baseline(&a, "n.txt");
    a.upload_lane().await.unwrap();
    let r = a.citation_pass(CitationSource::Sentinel).await.unwrap();
    assert_eq!(r.versions_reclaimed, 1, "the reaper stopped reclaiming anything at all");
    assert!(
        store.get_version(&key, &v1).await.is_err(),
        "the version this workspace superseded was not reclaimed"
    );
    let installed = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let v2 = installed.entries["n.txt"].version_id.clone().unwrap();

    // ARM 2 — a version this workspace's record cannot name: the shape
    // of a crash remnant (the staging PUT landed, the pending record
    // never did) and of any foreign write.
    // TWO of them, so the older is NONCURRENT — which is the only shape
    // the old rule would have swept. One current unnameable version
    // proves nothing: the old rule skipped `is_current` too.
    let mut unnameable = vec![];
    for (n, body) in [(98u64, "UNNAMEABLE-A"), (99, "UNNAMEABLE-B")] {
        let b = Bytes::from(body);
        let meta = store
            .put_whole(
                &key,
                b.clone(),
                &PutCondition::IfMatch(store.head(&key).await.unwrap().etag),
                &GenerationStamps {
                    generation: n,
                    epoch: 0,
                    flush_uuid: "crash-remnant".into(),
                    boundary_source: None,
                    posix: None,
                },
                crc64_nvme(&b),
            )
            .await
            .unwrap();
        unnameable.push(meta.version_id.clone().expect("versioned store"));
    }
    let (orphan_noncurrent, orphan_vid) = (unnameable[0].clone(), unnameable[1].clone());
    // Anti-vacuity: the older remnant really is in the old rule's kill
    // zone — present, not current, not `keep`.
    let vs = store.list_versions(&key).await.unwrap();
    assert!(
        vs.iter().any(|v| v.version_id == orphan_noncurrent && !v.is_current),
        "the unnameable remnant is current — the old rule would have skipped it anyway"
    );

    // Drive the reaper straight, with a record that names v2 as what it
    // superseded — so the ONLY difference between the two versions on
    // this key is whether our own record names them.
    let mut stage = a.load_stage().unwrap();
    let mut pe = super::gated::PendingEntry {
        key: key.clone(),
        etag: installed.entries["n.txt"].etag.clone(),
        crc64_b64: None,
        size: 2,
        mode: 0o644,
        mtime_unix: 0,
        generation: 3,
        epoch: 1,
        version_id: None,
        base_version_id: Some(v2.clone()),
        staged_unix: 0,
    };
    pe.version_id = Some(orphan_vid.clone());
    stage.entries.insert("n.txt".to_string(), pe);

    // `keep` must differ from both, or the guards short-circuit.
    let mut keep_manifest = installed.clone();
    keep_manifest.entries.get_mut("n.txt").unwrap().version_id = Some(orphan_vid.clone());
    let mut upserts = std::collections::BTreeMap::new();
    upserts.insert("n.txt".to_string(), keep_manifest.entries["n.txt"].clone());

    let mut report = Default::default();
    a.reclaim_superseded(&upserts, &keep_manifest, &stage, &mut report).await.unwrap();

    // v2 IS named as superseded ⇒ reclaimed. Anti-vacuity for arm 2.
    assert_eq!(report.versions_reclaimed, 1, "the named superseded version was not reclaimed");
    assert!(store.get_version(&key, &v2).await.is_err(), "the named version survived");
    // Nothing else on the key was touched — including the NONCURRENT
    // remnant, which is precisely what the old rule swept.
    store
        .get_version(&key, &orphan_noncurrent)
        .await
        .expect("the reaper deleted a NONCURRENT version its own record never named");
    store
        .get_version(&key, &orphan_vid)
        .await
        .expect("the reaper deleted a version its own record never named");
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
    // And the source is readable FROM THE BUCKET alone, without
    // fetching a single entry — the pointer carries it.
    let lp = manifest::load_pointer(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    assert_eq!(lp.pointer.boundary_source.as_deref(), Some("quiescence"));
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
    let lp = manifest::load_pointer(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    assert_eq!(
        lp.pointer.boundary_source.as_deref(),
        Some("forced-lag-cap"),
        "the forced source is not readable from the bucket pointer alone"
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
        echo: Option<&str>,
    ) -> flint_store::StoreResult<flint_store::EpochLease> {
        self.0.epoch_renew(key, lease, echo).await
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
        noted_not_regular: Default::default(),
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
/// U1 — D9's durable summary is the MECHANISM, not decoration.
///
/// `flint-store`'s trait doc has always said the prefix-wide
/// `ListObjectVersions` is "the claim-time/DR fallback when
/// `orphans.json` is missing or stale — the expensive path, which is
/// why the durable summary is written eagerly". Nothing in the sidecar
/// read the summary, so recovery always took the expensive path and the
/// eager write bought the sidecar nothing.
#[tokio::test]
async fn recover_staged_uses_the_durable_summary_and_falls_back_without_it() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);

    write(dir.path(), "cited.txt", "V1");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();

    // A path that is CITED and then never touched again. It is NOT in
    // the orphan summary — the summary names staged candidates — so a
    // narrowing that lists only the summary's keys leaves it with no
    // surviving version and recovery calls it UNRECOVERABLE. Drill leg
    // B11b caught exactly that; the first version of this test did not,
    // because every cited path was also a staged one.
    write(dir.path(), "quiet.txt", "CITED-AND-THEN-QUIET");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();

    write(dir.path(), "cited.txt", "V2-UNCITED");
    backdate_baseline(&a, "cited.txt");
    write(dir.path(), "brand-new.txt", "NEW-UNCITED");
    a.upload_lane().await.unwrap();
    // Anti-vacuity: the lane really did publish a summary to recover
    // FROM, and that summary really does NOT name the quiet path.
    let okey = a.orphans_key();
    let (_, obytes) = store.get_whole(&okey, None).await.expect("no orphan summary");
    let odoc: serde_json::Value = serde_json::from_slice(&obytes).unwrap();
    let named: Vec<String> = odoc["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap().to_string())
        .collect();
    assert!(!named.is_empty(), "the summary names nothing");
    assert!(
        !named.contains(&"quiet.txt".to_string()),
        "the summary names the quiet path — the narrowing gap cannot be reproduced"
    );
    drop(a);

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    b.cfg.boundary_mode = super::BoundaryMode::Gated;
    b.cfg.visibility_lag_bound_secs = Some(3600);
    assert!(claim_until_held(&mut b, 12).await);

    let r = b.recover_staged().await.unwrap();
    assert!(r.from_summary, "recovery took the expensive path with a good summary on hand");
    assert_eq!(r.recited.len(), 2, "the cheap path recovered less: {:?}", r.recited);
    let m = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("brand-new.txt"), "the cheap path missed the new path");
    // THE REGRESSION B11b FOUND: the quiet cited path must survive the
    // narrowing. Recovery must not report it unrecoverable, and it must
    // still be cited afterwards.
    assert!(
        r.unrecoverable.is_empty(),
        "recovery called a cited path unrecoverable: {:?}",
        r.unrecoverable
    );
    assert!(
        m.entries.contains_key("quiet.txt"),
        "the narrowed recovery dropped a cited-but-not-staged path"
    );
    let q = &m.entries["quiet.txt"];
    let (_, qb) = store.get_version(&q.key, q.version_id.as_ref().unwrap()).await.unwrap();
    assert_eq!(&qb[..], b"CITED-AND-THEN-QUIET");

    // THE FALLBACK. Same work, no summary: recovery must still find
    // everything, by the expensive route. A summary is an optimisation,
    // never the source of truth.
    let dir_c = tempfile::tempdir().unwrap();
    let mut c = sidecar(&store, dir_c.path()).await;
    c.cfg.boundary_mode = super::BoundaryMode::Gated;
    c.cfg.visibility_lag_bound_secs = Some(3600);
    assert!(claim_until_held(&mut c, 12).await);
    c.checkout().await.unwrap();
    write(dir_c.path(), "later.txt", "UNCITED-AGAIN");
    c.upload_lane().await.unwrap();
    store.delete(&okey).await.unwrap(); // the summary is gone
    let r2 = c.recover_staged().await.unwrap();
    assert!(!r2.from_summary, "reported the cheap path with no summary present");
    assert!(
        r2.recited.iter().any(|p| p == "later.txt"),
        "the fallback missed uncited work: {:?}",
        r2.recited
    );
}

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

    let cite = a.citation_pass(CitationSource::Sentinel).await.unwrap();

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
    // …and it is reported as the kind that makes an ack for a DECLARED
    // boundary a lie (C6), not as the content-equal stale-base drop.
    assert_eq!(cite.dropped_inflight, vec!["shared.txt".to_string()]);
    assert!(cite.dropped_stale_base.is_empty());
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

/// D1 — the ack's own contract, on the one classification the barrier
/// deliberately withholds.
///
/// "A sentinel with mtime T means everything ordered-before T is a
/// coherent point; publish it", and the ack means that boundary is
/// installed. A delete the agent made BEFORE the touch is part of that
/// coherent point: the file's absence is visible on disk at consume
/// time, which is exactly the state D1's at-least guarantee names.
#[tokio::test]
async fn a_sentinel_boundary_carries_a_delete_made_before_the_touch() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    a.checkout().await.unwrap();

    write(dir.path(), "keep.txt", "k");
    write(dir.path(), "gone.txt", "v1");
    a.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("gone.txt"), "fixture: never published");

    // The agent's logical step: remove the file, then declare.
    std::fs::remove_file(dir.path().join("gone.txt")).unwrap();
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"n-del"}"#);

    let acks = a.sentinel_tick().await.unwrap();
    assert_eq!(acks.len(), 1, "the sentinel was not honored");
    assert_eq!(acks[0].status, "ok");

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(
        !m.entries.contains_key("gone.txt"),
        "the ack said ok at seq {:?} while the manifest still cites a file the agent \
         deleted before the touch (report.deleted = {})",
        acks[0].seq,
        acks[0].report.deleted
    );
}

/// The A/B that isolates the rule: the CADENCE barrier still withholds
/// the delete (the rename-vs-walk guard is not weakened for it), and the
/// DECLARED barrier confirms the absence and publishes it.
#[tokio::test]
async fn only_a_declared_barrier_confirms_a_first_absence() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "gone.txt", "v1");
    a.run_barrier().await.unwrap();

    std::fs::remove_file(dir.path().join("gone.txt")).unwrap();

    // Cadence: first absence, withheld, nothing deleted.
    let r = a.run_barrier().await.unwrap();
    assert_eq!(r.first_absence, vec!["gone.txt".to_string()]);
    assert_eq!(r.absences_confirmed, 0);
    assert!(r.deleted.is_empty(), "the cadence barrier published a first absence");

    // Put the path back into the withheld state the declared barrier
    // has to act on (the cadence pass above advanced prev_scan, so a
    // second cadence pass would delete it on its own — which is the
    // vacuity this fixture has to avoid).
    let mut b = a.state.load_baseline().unwrap();
    b.prev_scan.insert("gone.txt".into());
    a.state.save_baseline(&b).unwrap();

    let r = a.declared_barrier().await.unwrap();
    assert_eq!(r.absences_confirmed, 1, "the declared barrier did not confirm the absence");
    assert!(r.first_absence.is_empty());
    assert_eq!(r.deleted, vec!["gone.txt".to_string()]);
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("gone.txt"));
}

/// The guard the two-scan rule is FOR, preserved: a path the walk
/// missed but that is on disk at confirmation time is not deleted. The
/// confirmation is an lstat precisely because it cannot be fooled by
/// the walk race the rule names.
#[tokio::test]
async fn the_confirmation_never_deletes_a_path_the_walk_merely_missed() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let a = sidecar(&store, dir.path()).await;
    write(dir.path(), "renamed.txt", "here all along");

    // What a walk that lost the rename race produces: the path is
    // classified first-absent while the file is on disk.
    let mut classified = super::scan::Classified::default();
    classified.first_absence.insert("renamed.txt".into());
    classified.first_absence.insert("truly-gone.txt".into());

    let confirmed = a.confirm_absences(&mut classified);
    assert_eq!(confirmed, 1);
    assert_eq!(
        classified.deletes.iter().cloned().collect::<Vec<_>>(),
        vec!["truly-gone.txt".to_string()]
    );
    assert!(
        classified.first_absence.contains("renamed.txt"),
        "the confirmation promoted a path that is on disk — the walk race would publish \
         a delete of a live file"
    );
}

/// D1 × D6 — the same contract under gated mode: the citation a publish
/// sentinel triggers carries the delete, not just the uploads.
#[tokio::test]
async fn a_gated_sentinel_boundary_carries_a_delete_made_before_the_touch() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    gated(&mut a);
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    a.checkout().await.unwrap();

    write(dir.path(), "gone.txt", "v1");
    a.gated_tick(true).await.unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("gone.txt"), "fixture: never cited");

    std::fs::remove_file(dir.path().join("gone.txt")).unwrap();
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"n-del"}"#);
    let acks = a.sentinel_tick().await.unwrap();
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0].status, "ok");

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(
        !m.entries.contains_key("gone.txt"),
        "the gated citation acked a boundary that still cites a file the agent deleted \
         before the touch"
    );
}

/// D10 — the drain is a declared boundary too, and it is the last one
/// this workspace will ever have: a delete left withheld here is
/// re-materialized by the successor's checkout.
#[tokio::test]
async fn the_drain_carries_a_delete_made_before_it() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "gone.txt", "v1");
    a.run_barrier().await.unwrap();

    std::fs::remove_file(dir.path().join("gone.txt")).unwrap();
    a.drain().await.unwrap();

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(
        !m.entries.contains_key("gone.txt"),
        "the drain left the delete withheld — the successor's checkout resurrects it"
    );
}

/// D12 × D2 — the heartbeat renewal arm owes the refused ack too.
///
/// The heartbeat runs on its own interval precisely so liveness
/// signalling does not wait for publish cadence, which makes it the arm
/// that usually discovers deposal FIRST — ahead of the floor tick, and
/// ahead of a poll arm that has nothing due to honor. If it exits
/// without settling, the pending sentinel is stranded and the marker
/// still advertises live verbs on a zombie: the hole D2's refused acks
/// exist to close, at the arm D12 added.
#[tokio::test]
async fn the_heartbeat_arm_settles_owed_acks_when_it_finds_the_fence() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture, false).unwrap();
    a.checkout().await.unwrap();

    // Both verbs owed: settle_fence answers every one, not just the
    // verb some other arm happened to be honoring.
    touch_sentinel(dir_a.path(), control::PUBLISH, r#"{"nonce":"pub-stranded"}"#);
    touch_sentinel(dir_a.path(), control::SYNC, r#"{"nonce":"sync-stranded"}"#);
    a.poll_sentinels().unwrap();
    assert!(a.load_pending(Verb::Publish).unwrap().is_some());
    assert!(a.load_pending(Verb::Sync).unwrap().is_some());

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    assert!(claim_until_held(&mut b, 12).await);
    let their_epoch = b.lease.as_ref().unwrap().epoch;
    assert!(their_epoch > a.lease.as_ref().unwrap().epoch, "no takeover happened");

    // The arm the run loop drives, not the poll arm.
    let err = a.heartbeat_tick().await.unwrap_err();
    assert!(matches!(err, LeanError::Fenced(_)));

    for (verb, nonce) in [(Verb::Publish, "pub-stranded"), (Verb::Sync, "sync-stranded")] {
        let ack = a
            .read_ack(verb)
            .unwrap_or_else(|| panic!("{verb:?} was stranded: the heartbeat exited unsettled"));
        assert_eq!(ack.status, "refused-fenced");
        assert!(ack.nonces.contains(&nonce.to_string()));
        assert_eq!(ack.observed_epoch, Some(their_epoch));
        assert!(a.load_pending(verb).unwrap().is_none());
    }
    let caps = a.read_capabilities().unwrap();
    assert_eq!(caps.state, "fenced", "the marker still advertises a zombie as live");
    assert!(caps.verbs.is_empty());
}

/// The merge base is rewritten at step 7 — after the manifest CAS and
/// after the GC deletes. A container restart in that window leaves the
/// bucket holding a document THIS workspace wrote and the persisted
/// merge base one generation behind it, so our own entries read as
/// foreign changes at the next merge. delete/modify then resolves
/// conservatively against the agent's own delete: the delete is dropped
/// from a boundary about to be acked, and the path is queued into the
/// inbox as a conflict nobody else ever touched.
///
/// Found by the formal model (tranche 3, product 1) on a strict run.
#[tokio::test]
async fn a_crash_between_the_cas_and_step_7_never_makes_our_own_entry_foreign() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "f.txt", "v1");
    a.run_barrier().await.unwrap();

    // A second publish of the same path...
    write(dir.path(), "f.txt", "v2 is longer");
    backdate_baseline(&a, "f.txt");
    let stale = a.state.load_baseline().unwrap();
    a.run_barrier().await.unwrap();
    let installed = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();

    // ...whose step 7 never ran. The manifest carries the install; the
    // persisted baseline and merge base are as they were before it.
    // The intent journal is NOT rolled back: it is written before the
    // deletes, which is exactly the point.
    a.state.save_baseline(&stale).unwrap();
    assert_ne!(
        stale.inst_base.get("f.txt"),
        Some(&installed.manifest.entries["f.txt"].etag),
        "fixture: the merge base is not actually behind the install"
    );

    // The agent removes the file and declares.
    std::fs::remove_file(dir.path().join("f.txt")).unwrap();
    let r = a.declared_barrier().await.unwrap();

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(
        !m.entries.contains_key("f.txt"),
        "our own install read as a foreign change and swallowed the agent's delete \
         (deleted={:?}, foreign_queued={})",
        r.deleted,
        r.foreign_queued
    );
    let ib = inbox::load(store.as_ref(), &a.cfg).await.unwrap();
    assert!(
        !ib.doc.entries.iter().any(|e| e.path == "f.txt"),
        "a phantom foreign entry was queued for a path only this workspace ever wrote"
    );
}

/// The same window in the gated citation lane: its CAS installs, its
/// baseline rewrite is a separate step, and a crash between them must
/// not turn the boundary it just installed into a foreign change.
#[tokio::test]
async fn a_gated_citation_never_reads_its_own_boundary_as_foreign() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    gated(&mut a);
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "f.txt", "v1");
    a.gated_tick(true).await.unwrap();

    write(dir.path(), "f.txt", "v2 is longer");
    backdate_baseline(&a, "f.txt");
    let stale = a.state.load_baseline().unwrap();
    a.gated_tick(true).await.unwrap();
    let installed = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    assert_eq!(
        installed.manifest.entries["f.txt"].size,
        "v2 is longer".len() as u64,
        "fixture: the second citation never landed"
    );

    // The citation installed; its baseline rewrite did not.
    a.state.save_baseline(&stale).unwrap();
    assert_ne!(
        stale.inst_base.get("f.txt"),
        Some(&installed.manifest.entries["f.txt"].etag),
        "fixture: the merge base is not actually behind the citation"
    );

    std::fs::remove_file(dir.path().join("f.txt")).unwrap();
    a.declared_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(
        !m.entries.contains_key("f.txt"),
        "the citation read its own boundary as foreign and withheld the agent's delete"
    );
}

// ---------------------------------------------------------------------
// The C1-C6 tranche: six findings that survived adversarial
// verification. Each test below is the RED form of one of them.
// ---------------------------------------------------------------------

/// C1 — D10 rule 1. The drain's "did a boundary already run?" guard
/// asks whether any settled ack carries a seq. A SYNC ack always
/// carries one (the manifest it synced against) while publishing
/// nothing, so a pending `.flint/sync` at SIGTERM satisfies the guard
/// and the drain returns without its cite-everything pass. On the
/// routine spot-reclaim path that forfeits every byte written since the
/// last boundary — the exact trap the floor arm's own comment names and
/// guards against.
#[tokio::test]
async fn a_pending_sync_at_sigterm_never_cancels_the_drains_own_boundary() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "seed.txt", "S1");
    a.run_barrier().await.unwrap();

    // The agent asks for news, then keeps working. SIGTERM lands with
    // the sync still owed and the new bytes unpublished.
    write(dir.path(), "work.txt", "W1");
    touch_sentinel(dir.path(), control::SYNC, r#"{"nonce":"s-1"}"#);
    a.poll_sentinels().unwrap();
    assert!(a.load_pending(Verb::Sync).unwrap().is_some());

    let acks = a.drain().await.unwrap();
    assert_eq!(acks.len(), 1, "the drain did not settle the owed sync ack");

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(
        m.entries.contains_key("work.txt"),
        "the drain skipped its cite-everything pass because a SYNC ack carried a seq — \
         every byte since the last boundary died with the emptyDir"
    );
}

/// C1, gated. Same guard, worse blast radius: the staged versions are
/// durable in the bucket but uncited, so they are invisible to every
/// import, DR checkout and successor — recoverable only by hand.
#[tokio::test]
async fn a_gated_drain_cites_its_stage_even_when_a_sync_was_owed() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "seed.txt", "S1");
    a.run_barrier().await.unwrap();
    gated(&mut a);

    write(dir.path(), "ckpt.bin", "C1");
    a.upload_lane().await.unwrap();
    assert_eq!(a.load_stage().unwrap().entries.len(), 1);

    touch_sentinel(dir.path(), control::SYNC, r#"{"nonce":"s-2"}"#);
    a.poll_sentinels().unwrap();
    a.drain().await.unwrap();

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(
        m.entries.contains_key("ckpt.bin"),
        "the gated drain left durable work uncited because a sync ack carried a seq"
    );
    assert!(
        a.load_stage().unwrap().entries.is_empty(),
        "the drain left staged work in the pending record"
    );
}

/// C3 — the withheld tombstone that outlives its file. `withheld_deletes`
/// is insert-only until a citation clears it wholesale, so a
/// delete-then-recreate inside one citation interval puts the same path
/// in BOTH the upsert set and the delete set. `manifest::merge` applies
/// upserts first and deletes second with no upsert check, so the
/// installed boundary omits a file that exists on disk, was staged this
/// pass, and — on a declared boundary — is covered by an ok ack. A
/// sibling sync then DELETES its copy: real byte destruction from a
/// checkpoint-rotation shape.
#[tokio::test]
async fn a_recreated_file_survives_its_own_withheld_tombstone() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "ckpt.bin", "v1");
    a.run_barrier().await.unwrap();
    gated(&mut a);

    // Rotation: the old checkpoint goes, two lane ticks confirm the
    // absence, then the new one lands under the same name.
    std::fs::remove_file(dir.path().join("ckpt.bin")).unwrap();
    a.upload_lane().await.unwrap();
    a.upload_lane().await.unwrap();
    assert!(
        a.load_stage().unwrap().withheld_deletes.contains("ckpt.bin"),
        "the fixture never armed the tombstone"
    );
    // A longer body: same-size, same-second rewrite of a path whose
    // baseline entry survives the withheld delete would scan clean.
    write(dir.path(), "ckpt.bin", "v2-the-replacement");
    a.upload_lane().await.unwrap();
    assert!(
        a.load_stage().unwrap().entries.contains_key("ckpt.bin"),
        "the fixture never staged the replacement"
    );

    a.citation_pass(CitationSource::ForcedLagCap).await.unwrap();

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(
        m.entries.contains_key("ckpt.bin"),
        "the stale tombstone amputated a file that exists on disk and was staged this pass"
    );
    let cited = m.entries["ckpt.bin"].version_id.clone().unwrap();
    let (_, body) = store.get_version(&a.cfg.file_key("ckpt.bin"), &cited).await.unwrap();
    assert_eq!(
        &body[..],
        b"v2-the-replacement",
        "the boundary cited the pre-rotation checkpoint"
    );
}

/// C4 — §2.2's containment rule covers the TARGET; the write goes
/// through a temp sibling nobody validates. `contained_path` refuses a
/// symlinked component, then `write_file_atomic` computes
/// `<name>.flint-sync-tmp` and `fs::write`s it — `File::create`
/// semantics, which follow symlinks. The scanner skips symlinks, so the
/// plant is invisible. The two helpers this tranche ADDED are worse:
/// `control::write_atomic` and `state::write_atomic` have no
/// containment at all and write into directories the app must be able
/// to write, and `.flint/remote.seq` is rewritten on every tick — so
/// the sidecar's own heartbeat performs the write, with no remote
/// cooperation at all.
///
/// The sidecar holds the bucket credentials and runs with no
/// `securityContext`: this is a cross-container write primitive, not a
/// workspace-local nuisance.
#[tokio::test]
async fn a_planted_temp_sibling_is_never_written_through() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "seed.txt", "S1");
    a.run_barrier().await.unwrap();

    // Three victims, one per unvalidated writer.
    let victims = ["consume-victim", "ticker-victim", "state-victim"];
    for v in victims {
        std::fs::write(outside.path().join(v), "ORIGINAL").unwrap();
    }
    // 1. the consume/checkout/sync writer's temp sibling
    std::fs::create_dir_all(dir.path().join("inputs")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("consume-victim"),
        dir.path().join("inputs/config.json.flint-sync-tmp"),
    )
    .unwrap();
    // 2. the control-namespace writer's — rewritten every tick, in the
    //    directory the agent drops its sentinels into
    std::fs::create_dir_all(dir.path().join(super::CONTROL_DIR)).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("ticker-victim"),
        dir.path().join(super::CONTROL_DIR).join("remote.seq.tmp"),
    )
    .unwrap();
    // 3. the state-dir writer's
    std::os::unix::fs::symlink(
        outside.path().join("state-victim"),
        a.cfg.state_dir().join("baseline.tmp"),
    )
    .unwrap();

    // A gateway write lands, and the ordinary barrier does the rest:
    // consume writes the file, the ticker refreshes, the baseline saves.
    hitl_write(&store, &a.cfg, "inputs/config.json", "REMOTE-BYTES", "ui").await.unwrap();
    a.run_barrier().await.unwrap();
    // …and one sentinel honor, which is what drives the ticker and the
    // ack through the control-namespace writer.
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"n-tmp"}"#);
    let acks = a.sentinel_tick().await.unwrap();
    assert_eq!(acks.len(), 1, "the fixture never exercised the control writer");

    for v in victims {
        assert_eq!(
            std::fs::read_to_string(outside.path().join(v)).unwrap(),
            "ORIGINAL",
            "the sidecar wrote through a planted temp sibling ({v}) — an arbitrary-file-write \
             primitive outside the workspace, with the bucket credentials"
        );
    }
    // …and the workspace itself still works.
    assert_eq!(read(dir.path(), "inputs/config.json").as_deref(), Some("REMOTE-BYTES"));
    assert!(control_exists(dir.path(), "remote.seq"));
}

/// C2 — D13's load-bearing premise. §2.4.2 exempts the inbox consume
/// and the citation-repair pass from gating, and D13 leans on it: HITL
/// writes reach pinned readers "through the ungated repair pass, i.e.
/// within one floor". The repair machinery lives ONLY in the fused
/// barrier, which gated mode structurally never runs — its floor tick
/// is the lane, its sentinel honor is lane+citation, its drain likewise.
/// So the consume adopts the acked HITL bytes into the tree, the path
/// is then clean-vs-baseline and never staged, and the manifest goes on
/// citing the PRE-HITL version forever: invisible to every pinned
/// reader, every DR checkout, every sibling sync, with no conflict
/// record and no gauge. On a pure-spot fleet the successor materializes
/// a tree without a write the gateway acked.
#[tokio::test]
async fn a_consumed_hitl_write_is_re_cited_in_gated_mode() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "shared.txt", "AGENT-V1");
    a.run_barrier().await.unwrap();
    gated(&mut a);

    // The gateway acks a UI edit to a path the agent is not touching.
    hitl_write(&store, &a.cfg, "shared.txt", "UI-EDIT", "ui").await.unwrap();
    a.upload_lane().await.unwrap();
    assert_eq!(
        read(dir.path(), "shared.txt").as_deref(),
        Some("UI-EDIT"),
        "the fixture never got as far as the consume"
    );
    assert!(
        a.load_stage().unwrap().entries.is_empty(),
        "the fixture staged the path, so it never exercises the repair gap"
    );

    // One floor passes with the agent quiet.
    let mut stage = a.load_stage().unwrap();
    stage.last_citation_unix -= 3600;
    a.save_stage(&stage).unwrap();

    let due = a.citation_due(false).unwrap();
    assert!(due.is_some(), "no citation is ever due for a consumed HITL write");
    a.citation_pass(due.unwrap()).await.unwrap();

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let e = m.entries.get("shared.txt").expect("the boundary dropped the path entirely");
    let key = a.cfg.file_key("shared.txt");
    let (_, body) = match &e.version_id {
        Some(v) => store.get_version(&key, v).await.unwrap(),
        None => store.get_whole(&key, Some(&e.etag)).await.unwrap(),
    };
    assert_eq!(
        &body[..],
        b"UI-EDIT",
        "the manifest still cites the pre-HITL version — under pinned_reads the acked write \
         is invisible to every checkout, sibling sync and DR re-materialization"
    );
}

/// C5 — the cell where two rules of §2.4.2 collide. D7's entry schema
/// says `version_id: None` ⇒ "today's If-Match GET path verbatim", and
/// calls mixed manifests a PERMANENT reader case. D13 says that under
/// `pinned_reads` readers resolve exclusively by version and never
/// S3-wins-adopt. A gated citation clones the predecessor manifest and
/// stamps `pinned_reads = true` over it, so every path a pre-D7 binary
/// cited keeps `version_id: None` INSIDE a pinned manifest — and
/// checkout's match sends `(true, None)` to the etag arm, whose 412
/// handler adopts the current version. So on the rollout path — enable
/// gated on an existing workspace — the first lane staging of each
/// legacy path makes a concurrent checkout adopt uncited,
/// mid-logical-change bytes through the exact arm the mode exists to
/// close. It does not self-correct: the adopter's baseline records the
/// adopted etag, so the path scans clean and reads unchanged forever.
#[tokio::test]
async fn a_pinned_manifest_never_adopts_current_for_a_legacy_entry() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir_a.path(), "legacy.txt", "L1");
    write(dir_a.path(), "other.txt", "O1");
    a.run_barrier().await.unwrap();

    // What a pre-D7 binary left behind: cited by etag, no version id.
    let loaded = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    let mut m = loaded.manifest.clone();
    m.entries.get_mut("legacy.txt").unwrap().version_id = None;
    manifest::cas_write(store.as_ref(), &a.cfg, &m, Some(&loaded.handle()), 1, "legacy-writer")
        .await
        .unwrap();

    // Gated is switched on. A boundary of its own stamps pinned_reads
    // over the inherited entries.
    gated(&mut a);
    write(dir_a.path(), "other.txt", "O2");
    backdate_baseline(&a, "other.txt");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::ForcedLagCap).await.unwrap();

    // The agent now touches the legacy path. The lane moves its CURRENT
    // version; the citation has not run.
    write(dir_a.path(), "legacy.txt", "L2-MID-CHANGE");
    backdate_baseline(&a, "legacy.txt");
    a.upload_lane().await.unwrap();

    // Anti-vacuity: the uncited version is current at the real key…
    let key = a.cfg.file_key("legacy.txt");
    let (_, raw) = store.get_whole(&key, None).await.unwrap();
    assert_eq!(&raw[..], b"L2-MID-CHANGE", "nothing was actually staged");
    let now = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(now.pinned_reads, "the fixture never got a pinned manifest");

    // …and a successor checks out in that window.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    b.checkout().await.unwrap();
    assert_eq!(
        read(dir_b.path(), "legacy.txt").as_deref(),
        Some("L1"),
        "a pinned checkout S3-wins-adopted uncited mid-change bytes for a legacy entry"
    );
    assert_eq!(read(dir_b.path(), "other.txt").as_deref(), Some("O2"));
}

/// C5, the residue. The backfill answers from the version history, so
/// what is left is the entry it CANNOT answer for: a legacy citation
/// whose etag matches no surviving version. Under `pinned_reads` the
/// current version is exactly what D13 excludes — uncited, possibly
/// mid-change bytes — so the reader must refuse, not adopt. The bytes
/// are not lost; `recover-staged` re-cites forward.
#[tokio::test]
async fn an_unresolvable_legacy_citation_refuses_rather_than_adopts() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir_a.path(), "legacy.txt", "L1");
    write(dir_a.path(), "other.txt", "O1");
    a.run_barrier().await.unwrap();

    let loaded = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    let mut m = loaded.manifest.clone();
    let v1 = m.entries["legacy.txt"].version_id.clone().unwrap();
    m.entries.get_mut("legacy.txt").unwrap().version_id = None;
    manifest::cas_write(store.as_ref(), &a.cfg, &m, Some(&loaded.handle()), 1, "legacy-writer")
        .await
        .unwrap();

    // A mixed-writer bucket: something moves the object on WITHOUT an
    // inbox entry (announce it and the repair pass would answer for it
    // instead), and the cited generation is then reaped — so the
    // citation names bytes no surviving version carries.
    let key = a.cfg.file_key("legacy.txt");
    let body = Bytes::from_static(b"FOREIGN-CURRENT");
    let crc = crc64_nvme(&body);
    store
        .put_whole(
            &key,
            body,
            &PutCondition::IfMatch(m.entries["legacy.txt"].etag.clone()),
            &GenerationStamps {
                generation: 0,
                epoch: 0,
                flush_uuid: "foreign".into(),
                boundary_source: None,
                posix: None,
            },
            crc,
        )
        .await
        .unwrap();
    store.delete_version(&key, &v1).await.unwrap();

    gated(&mut a);
    write(dir_a.path(), "other.txt", "O2");
    backdate_baseline(&a, "other.txt");
    a.upload_lane().await.unwrap();
    let cite = a.citation_pass(CitationSource::ForcedLagCap).await.unwrap();
    assert!(
        !cite.backfilled.contains(&"legacy.txt".to_string()),
        "the fixture left the entry resolvable, so it never reaches the reader rule"
    );
    let installed = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(installed.pinned_reads);
    assert!(
        installed.entries["legacy.txt"].version_id.is_none(),
        "the fixture never produced an unresolvable legacy entry"
    );

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = sidecar(&store, dir_b.path()).await;
    let err = b.checkout().await;
    assert!(
        err.is_err(),
        "a pinned checkout adopted the current version for an unresolvable legacy citation"
    );
    assert_eq!(
        read(dir_b.path(), "legacy.txt"),
        None,
        "the uncited foreign bytes were materialized anyway"
    );
}

/// C6 — the ack that claims a boundary the citation dropped. A gated
/// publish honor runs the citation lane, and the citation drops a
/// staged path when a HITL write lands between the lane's consume and
/// its window. The shipped honor acked `status: "ok"` regardless, with
/// `parked` taken from the LANE only and no field anywhere that could
/// name the dropped path: the agent that declared a point containing p
/// is told "ok, uploaded 1" while the manifest at the acked seq still
/// cites p's previous generation. That is §2.1's D1 corollary
/// falsified, and the drain makes it worst — the last boundary of the
/// workspace's life, with the pending record dying with the emptyDir.
///
/// The rule is tested where it lives, because the drop's own window —
/// between two awaits inside the honor — is not schedulable from a
/// fixture. The stale-base drop is deliberately NOT a lie: it fires
/// only when the foreign bytes are the local bytes.
#[tokio::test]
async fn a_gated_ack_never_claims_a_path_the_citation_dropped() {
    let pending = super::sentinel::PendingSentinel {
        verb: "publish".into(),
        consumed_mtime_unix_ns: 7,
        consumed_at: 0,
        nonces: vec!["n-1".into()],
        note: None,
        scope: None,
        torn: false,
    };
    let lane = super::gated::LaneReport {
        staged: vec!["p".into(), "q".into()],
        staged_bytes: 2,
        ..Default::default()
    };

    // The boundary carried everything declared.
    let clean = super::gated::CitationReport { seq: Some(9), cited: 2, ..Default::default() };
    let ok = super::sentinel::gated_ack(&pending, false, &lane, &clean, Some("e".into()));
    assert_eq!(ok.status, "ok");
    assert!(ok.report.dropped.is_empty());

    // A HITL write landed mid-lane and p was dropped from the boundary.
    let dropped = super::gated::CitationReport {
        seq: Some(9),
        cited: 1,
        dropped_inflight: vec!["p".into()],
        ..Default::default()
    };
    let partial = super::sentinel::gated_ack(&pending, false, &lane, &dropped, Some("e".into()));
    assert_ne!(
        partial.status, "ok",
        "the ack claimed a coherent point that omits a path the agent declared"
    );
    assert_eq!(partial.report.dropped, vec!["p".to_string()]);
    assert_eq!(partial.nonces, vec!["n-1".to_string()], "the agent must still be answered");

    // The stale-base drop is content-equal by construction: the point
    // does carry the agent's bytes, so it stays ok.
    let equal = super::gated::CitationReport {
        seq: Some(9),
        cited: 1,
        dropped_stale_base: vec!["p".into()],
        ..Default::default()
    };
    let still_ok = super::sentinel::gated_ack(&pending, false, &lane, &equal, Some("e".into()));
    assert_eq!(still_ok.status, "ok");
}

/// The other half of C3, and the one TLC found — in a fix that was two
/// hours old. `stage.entries` and `withheld_deletes` carry no ordering,
/// so an overlap between them is ambiguous: delete-then-recreate needs
/// the upsert to win, create-then-delete needs the delete to win, and
/// `manifest::merge` sees only two sets. The shipped merge applied
/// deletes last (right here, wrong for the recreate); the first C3 fix
/// made upserts win (right there, wrong here — a boundary citing a file
/// the agent had deleted, acked ok on a declared boundary). The
/// ordering is only known where it is observed, so the lane cancels
/// each against the other and merge resolves nothing.
///
/// Two shapes, because they reach the stage by different routes: a path
/// the manifest already cites (classify sees the delete) and one staged
/// but never cited (classify cannot see it AT ALL — it has no baseline
/// entry — so the stage carries its own absence memory).
#[tokio::test]
async fn a_delete_cancels_the_version_the_lane_had_already_staged() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "ckpt.bin", "V1");
    a.run_barrier().await.unwrap();
    gated(&mut a);

    // Shape A: a CITED path, re-staged, then removed.
    write(dir.path(), "ckpt.bin", "V2-the-next-one");
    backdate_baseline(&a, "ckpt.bin");
    // Shape B: a scratch file staged but never cited.
    write(dir.path(), "scratch.tmp", "TEMP");
    a.upload_lane().await.unwrap();
    let stage = a.load_stage().unwrap();
    let staged_a = stage.entries["ckpt.bin"].version_id.clone().unwrap();
    let staged_b = stage.entries["scratch.tmp"].version_id.clone().unwrap();

    std::fs::remove_file(dir.path().join("ckpt.bin")).unwrap();
    std::fs::remove_file(dir.path().join("scratch.tmp")).unwrap();
    a.upload_lane().await.unwrap();
    a.upload_lane().await.unwrap();

    let stage = a.load_stage().unwrap();
    assert!(
        stage.withheld_deletes.contains("ckpt.bin"),
        "the fixture never confirmed the cited path's absence"
    );
    assert!(
        !stage.entries.contains_key("ckpt.bin"),
        "the lane kept a staged version for a path it has since seen deleted"
    );
    assert!(
        !stage.entries.contains_key("scratch.tmp"),
        "a staged-but-never-cited path survived its own deletion — classify cannot see it"
    );

    a.citation_pass(CitationSource::ForcedLagCap).await.unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(
        !m.entries.contains_key("ckpt.bin"),
        "the boundary cited a file the agent had deleted before it"
    );
    assert!(
        !m.entries.contains_key("scratch.tmp"),
        "the boundary cited a scratch file that never existed at any coherent point"
    );
    // The uncited versions are reclaimed rather than left as litter.
    for (path, v) in [("ckpt.bin", staged_a), ("scratch.tmp", staged_b)] {
        assert!(
            store.get_version(&a.cfg.file_key(path), &v).await.is_err(),
            "the cancelled staging left an uncited version behind ({path})"
        );
    }
}

// ── Phase 4: the operator-facing surfaces (§2.6) ─────────────────────

/// D12's heartbeat is the ONE request a live sidecar always pays, so it
/// is where the observed-state echo rides (§2.6). Without it the
/// operator can only report what the spec ASKED for: the env read is a
/// fixed list, so `FLINT_SYNC_BOUNDARY_MODE=gated` reaching a
/// pre-boundary binary is ignored in silence and the workspace runs
/// fused cadence behind a green condition — the mixed-version hole D11
/// closes on the agent side and nothing closed on the operator's.
///
/// The count is the part that has to be live rather than derived: the
/// number of durable-but-invisible objects is gated mode's whole
/// exposure, and it exists nowhere the operator can reach.
#[tokio::test]
async fn a_heartbeat_echoes_the_running_mode_into_the_lease_cell() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "seed.txt", "S1");
    let cited = a.run_barrier().await.unwrap().seq.unwrap();
    gated(&mut a);

    // Durable, uncited work standing right now.
    write(dir.path(), "ckpt.bin", "C1");
    a.upload_lane().await.unwrap();
    let staged = a.load_stage().unwrap().entries.len() as u64;
    assert_eq!(staged, 1, "the fixture staged nothing — the echo's count would be 0 == 0");

    a.heartbeat_tick().await.unwrap();

    let cell = store.epoch_read(&a.cfg.epoch_key()).await.unwrap().unwrap();
    let echo: flint_store::LeaseEcho = serde_json::from_str(
        cell.echo.as_deref().expect("the heartbeat carried no observed-state echo"),
    )
    .expect("the echo is not a LeaseEcho");
    assert_eq!(echo.active_boundary_mode, "gated", "the echo reports the spec, not the run");
    assert_eq!(echo.staged_uncited_count, staged, "the gated exposure is not echoed");
    assert_eq!(echo.last_cited_seq, cited, "the echo names no citation");
    assert_eq!(echo.protocol, super::SENTINEL_PROTOCOL);
    assert!(!echo.sidecar_version.is_empty(), "no version ⇒ no mixed-fleet tell");
}

/// D8's refusal arm, as a unit test rather than only as drill leg
/// B24(a): a project-scoped proxy that answers every PUT successfully
/// but strips `x-amz-version-id`. Everything keeps working until a
/// citation needs to NAME a version — at which point pending entries
/// carry `None` and citation falls back to etag semantics on a key
/// whose current version is uncited, which is the torn view gated mode
/// exists to prevent. The probe must refuse, never degrade.
#[tokio::test]
async fn a_version_stripping_proxy_refuses_gated_startup() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    gated(&mut a);

    // Control: the same probe against a conformant store passes, so a
    // later failure tracks the proxy and not the fixture.
    a.gated_startup_check().await.expect("a conformant store must pass the probe");

    store.strip_version_ids(true);
    let err = a.gated_startup_check().await.expect_err("a stripping proxy was accepted");
    let msg = err.to_string();
    assert!(
        msg.contains("x-amz-version-id"),
        "the refusal must name the header a proxy operator can fix: {msg}"
    );
}

/// D9's durable surfacing. `pending.json` lives on the emptyDir — the
/// one thing a pure-spot replacement is guaranteed to destroy — so on
/// this fleet's ROUTINE failure the record that names uncited work dies
/// with the pod that staged it. The summary has to live in the bucket.
///
/// The clearing write is half the contract: a summary still claiming
/// candidates after a citation pages an operator about work that is
/// already visible, and an alert that cries wolf is worse than no
/// alert.
#[tokio::test]
async fn uncited_work_is_surfaced_durably_and_cleared_by_its_citation() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "seed.txt", "S1");
    a.run_barrier().await.unwrap();
    gated(&mut a);

    let key = a.orphans_key();
    write(dir.path(), "ckpt.bin", "C1");
    a.upload_lane().await.unwrap();
    assert_eq!(a.load_stage().unwrap().entries.len(), 1, "the fixture staged nothing");

    let (_, body) = store.get_whole(&key, None).await.expect("no durable orphan summary");
    let doc: super::gated::OrphanDoc = serde_json::from_slice(&body).unwrap();
    assert_eq!(doc.candidates.len(), 1);
    assert_eq!(doc.candidates[0].path, "ckpt.bin");
    assert!(
        doc.candidates[0].version_id.is_some(),
        "a candidate with no version id cannot be recovered by name"
    );

    // The pod dies here in the routine case; the summary must not.
    a.citation_pass(CitationSource::Sentinel).await.unwrap();
    let (_, body) = store.get_whole(&key, None).await.unwrap();
    let doc: super::gated::OrphanDoc = serde_json::from_slice(&body).unwrap();
    assert!(
        doc.candidates.is_empty(),
        "the summary still names work the citation made visible: {:?}",
        doc.candidates
    );
}

/// The summary is written on CHANGE, not on every tick: a re-PUT per
/// lane tick is a request the design does not need, and leg B8's
/// zero-added-cost oracle counts every one of them.
#[tokio::test]
async fn an_unchanged_orphan_set_costs_no_request() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    gated(&mut a);
    write(dir.path(), "ckpt.bin", "C1");
    a.upload_lane().await.unwrap();
    let first = store.head(&a.orphans_key()).await.unwrap().etag;

    // A tick with nothing new to say. Counted in VERSIONS, not etags:
    // the doc's timestamp is second-granular, so two writes inside one
    // second produce identical bytes and an etag oracle would pass with
    // the guard removed — the same "the oracle cannot see the failure"
    // shape the prefix-overlap leg was caught by.
    a.upload_lane().await.unwrap();
    let versions = store
        .list_versions(&a.orphans_key())
        .await
        .unwrap()
        .into_iter()
        .filter(|v| v.key == a.orphans_key())
        .count();
    assert_eq!(versions, 1, "an unchanged candidate set re-wrote the summary");
    assert_eq!(store.head(&a.orphans_key()).await.unwrap().etag, first);
}

// ── Phase 5: the layered doors (§2.5, D14) ───────────────────────────

/// The doors are SUGAR over one consume path, and this is the test that
/// says so: a UDS boundary (`request_boundary`, which is exactly what
/// the socket handler calls) and a file-protocol touch inside the same
/// min-interval coalesce into ONE barrier whose single ack covers both
/// nonces. Two implementations would produce two barriers, or two acks,
/// or an ack that named only one of them.
#[tokio::test]
async fn a_uds_boundary_and_a_file_sentinel_coalesce_into_one_ack() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    // Two honors in one test: the min-interval would defer the second,
    // and the claim under test is about the consume path, not cadence.
    a.cfg.sentinel_min_interval_secs = 0;
    write(dir.path(), "work.txt", "W1");

    // BOTH orders, because they are not symmetric and only one of them
    // discriminates. Settle-before-consume means the FILE path always
    // folds into a standing record, so a socket handler that minted its
    // own record would still look right if the socket went first — the
    // file touch would coalesce into it. File-first is the order that
    // catches it: a second implementation overwrites the standing
    // record and the ack silently loses the file's nonce.
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"file:1"}"#);
    a.poll_sentinels().unwrap();
    a.request_boundary("uds:1", Some("from the socket".into())).unwrap();

    let acks = a.sentinel_tick().await.unwrap();
    assert_eq!(acks.len(), 1, "the two doors produced {} barriers", acks.len());
    let ack = &acks[0];
    assert_eq!(ack.status, "ok");
    for n in ["uds:1", "file:1"] {
        assert!(
            ack.nonces.iter().any(|x| x == n),
            "the ack does not cover {n}: {:?}",
            ack.nonces
        );
    }
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("work.txt"), "the coalesced boundary published nothing");

    // And the other order, for coverage of the coalescing rule itself.
    write(dir.path(), "work2.txt", "W2");
    a.request_boundary("uds:2", None).unwrap();
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"file:2"}"#);
    a.poll_sentinels().unwrap();
    let acks = a.sentinel_tick().await.unwrap();
    assert_eq!(acks.len(), 1);
    for n in ["uds:2", "file:2"] {
        assert!(acks[0].nonces.iter().any(|x| x == n), "socket-first lost {n}");
    }
}

/// The gateway's boundary request is a FIELD on the inbox document, not
/// a fake no-object entry: `consume_inbox` HEADs `file_key(path)` for
/// every entry, so an entry naming no object lands in the NotFound arm
/// as a spurious `consume-object-missing` conflict.
///
/// It is also consumed on the inbox GET the barrier already pays for —
/// the anti-vacuity check here is that no conflict record was minted
/// and the pending sentinel really exists afterwards.
#[tokio::test]
async fn a_gateway_boundary_request_becomes_a_pending_sentinel_with_no_conflict() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();

    inbox::gateway_request(
        store.as_ref(),
        &a.cfg,
        inbox::RequestedVerb::Boundary,
        "ci@example",
    )
    .await
    .unwrap();

    write(dir.path(), "work.txt", "W1");
    a.consume_inbox().await.unwrap();

    let pending = a.load_pending(super::sentinel::Verb::Publish).unwrap();
    let pending = pending.expect("the gateway request minted no pending sentinel");
    assert!(
        pending.nonces.iter().any(|n| n.contains("ci@example")),
        "the requestor is not in the covered nonces: {:?}",
        pending.nonces
    );
    assert_eq!(
        std::fs::read_to_string(a.cfg.state_dir().join("conflicts.jsonl")).unwrap_or_default(),
        "",
        "the boundary request minted a conflict record"
    );

    // Idempotent state, not a queue: a second consume of the SAME
    // request must not mint a second boundary.
    let before = pending.nonces.len();
    a.consume_inbox().await.unwrap();
    assert_eq!(
        a.load_pending(super::sentinel::Verb::Publish).unwrap().unwrap().nonces.len(),
        before,
        "the same gateway request was consumed twice"
    );
}

/// D14, with the failing control the plan asks for: a gateway sync
/// request moves the ticker and mutates NOTHING. `sync` deletes local
/// files for remotely-deleted paths, so performing it on a remote's
/// say-so would upgrade a leaked bearer from "publish, plus hand over
/// these N named objects" to "rewrite and delete across a running
/// agent's tree, at my timing, under a scope I choose".
#[tokio::test]
async fn a_gateway_sync_request_is_carried_and_never_executed() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "keep.txt", "LOCAL");
    a.run_barrier().await.unwrap();

    // A foreign party deletes the file remotely — the exact change a
    // performed sync would apply to the local tree.
    let mut m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    m.manifest.entries.remove("keep.txt");
    m.manifest.seq += 1;
    manifest::cas_write(store.as_ref(), &a.cfg, &m.manifest, Some(&m.handle()), 9, "foreign")
        .await
        .unwrap();

    let tree_before = read(dir.path(), "keep.txt");
    inbox::gateway_request(store.as_ref(), &a.cfg, inbox::RequestedVerb::Sync, "ci@example")
        .await
        .unwrap();
    a.consume_inbox().await.unwrap();

    assert_eq!(
        read(dir.path(), "keep.txt"),
        tree_before,
        "the sidecar EXECUTED a sync on a remote's say-so and rewrote the agent's tree"
    );
    assert!(tree_before.is_some(), "the fixture had nothing to lose");
    let t: control::RemoteSeq = serde_json::from_slice(
        &std::fs::read(dir.path().join(".flint/remote.seq")).unwrap(),
    )
    .unwrap();
    assert!(t.sync_requested_unix.is_some(), "the request was not carried to the agent");
    assert_eq!(t.sync_requested_by.as_deref(), Some("ci@example"));
    // And no publish sentinel was minted: a sync request is not a
    // boundary request wearing a different name.
    assert!(a.load_pending(super::sentinel::Verb::Publish).unwrap().is_none());
}

// ── Phase 6: /metrics (D15) ──────────────────────────────────────────

/// The parity gate: every field in `gauges.json` reaches exactly one
/// metric, and every metric reports a field. Two computations of the
/// same number drift, and the drift is invisible until somebody makes a
/// decision on the wrong one — so there is one struct, one renderer,
/// and this test says so field by field.
#[test]
fn every_gauges_field_reaches_exactly_one_metric() {
    let g = super::Gauges {
        state: "live".into(),
        boundary_mode: "gated".into(),
        rpo_secs: 11,
        visibility_lag_secs: 22,
        staged_uncited_count: 3,
        staged_uncited_bytes: 4096,
        cited_noncurrent_age_max_secs: 33,
        withheld_reason: Some("awaiting-boundary".into()),
        sentinel_budget_remaining: 44,
        forced_citation_count: 5,
        last_boundary: Some(super::gauges::LastBoundary {
            source: "quiescence".into(),
            seq: 66,
            unix: 1_756_000_000,
        }),
        updated_unix: 1_756_000_100,
        last_durable_unix: 1_756_000_050,
        auth_paused_since_unix: Some(1_756_000_010),
    };
    let json = serde_json::to_value(&g).unwrap();
    let fields: Vec<String> = json.as_object().unwrap().keys().cloned().collect();
    assert!(fields.len() >= 13, "the fixture did not populate the struct: {fields:?}");

    for f in &fields {
        assert!(
            super::metrics::COVERED_FIELDS.contains(&f.as_str()),
            "gauges field {f:?} reaches no metric — an operator reading /metrics cannot see \
             what an operator reading gauges.json can"
        );
    }
    for f in super::metrics::COVERED_FIELDS {
        assert!(
            fields.iter().any(|x| x == f),
            "metric table names {f:?}, which is not a gauges field any more"
        );
    }

    // …and the VALUES agree at the same tick, not just the names.
    let text = super::metrics::render(
        &g,
        &super::metrics::Labels { workspace: "proj1".into(), namespace: "agents".into() },
    );
    for (name, want) in [
        ("flint_lean_rpo_seconds", 11u64),
        ("flint_lean_visibility_lag_seconds", 22),
        ("flint_lean_staged_uncited_objects", 3),
        ("flint_lean_staged_uncited_bytes", 4096),
        ("flint_lean_cited_noncurrent_age_max_seconds", 33),
        ("flint_lean_sentinel_budget_remaining", 44),
        ("flint_lean_forced_citations_total", 5),
        ("flint_lean_last_boundary_seq", 66),
        ("flint_lean_boundary_mode", 2),      // gated
        ("flint_lean_withheld_reason", 2),    // awaiting-boundary
        ("flint_lean_last_boundary_source", 2), // quiescence
        ("flint_lean_fenced", 0),
    ] {
        let line = text
            .lines()
            .find(|l| l.starts_with(name) && !l.starts_with('#'))
            .unwrap_or_else(|| panic!("no series for {name}"));
        let got: u64 = line.rsplit(' ').next().unwrap().parse().unwrap();
        assert_eq!(got, want, "{name} disagrees with the gauges it renders");
    }
}

/// The label-key set is exactly `{workspace, namespace}`. The failing
/// control this test exists to be: a per-path metric multiplies series
/// by the workspace's inventory — 250,000 files is the shipped cap —
/// across a 3,000-workspace fleet.
#[test]
fn the_label_key_set_is_exactly_workspace_and_namespace() {
    let g = super::Gauges { state: "live".into(), boundary_mode: "hybrid".into(), ..Default::default() };
    let text = super::metrics::render(
        &g,
        &super::metrics::Labels { workspace: "proj1".into(), namespace: "agents".into() },
    );
    let mut series = 0;
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let labels = line
            .split_once('{')
            .and_then(|(_, r)| r.split_once('}'))
            .map(|(l, _)| l)
            .unwrap_or_else(|| panic!("unlabelled series: {line}"));
        let keys: Vec<&str> =
            labels.split(',').map(|kv| kv.split_once('=').unwrap().0).collect();
        assert_eq!(
            keys,
            vec!["workspace", "namespace"],
            "series carries label keys beyond the fixed set: {line}"
        );
        series += 1;
    }
    assert!(series >= 13, "the renderer emitted almost nothing: {series}");
}

/// A scrape costs zero bucket requests, and the type system is what
/// says so: `render` takes a `Gauges` and nothing else — no store, no
/// async, no stage. This test is the readable form of that argument,
/// and it fails the moment someone gives the renderer a way to reach
/// the bucket.
#[tokio::test]
async fn a_scrape_costs_no_bucket_request() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "work.txt", "W1");
    a.run_barrier().await.unwrap();
    let g = a.write_gauges(false, None).unwrap();

    let before = store.list("").await.unwrap().len();
    for _ in 0..25 {
        let text = super::metrics::render(
            &g,
            &super::metrics::Labels { workspace: "w".into(), namespace: "n".into() },
        );
        assert!(text.contains("flint_lean_rpo_seconds"));
    }
    assert_eq!(
        store.list("").await.unwrap().len(),
        before,
        "25 scrapes changed the bucket"
    );
}

/// `flint-sync status` exists to answer "why is my agent blocked?", and
/// `pending_sentinels` is the field that answers it. It built the
/// pending record's filename a SECOND time, and got it wrong
/// ("pending-publish.json" against the written "publish.pending.json"),
/// so the answer was permanently "nothing pending" on a workspace with
/// a sentinel standing. Found by writing a drill leg that looked for
/// the file by the name the status verb used.
#[tokio::test]
async fn status_reports_a_standing_pending_sentinel() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();

    let before = super::status_report(&a.cfg).unwrap();
    assert!(before.pending_sentinels.is_empty(), "the fixture started with one pending");

    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"s-1"}"#);
    a.poll_sentinels().unwrap();
    assert!(
        a.load_pending(super::sentinel::Verb::Publish).unwrap().is_some(),
        "the fixture consumed nothing — the status field would be empty either way"
    );

    let r = super::status_report(&a.cfg).unwrap();
    assert_eq!(
        r.pending_sentinels,
        vec!["publish".to_string()],
        "status cannot see the pending record it exists to report"
    );
}

/// Checkout's resume rule — "local-wins on present paths" — is right
/// for the case it was written for: a checkout that crashed halfway
/// finds files IT wrote, and re-fetching them would cost bucket GETs
/// for bytes already on disk.
///
/// It is wrong when the manifest MOVED while the pod was down. The
/// resumed checkout then adopts the old generation's bytes and stamps
/// the baseline with the NEW entry's etag, so the workspace holds stale
/// content that every later mechanism believes is published: the scan
/// sees it as clean and never uploads it, and a sync sees
/// baseline == manifest and never re-fetches it. Nothing is loud; the
/// file is simply wrong from then on.
///
/// Reachable on a pure-spot fleet with a gateway: crash mid-checkout,
/// a HITL write lands, the replacement pod resumes.
#[tokio::test]
async fn a_resumed_checkout_never_adopts_a_stale_generation() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();

    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "big.bin", "GENERATION-ONE");
    a.run_barrier().await.unwrap();

    // The replacement pod got as far as materializing generation 1
    // before its checkout died: files on disk, no completion marker.
    write(dir2.path(), "big.bin", "GENERATION-ONE");

    // …and while it was down, generation 2 landed.
    write(dir.path(), "big.bin", "GENERATION-TWO-IS-LONGER");
    a.run_barrier().await.unwrap();
    let cited = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    assert_eq!(
        cited.manifest.entries["big.bin"].size,
        "GENERATION-TWO-IS-LONGER".len() as u64,
        "the fixture never advanced the manifest"
    );
    lease::release(&mut a).await.unwrap();
    drop(a);

    let mut b = sidecar(&store, dir2.path()).await;
    assert!(claim_until_held(&mut b, 8).await);
    assert!(
        !b.state.marker_present(),
        "the fixture left a completion marker — this is the RESUME row"
    );
    b.checkout().await.unwrap();

    let local = read(dir2.path(), "big.bin").unwrap();
    assert_eq!(
        local, "GENERATION-TWO-IS-LONGER",
        "the resumed checkout adopted the STALE generation: the workspace holds {local:?} while \
         the manifest cites the newer bytes, and the baseline claims they are the same — so the \
         scan will never upload it and a sync will never re-fetch it"
    );
}

/// The gateway is a READER of the coherent view, so it owes the same
/// promise `checkout` does: under `pinned_reads` the manifest entry
/// names a `version_id`, and that is what a coherent read resolves.
///
/// Reading by ETag alone breaks precisely when gating is doing its job.
/// The upload lane makes the cited version NONCURRENT, so the current
/// object's etag no longer matches the citation, and an If-Match GET
/// fails its precondition — the gateway turned a perfectly readable
/// cited version into a 409 for every staged-but-uncited file. The
/// human read path went dark for the whole withholding window, which is
/// the window gated mode exists to make invisible *and still readable*.
#[tokio::test]
async fn the_gateway_resolves_the_cited_version_while_newer_bytes_are_staged() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    gated(&mut sc);
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();

    write(dir.path(), "docs/spec.md", "CITED");
    sc.gated_tick(true).await.unwrap();
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;
    assert!(m.pinned_reads, "the fixture did not produce a pinned boundary");
    let cited_seq = m.seq;
    assert!(
        m.entries.get("docs/spec.md").and_then(|e| e.version_id.as_ref()).is_some(),
        "the citation names no version — the leg cannot test version resolution"
    );

    let routes = super::gateway::routes(gw_core(&store));
    let res = gw_req().method("GET").path("/lean/v1/proj1/files/docs/spec.md").reply(&routes).await;
    assert_eq!(res.status(), 200, "the cited version is not readable at all");
    assert_eq!(&res.body()[..], b"CITED");

    // Stage newer bytes WITHOUT citing them: the current version moves,
    // the manifest does not. This is the ordinary gated steady state,
    // not an exotic one.
    write(dir.path(), "docs/spec.md", "STAGED-NEWER");
    sc.upload_lane().await.unwrap();
    let m2 = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m2.seq, cited_seq, "the fixture cited the new bytes — nothing is withheld");
    let (_, cur) = store.get_whole(&sc.cfg.file_key("docs/spec.md"), None).await.unwrap();
    assert_eq!(&cur[..], b"STAGED-NEWER", "the lane did not stage over the real key");

    // The coherent read must still serve the CITED bytes.
    let res = gw_req().method("GET").path("/lean/v1/proj1/files/docs/spec.md").reply(&routes).await;
    assert_eq!(
        res.status(),
        200,
        "the gateway refused a readable cited version while newer bytes were staged"
    );
    assert_eq!(
        &res.body()[..],
        b"CITED",
        "the gateway served uncited, possibly mid-logical-change bytes"
    );
}

/// Which clock published a boundary is a FLEET question, not a local
/// one: the agent gets its answer in the ack, but an operator holding
/// only the bucket — and the gateway's `/status`, which reports this
/// field for every workspace — gets whatever the manifest was stamped
/// with. Gated citations stamped it from the start; the ordinary floor
/// and the sentinel honor left it null, so the field read as "unknown"
/// on every workspace not running gated mode.
#[tokio::test]
async fn every_boundary_says_which_clock_installed_it() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();

    // The ordinary floor tick.
    write(dir.path(), "a.txt", "one");
    sc.floor_tick().await.unwrap();
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("a.txt"), "the fixture published nothing");
    assert_eq!(
        m.boundary_source.as_deref(),
        Some("cadence"),
        "a cadence boundary does not say it was cadence"
    );

    // A sentinel honor.
    // Driven the way the run loop drives it. `settle_pending_at_startup`
    // is the CRASH path and stamps `sentinel-deferred` — correctly, and
    // that is a different clock than the one under test here.
    write(dir.path(), "b.txt", "two");
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"n1"}"#);
    sc.sentinel_tick().await.unwrap();
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("b.txt"), "the sentinel published nothing");
    assert_eq!(
        m.boundary_source.as_deref(),
        Some("sentinel"),
        "a sentinel boundary does not say it was a sentinel"
    );

    // The preStop drain.
    write(dir.path(), "c.txt", "three");
    sc.drain().await.unwrap();
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("c.txt"), "the drain published nothing");
    assert_eq!(
        m.boundary_source.as_deref(),
        Some("drain"),
        "a drain boundary does not say it was a drain"
    );
}

/// The mixed-manifest cell, through the gateway: a pinned boundary
/// carrying an entry the citation could not make version-addressable,
/// whose object has since moved. "Retry" is advice that can never come
/// true — the cited etag is gone — and serving the current version
/// would hand a coherent reader the uncited bytes gating withholds.
#[tokio::test]
async fn the_gateway_never_tells_a_reader_to_retry_a_citation_that_cannot_come_back() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    gated(&mut sc);
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "spec.md", "CITED");
    sc.gated_tick(true).await.unwrap();

    // Strip the version id off the citation, keeping the boundary
    // pinned: this is the cell, not a mode change.
    let loaded = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    let mut m = loaded.manifest.clone();
    m.entries.get_mut("spec.md").unwrap().version_id = None;
    assert!(m.pinned_reads, "the fixture stopped being a pinned boundary");
    manifest::cas_write(store.as_ref(), &sc.cfg, &m, Some(&loaded.handle()), 1, "test-strip")
        .await
        .unwrap();

    // …and the object moves past the cited etag.
    write(dir.path(), "spec.md", "MOVED-PAST-THE-CITATION");
    sc.upload_lane().await.unwrap();

    let routes = super::gateway::routes(gw_core(&store));
    let res = gw_req().method("GET").path("/lean/v1/proj1/files/spec.md").reply(&routes).await;
    assert_ne!(res.status(), 200, "the gateway served uncited bytes to a pinned reader");
    let body = String::from_utf8_lossy(&res.body()[..]).to_string();
    assert!(
        !body.contains("retry"),
        "the gateway told a reader to retry a citation that can never come back: {body}"
    );
    assert!(
        body.contains("recover-staged"),
        "the refusal does not name the way out: {body}"
    );
}

/// The ack and the manifest must never name two different clocks for
/// one boundary. The drain's pending-sentinel arm rewrites the ack to
/// `drain`; if the manifest it installed still said `sentinel-deferred`,
/// the agent's local answer and the fleet's bucket answer would disagree
/// about the same event — and the bucket is the one an operator trusts.
#[tokio::test]
async fn a_drained_sentinel_names_the_same_clock_in_the_ack_and_in_the_bucket() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();

    write(dir.path(), "late.txt", "written before the drain");
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"owed"}"#);
    sc.poll_sentinels().unwrap();
    // Anti-vacuity: the drain must find a PENDING record, not a raw
    // sentinel file — the raw file is the next incarnation's problem and
    // would send this leg down the cite-everything arm instead.
    assert!(
        sc.load_pending(Verb::Publish).unwrap().is_some(),
        "no pending record — the drain would not take the owed-ack arm"
    );

    let acks = sc.drain().await.unwrap();
    let ack = acks.iter().find(|a| a.nonces.iter().any(|n| n == "owed")).expect("owed ack unsettled");
    assert_eq!(ack.boundary, "drain");
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("late.txt"), "the drain published nothing");
    assert_eq!(
        m.boundary_source.as_deref(),
        Some("drain"),
        "the ack says drain and the bucket says something else"
    );
}

/// The same rule on the GATED path, which is a separate honor with its
/// own citation source — and where the first fix missed it. B11a caught
/// this one in the cluster before the battery did.
#[tokio::test]
async fn a_gated_drain_names_the_same_clock_in_the_ack_and_in_the_bucket() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    gated(&mut sc);
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();

    write(dir.path(), "staged.txt", "staged before the drain");
    sc.upload_lane().await.unwrap();
    // Anti-vacuity: uncited work must be standing, or the drain has
    // nothing to cite and the leg is about an event that never happened.
    assert!(
        !sc.load_stage().unwrap().entries.is_empty(),
        "nothing staged uncited — the gated drain would have nothing to do"
    );
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"owed"}"#);
    sc.poll_sentinels().unwrap();
    assert!(sc.load_pending(Verb::Publish).unwrap().is_some());

    let acks = sc.drain().await.unwrap();
    let ack = acks.iter().find(|a| a.nonces.iter().any(|n| n == "owed")).expect("owed ack unsettled");
    assert_eq!(ack.boundary, "drain");
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("staged.txt"), "the gated drain cited nothing");
    assert_eq!(
        m.boundary_source.as_deref(),
        Some("drain"),
        "the gated drain's ack says drain and the bucket says something else"
    );
}

/// A LONG BARRIER MUST NOT STARVE THE LEASE.
///
/// The run loop is one `tokio::select!`, and its branches are mutually
/// exclusive: while the floor arm's `floor_tick().await` runs — and that
/// call contains the entire upload loop — the renewal arm cannot fire.
/// Two things follow, and the second is worse than the first:
///
///   - a deposed straggler cannot learn it was deposed, because the
///     renewal CAS that would tell it is starved by the very barrier it
///     is executing (chaos C3's 7,591 post-deposal PUTs, drill B12);
///   - a HEALTHY sidecar can depose ITSELF. Takeover is QUIET_POLLS(6) x
///     10 s = 60 s, so any barrier that outruns that window stops
///     renewing and a standby legitimately takes the lease from a live
///     writer. The 0b measurements put a 1M-file checkout at 7 m 05 s.
///
/// B17 missed this: it storms sentinels under an HOUR-LONG floor, so its
/// barriers are short and renewals flow (22 renewals, 26 s max gap). The
/// starvation shape is a long barrier, which no leg had.
#[tokio::test]
async fn a_long_barrier_does_not_starve_the_lease_renewal() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();

    // Enough files that the upload phase spans more than one chunk.
    for i in 0..600 {
        write(dir.path(), &format!("f{i:04}.txt"), &format!("body-{i}"));
    }

    // The lease is overdue: the cell has not been touched for longer
    // than the renewal cadence. Anti-vacuity — if it were fresh, a
    // renewal would be correct to skip and the leg would prove nothing.
    let key = sc.cfg.epoch_key();
    store.backdate_epoch(&key, 120);
    let before = store.epoch_read(&key).await.unwrap().unwrap().last_renew_unix;

    sc.run_barrier().await.unwrap();

    let after = store.epoch_read(&key).await.unwrap().unwrap().last_renew_unix;
    assert!(
        after > before,
        "the barrier ran to completion without renewing an overdue lease \
         (last_renew {before:?} -> {after:?}); a barrier longer than the \
         60 s takeover window would hand the lease to a standby while this \
         writer was perfectly alive"
    );
}

/// AUDIT of the B11b class — a fixture whose cases all coincide, so an
/// asymmetry no case exercises rides through green.
///
/// `PendingStage` carries NO seq and no manifest stamp, `load_stage`
/// deserializes whatever is on disk, and `gated_startup_check` runs
/// only the versioning conformance probe. Meanwhile `citation_pass`
/// clears the stage LAST — after the manifest CAS, after the reaper,
/// after the baseline save and the intent clear. So a process that dies
/// anywhere in that window leaves a stage naming versions the installed
/// manifest now CITES, and the very next lane pass reads exactly that
/// field to decide what to reclaim:
///
///     let superseded = stage.entries.get(path).and_then(|p| p.version_id...)
///     ...
///     if Some(&vid) != entry.version_id.as_ref() { delete_version(key, vid) }
///
/// Every existing lane fixture stages and cites in one uninterrupted
/// pass, so `superseded` is always an UNCITED version and the two sets
/// coincide. This is the case where they do not.
///
/// AND IT NEEDS NO CRASH. Between the CAS and the clear sit four
/// ordinary `?` returns — the withheld-delete GC's `store.delete`, its
/// HEAD arm, `append_conflict`, and `reclaim_superseded`, which itself
/// awaits `renew_if_due` and `verify_not_deposed_pub`. One transient
/// store error on any of them returns Err from `citation_pass` with the
/// boundary already installed and the stage still standing. This test
/// EMULATES that state (it restores the pre-clear stage) rather than
/// injecting the fault; the reachability is in the four call sites.
///
/// Contrast `reclaim_superseded`, which fails CLOSED: it will not
/// reclaim unless the installed manifest names a version for the path,
/// it skips `superseded == keep`, and it never touches `is_current`.
/// The lane's reclaim has exactly one guard — "not the version I just
/// wrote" — and no reference to the installed manifest at all. This is
/// U2's rule applied at one of the two sites that delete versions.
#[tokio::test]
async fn a_stage_that_outlived_its_citation_must_not_reap_the_cited_version() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    gated(&mut a);
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();

    write(dir.path(), "f.txt", "v1");
    a.upload_lane().await.unwrap();
    // The stage exactly as it stands the instant before a citation
    // would clear it.
    let pre_clear = a.load_stage().unwrap();
    let staged_vid = pre_clear.entries["f.txt"].version_id.clone().unwrap();

    a.citation_pass(CitationSource::Sentinel).await.unwrap();
    let installed = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let cited_vid = installed.entries["f.txt"].version_id.clone().unwrap();
    assert_eq!(
        cited_vid, staged_vid,
        "precondition: the citation named the version the lane staged"
    );

    // THE CRASH: the process dies between the manifest CAS and
    // `save_stage`. The emptyDir survives — this is a restart, not a
    // pod replacement — so the pre-citation stage is what the next
    // incarnation loads.
    a.save_stage(&pre_clear).unwrap();

    // The agent goes on working. Nothing exotic: one more write.
    // A DIFFERENT LENGTH, deliberately: `v1` -> `v2` is size-identical
    // and lands in the same mtime second, so the scan cannot see it and
    // the lane stages nothing. The first draft of this leg did exactly
    // that and passed green having never reached the branch under test.
    write(dir.path(), "f.txt", "v2 — the agent keeps working");
    let lane = a.upload_lane().await.unwrap();

    // ANTI-VACUITY: the leg is worthless unless the second lane pass
    // actually re-staged this path AND actually reached the reclaim
    // branch that reads `superseded`.
    assert!(
        lane.staged.contains(&"f.txt".to_string()),
        "the second lane pass never re-staged f.txt (staged={:?} parked={:?}) — \
         this leg never reached the reclaim branch",
        lane.staged, lane.parked
    );
    assert_eq!(
        lane.superseded_recorded, 1,
        "the lane did not reach the branch under test at all — it neither recorded \
         nor reclaimed, so this leg proves nothing"
    );

    // No citation has run since, so the boundary still names v1.
    let still = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(
        still.entries["f.txt"].version_id.as_ref(),
        Some(&cited_vid),
        "precondition: no citation ran, so the installed boundary still cites v1"
    );
    store
        .get_version(&a.cfg.file_key("f.txt"), &cited_vid)
        .await
        .expect("the lane reaped the version the INSTALLED MANIFEST CITES");
}

/// The companion to the leg above, and the one that actually holds the
/// FIX in place. That leg proves the lane no longer deletes; this one
/// proves the reaper's drain will not delete either, in the one case
/// where a recorded id is still the cited one.
///
/// It is reachable through the merge, not through a crash: a citation
/// whose merge resolves foreign-wins on a path — or which DROPS the
/// path because a HITL write is in flight over it — installs a boundary
/// that still cites the OLD version, which is precisely the version the
/// lane recorded as superseded. `reclaim_superseded` is driven straight
/// here, the way the `base_version_id` guard's own test does, so the
/// arm under test is the only thing in the frame.
///
/// Without the `Some(vid) == keep` skip this passes nothing: removing
/// that guard was mutation-tested and left the whole battery green
/// until this leg existed.
#[tokio::test]
async fn the_drain_never_reclaims_a_recorded_version_the_boundary_still_cites() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    gated(&mut a);
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();

    write(dir.path(), "m.txt", "generation one");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();
    let installed = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let cited = installed.entries["m.txt"].version_id.clone().unwrap();

    // Stage a NEWER generation without citing it. This is the ordinary
    // gated state and it is what makes the cited version NONCURRENT —
    // D8's inversion, stated in the code's own comments. It also puts
    // the cited version outside the `is_current` guard's reach, so the
    // `keep` guard is the ONLY thing left protecting it. The first
    // draft of this leg skipped this step, left the cited version
    // current, and survived the mutation that deletes the guard.
    write(dir.path(), "m.txt", "generation two, longer than the first");
    a.upload_lane().await.unwrap();

    // The lane recorded this very version as superseded — and then the
    // merge resolved onto it, so the installed boundary still cites it.
    let mut stage = a.load_stage().unwrap();
    stage.pending_reclaims.insert("m.txt".to_string(), vec![cited.clone()]);

    // ANTI-VACUITY, stated as the two facts that matter: the version is
    // reachable (it is in the listing) and it is NOT protected by the
    // is_current guard. Without both, removing `keep` changes nothing
    // and this leg is decorative.
    let versions = store.list_versions(&a.cfg.file_key("m.txt")).await.unwrap();
    let v = versions
        .iter()
        .find(|v| v.version_id == cited)
        .expect("the cited version is not in the listing — the pass would skip it anyway");
    assert!(
        !v.is_current,
        "the cited version is still CURRENT, so `is_current` protects it and this leg \
         cannot isolate the `keep` guard"
    );
    assert!(
        installed.entries.contains_key("m.txt"),
        "the boundary does not cite this path, so `keep` is None and the guard is moot"
    );

    let upserts = std::collections::BTreeMap::new();
    let mut report = Default::default();
    a.reclaim_superseded(&upserts, &installed, &stage, &mut report).await.unwrap();

    assert_eq!(
        report.versions_reclaimed, 0,
        "the drain reclaimed a version the installed boundary CITES"
    );
    store
        .get_version(&a.cfg.file_key("m.txt"), &cited)
        .await
        .expect("the drain deleted the version the installed manifest CITES");
}

/// COVERAGE AUDIT: `sync`'s dirty-conflict arm — the branch that stops
/// a sync overwriting work the agent has not published — was never
/// executed by any battery fixture. `sync_scan_first_dirty_wins_clean_
/// applies` exercises the CLEAN half and `sync_rehonor_no_phantom_
/// conflicts` exercises the identical-content half, which returns at
/// the `identical` early-continue above it. Nothing reached the arm
/// that writes `sync-dirty` and skips the write.
///
/// The formal model covers the RULE (`LeanSyncStaleDirt` refutes
/// judging dirt from the last barrier's snapshot). This covers the Rust
/// that implements it, which is a different artifact.
///
/// Two paths, deliberately: a clean one that MUST be applied alongside
/// the dirty one that must not. Without the clean control, a sync that
/// silently did nothing at all would satisfy every assertion here.
#[tokio::test]
async fn sync_refuses_to_clobber_a_locally_dirty_path_and_says_so() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "dirty.txt", "published v1");
    write(dir.path(), "clean.txt", "published v1");
    a.run_barrier().await.unwrap();

    // A sibling installs new generations of BOTH paths.
    let loaded = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    let mut theirs = loaded.manifest.clone();
    theirs.seq += 1;
    for (path, content) in
        [("dirty.txt", "foreign bytes for dirty"), ("clean.txt", "foreign bytes for clean")]
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
    manifest::cas_write(store.as_ref(), &a.cfg, &theirs, Some(&loaded.handle()), 0, "sibling")
        .await
        .unwrap();

    // The agent edits one of them locally and does NOT publish. A
    // different length, so the scan can actually see it.
    write(dir.path(), "dirty.txt", "the agent's own unpublished work, longer");

    let r = a.sync().await.unwrap();

    // ANTI-VACUITY (1): the clean path really was applied, so a sync
    // that did nothing cannot pass this leg.
    assert_eq!(
        read(dir.path(), "clean.txt").unwrap(),
        "foreign bytes for clean",
        "the control path was not applied — this sync did nothing at all"
    );
    assert!(r.applied.contains(&"clean.txt".to_string()));

    // ANTI-VACUITY (2): the two versions genuinely differ, or
    // "preserved" is trivially true.
    assert_ne!(
        read(dir.path(), "dirty.txt").unwrap(),
        "foreign bytes for dirty",
        "local and remote bytes are identical — nothing was in conflict"
    );

    // The rule: the agent's unpublished bytes stand.
    assert_eq!(
        read(dir.path(), "dirty.txt").unwrap(),
        "the agent's own unpublished work, longer",
        "sync CLOBBERED work the agent had not published"
    );
    assert!(
        r.conflicts.contains(&"dirty.txt".to_string()),
        "the conflict was not surfaced in the report: {:?}",
        r.conflicts
    );
    let conflicts = a.state.load_conflicts().unwrap();
    assert!(
        conflicts.iter().any(|c| c.kind == "sync-dirty" && c.path == "dirty.txt"),
        "no sync-dirty record was written: {conflicts:?}"
    );
}

/// COVERAGE AUDIT: `manifest::merge`'s "our delete does not apply"
/// arms. All eleven delete-merges in the battery took `(Some, Some)`
/// with EQUAL etags and removed the entry; the `(None, None)` arm, the
/// `_ => false` arm, and the `parked` skip were never executed at all.
///
/// The arm that matters is `(Some, None)` — present in theirs, absent
/// from our merge base, i.e. a FOREIGN ADD at a path we are deleting.
/// Flipping `_ => false` to `true` deletes somebody else's new file and
/// the whole battery stays green. `merge` is a pure function over plain
/// maps, so every arm is pinned here directly rather than reached
/// through a barrier that can only produce the easy one.
#[tokio::test]
async fn merge_applies_a_local_delete_only_where_theirs_is_unchanged() {
    fn entry(etag: &str) -> manifest::LeanEntry {
        manifest::LeanEntry {
            key: "tenant/proj1/files/p.txt".into(),
            etag: etag.into(),
            crc64_b64: None,
            size: 3,
            mode: 0o644,
            mtime_unix: 0,
            generation: 1,
            epoch: 0,
            version_id: None,
        }
    }
    // (name, theirs entry, base etag, parked, must_survive_our_delete)
    let cases: Vec<(&str, Option<&str>, Option<&str>, bool, bool)> = vec![
        ("theirs unchanged since our base", Some("e1"), Some("e1"), false, false),
        ("FOREIGN MODIFY under our delete", Some("e2"), Some("e1"), false, true),
        ("FOREIGN ADD under our delete", Some("e2"), None, false, true),
        ("absent from both", None, None, false, false),
        ("already deleted by someone else", None, Some("e1"), false, false),
        ("parked: never resolved this pass", Some("e1"), Some("e1"), true, true),
    ];
    for (name, theirs_etag, base_etag, is_parked, must_survive) in cases {
        let mut theirs = manifest::LeanManifest::default();
        if let Some(e) = theirs_etag {
            theirs.entries.insert("p.txt".to_string(), entry(e));
        }
        let mut base = std::collections::BTreeMap::new();
        if let Some(b) = base_etag {
            base.insert("p.txt".to_string(), b.to_string());
        }
        let mut deletes = std::collections::BTreeSet::new();
        deletes.insert("p.txt".to_string());
        let mut parked = std::collections::BTreeSet::new();
        if is_parked {
            parked.insert("p.txt".to_string());
        }

        // ANTI-VACUITY: the case is only the case if the inputs say so.
        assert_eq!(theirs.entries.contains_key("p.txt"), theirs_etag.is_some(), "{name}");
        assert_eq!(base.contains_key("p.txt"), base_etag.is_some(), "{name}");

        let (merged, _foreign) =
            manifest::merge(&base, &theirs, &Default::default(), &deletes, &parked);

        assert_eq!(
            merged.entries.contains_key("p.txt"),
            must_survive,
            "{name}: our local delete {} the entry",
            if must_survive { "destroyed" } else { "failed to remove" }
        );
        // Where it survives because THEIRS moved, it must survive as
        // THEIRS — not as some merged-in ghost of our own.
        if must_survive {
            if let Some(e) = theirs_etag {
                assert_eq!(merged.entries["p.txt"].etag, e, "{name}: wrong bytes survived");
            }
        }
    }
}

/// COVERAGE AUDIT: `checkout`'s resume-adoption guard. `local_
/// crc64_b64` is never called by any battery fixture — the entire
/// content check is dead in the battery — because a second checkout
/// short-circuits at `marker_present()` and never reaches the body.
///
/// The hazard is the one the code's own comment states: a checkout that
/// died halfway leaves generation N on disk while the manifest MOVES
/// (a HITL write, a sibling's barrier — routine on this fleet).
/// Adopting on size alone stamps the baseline with the NEW entry's etag
/// over the OLD content; the scan then reads the file as clean and
/// never uploads it, sync reads baseline == manifest and never
/// re-fetches it, and the workspace holds bytes nothing will ever
/// reconcile. Silent and permanent, in the code's words.
///
/// Both halves are asserted: a genuinely identical file must be ADOPTED
/// (or the leg is just "checkout re-fetches everything"), and a
/// same-length-different-bytes file must be RE-FETCHED.
#[tokio::test]
async fn a_resumed_checkout_adopts_identical_bytes_and_refetches_same_size_impostors() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "same.txt", "these bytes never move");
    write(dir.path(), "impostor.txt", "AAAA");
    a.run_barrier().await.unwrap();

    // A sibling replaces impostor.txt with the SAME NUMBER OF BYTES.
    let loaded = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    let mut theirs = loaded.manifest.clone();
    theirs.seq += 1;
    let key = a.cfg.file_key("impostor.txt");
    let body = Bytes::from_static(b"BBBB");
    let meta = store
        .put_whole(
            &key,
            body,
            &PutCondition::IfMatch(store.head(&key).await.unwrap().etag),
            &GenerationStamps {
                generation: 2,
                epoch: 0,
                flush_uuid: "sibling".into(),
                boundary_source: None,
                posix: None,
            },
            crc64_nvme(&Bytes::from_static(b"BBBB")),
        )
        .await
        .unwrap();
    let e = theirs.entries.get_mut("impostor.txt").unwrap();
    e.etag = meta.etag.clone();
    e.crc64_b64 = meta.crc64_b64.clone();
    e.size = meta.size;
    e.generation = 2;
    manifest::cas_write(store.as_ref(), &a.cfg, &theirs, Some(&loaded.handle()), 0, "sibling")
        .await
        .unwrap();

    // ANTI-VACUITY: size alone cannot tell these apart. If the manifest
    // entry and the on-disk file differed in length, the cheap check
    // would catch it and the crc would never be consulted.
    assert_eq!(
        theirs.entries["impostor.txt"].size,
        std::fs::metadata(dir.path().join("impostor.txt")).unwrap().len(),
        "the impostor differs in SIZE — this leg would pass without any crc at all"
    );
    assert_eq!(read(dir.path(), "impostor.txt").unwrap(), "AAAA");

    // A checkout that died halfway: the files are on disk, the marker
    // was never written. Without this the resume row returns at
    // `marker_present()` and the adoption code is unreachable — which
    // is exactly why no fixture had ever run it.
    std::fs::remove_file(dir.path().join(".flint-sync/checkout-complete")).unwrap();
    let r = a.checkout().await.unwrap();

    assert!(
        r.skipped_present >= 1,
        "nothing was adopted — the leg degenerates into 'checkout re-fetches everything'"
    );
    assert_eq!(
        read(dir.path(), "same.txt").unwrap(),
        "these bytes never move",
        "an identical file was not left alone"
    );
    assert_eq!(
        read(dir.path(), "impostor.txt").unwrap(),
        "BBBB",
        "checkout ADOPTED a same-size file whose bytes are not the cited bytes — \
         the baseline now attests content the workspace does not hold"
    );
}

/// COVERAGE AUDIT: `sync` under `pinned_reads` — D13's promise that a
/// reader resolves the CITED version and never the current one. The
/// whole `pinned_version` branch was never executed: every sync fixture
/// ran against a cadence manifest, where `pinned_reads` is false and
/// the `else { None }` arm is taken.
///
/// The state that makes it matter is the ordinary gated one: a
/// citation names V2 while the lane has already staged an uncited V3
/// over it, so the CURRENT version of a real `files/` key is not the
/// cited one. A sync that ignored `pinned_reads` would take the
/// `get_whole(key, If-Match etag_of_V2)` path, 412 against V3, hit the
/// `continue` and leave the workspace on V1 — no error, no conflict
/// record, and a path that never converges.
#[tokio::test]
async fn sync_under_pinned_reads_resolves_the_cited_version_not_the_current_one() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = sidecar(&store, dir.path()).await;
    gated(&mut a);
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "d.txt", "cited generation one");
    a.upload_lane().await.unwrap();
    a.citation_pass(CitationSource::Sentinel).await.unwrap();

    let key = a.cfg.file_key("d.txt");
    let loaded = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    assert!(loaded.manifest.pinned_reads, "the gated citation did not pin reads");

    // A sibling cites generation two...
    let v2_body = Bytes::from_static(b"cited generation TWO");
    let v2 = store
        .put_whole(
            &key,
            v2_body.clone(),
            &PutCondition::IfMatch(store.head(&key).await.unwrap().etag),
            &GenerationStamps {
                generation: 2,
                epoch: 0,
                flush_uuid: "sibling".into(),
                boundary_source: None,
                posix: None,
            },
            crc64_nvme(&v2_body),
        )
        .await
        .unwrap();
    let mut theirs = loaded.manifest.clone();
    theirs.seq += 1;
    theirs.pinned_reads = true;
    {
        let e = theirs.entries.get_mut("d.txt").unwrap();
        e.etag = v2.etag.clone();
        e.crc64_b64 = v2.crc64_b64.clone();
        e.size = v2.size;
        e.generation = 2;
        e.version_id = v2.version_id.clone();
    }
    manifest::cas_write(store.as_ref(), &a.cfg, &theirs, Some(&loaded.handle()), 0, "sibling")
        .await
        .unwrap();

    // ...and then stages an UNCITED generation three over it. This is
    // not exotic: it is what the gated lane does every floor tick.
    let v3_body = Bytes::from_static(b"uncited generation three, staged");
    let v3 = store
        .put_whole(
            &key,
            v3_body.clone(),
            &PutCondition::IfMatch(v2.etag.clone()),
            &GenerationStamps {
                generation: 3,
                epoch: 0,
                flush_uuid: "sibling".into(),
                boundary_source: None,
                posix: None,
            },
            crc64_nvme(&v3_body),
        )
        .await
        .unwrap();

    // ANTI-VACUITY: the cited version must NOT be the current one, or
    // "resolved the cited version" is indistinguishable from "read the
    // key".
    let versions = store.list_versions(&key).await.unwrap();
    let cited_vid = theirs.entries["d.txt"].version_id.clone().unwrap();
    assert!(
        versions.iter().any(|v| v.version_id == v3.version_id.clone().unwrap() && v.is_current),
        "the uncited generation is not current — the two reads would agree"
    );
    assert_ne!(cited_vid, v3.version_id.clone().unwrap());

    let r = a.sync().await.unwrap();

    assert!(
        r.applied.contains(&"d.txt".to_string()),
        "sync applied nothing — it took the 412 path and left the workspace behind: {r:?}"
    );
    assert_eq!(
        read(dir.path(), "d.txt").unwrap(),
        "cited generation TWO",
        "sync did not land the CITED bytes"
    );
    assert_ne!(
        read(dir.path(), "d.txt").unwrap(),
        "uncited generation three, staged",
        "sync served the CURRENT version over the cited one — D13's promise inverted"
    );
}

/// MEASUREMENT, not an assertion: what counting recorded reclaims
/// toward `stagedBacklogCapObjects` costs a fleet.
///
/// The change makes a hot path force citations it previously never
/// triggered, because the old predicate counted staged PATHS and a file
/// rewritten every tick is one path forever. What that costs depends on
/// a rate nobody should guess at — how fast `pending_reclaims` actually
/// grows per lane tick — so it is measured here rather than derived,
/// along with the per-tick and per-citation request counts that price
/// it. Run with `--nocapture` to read the table.
#[tokio::test]
async fn measure_backlog_cap_footprint() {
    for hot in [1usize, 10, 100] {
        let store = Arc::new(MemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        let mut a = sidecar(&store, dir.path()).await;
        gated(&mut a);
        // The lag cap and quiescence must be unreachable, or they, not
        // the backlog cap, decide when a citation fires and the
        // measurement is of the wrong thing.
        a.cfg.visibility_lag_bound_secs = Some(86_400);
        a.cfg.quiesce_bound_secs = 86_400;
        assert!(claim_until_held(&mut a, 3).await);
        a.checkout().await.unwrap();

        const TICKS: usize = 12;
        let mut lane_ops = 0u64;
        let mut lane_shape = Default::default();
        let mut recorded_at = vec![];
        for t in 0..TICKS {
            for h in 0..hot {
                // Length varies per tick: a same-size rewrite inside one
                // mtime second is invisible to the scan, which would
                // measure a workload that never happened.
                write(dir.path(), &format!("hot{h:03}.txt"), &"x".repeat(8 + t * 3 + h));
            }
            store.reset_op_counts();
            a.upload_lane().await.unwrap();
            lane_ops += store.total_ops();
            if t == TICKS - 1 {
                lane_shape = store.op_counts();
            }
            let st = a.load_stage().unwrap();
            let rec: usize = st.pending_reclaims.values().map(|v| v.len()).sum();
            recorded_at.push((st.entries.len(), rec));
        }

        store.reset_op_counts();
        a.citation_pass(CitationSource::Sentinel).await.unwrap();
        let cite_ops = store.total_ops();
        let cite_shape = store.op_counts();

        let (entries, rec_last) = *recorded_at.last().unwrap();
        // Growth per tick, MEASURED across the run rather than assumed
        // to be one-per-path.
        let rate = rec_last as f64 / (TICKS - 1) as f64;
        let cap = 5000f64; // the shipped default
        let old_ticks = if (entries as f64) >= cap { 1.0 } else { f64::INFINITY };
        let new_ticks = if rate > 0.0 { (cap - entries as f64) / rate } else { f64::INFINITY };

        eprintln!(
            "\nhot paths = {hot}\n  \
             stage entries after {TICKS} ticks : {entries}\n  \
             recorded reclaims                : {rec_last}  (measured {rate:.2}/tick)\n  \
             lane ops/tick                    : {:.1}  {:?}\n  \
             citation ops                     : {cite_ops}  {:?}\n  \
             ticks to forced citation, OLD    : {}\n  \
             ticks to forced citation, NEW    : {:.0}",
            lane_ops as f64 / TICKS as f64,
            lane_shape,
            cite_shape,
            if old_ticks.is_finite() { "1".to_string() } else { "never".to_string() },
            new_ticks,
        );
    }
}


/// T1.2's second half. `/status` reports the boundary source from the
/// manifest OBJECT STAMP, so a writer that leaves the stamp behind
/// makes the workspace report an unknown clock.
///
/// `rotate_for_takeover` clones the standing manifest and re-CASes it
/// through `cas_write` — which passes `boundary_source: None`. The
/// DOCUMENT carried the source through the clone; the STAMP did not.
/// That is the GET/HEAD divergence `cas_write_stamped` documents as
/// forbidden, and it was invisible for exactly as long as every
/// reader used GET.
#[tokio::test]
async fn a_takeover_rotation_carries_the_boundary_stamp_with_the_document() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = sidecar(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "f.txt", "v1");
    sc.run_barrier().await.unwrap();

    // Install a manifest that names its clock, the way a sentinel
    // honor or a gated citation does.
    let loaded = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    manifest::cas_write_stamped(
        store.as_ref(),
        &sc.cfg,
        &loaded.manifest,
        Some(&loaded.handle()),
        1,
        "u-cited",
        Some("sentinel"),
    )
    .await
    .unwrap();

    // The citation's clock lives on the POINTER now — one small object
    // that a reader gets in full, instead of an object stamp that had to
    // be kept in sync with a document nobody wanted to download.
    let lp = manifest::load_pointer(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert_eq!(
        lp.pointer.boundary_source.as_deref(),
        Some("sentinel"),
        "precondition: the cited manifest names its clock on the pointer"
    );

    // Now the successor rotates the fence.
    manifest::rotate_for_takeover(store.as_ref(), &sc.cfg, 2).await.unwrap().unwrap();

    let lp = manifest::load_pointer(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    let doc = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert_eq!(
        lp.pointer.boundary_source.as_deref(),
        Some("sentinel"),
        "rotation must not drop the boundary stamp: a pointer reader would report an \
         unknown clock for a workspace whose document still says `sentinel`"
    );
    assert_eq!(
        lp.pointer.boundary_source, doc.manifest.boundary_source,
        "the pointer and the document a reader assembles from it must never disagree"
    );
    assert_eq!(lp.pointer.seq, doc.manifest.seq, "the pointer's seq IS the document's seq");
}


/// A minimal entry for tests that care about the manifest's SHAPE
/// rather than any file's content.
fn entry(name: &str) -> manifest::LeanEntry {
    manifest::LeanEntry {
        key: format!("{PREFIX}/files/{name}"),
        etag: "e".into(),
        crc64_b64: None,
        size: 1,
        mode: 0o100644,
        mtime_unix: 0,
        generation: 1,
        epoch: 1,
        version_id: None,
    }
}

// ── the manifest pointer layout ─────────────────────────────────────
// Design of record: docs/plans/flint-lean-manifest-pointer-design.md.

/// Everything a writer publishes lands in TWO objects, and the entries
/// object is immutable — which is what lets a rotation be a small write.
#[tokio::test]
async fn a_publish_writes_an_immutable_generation_and_a_pointer_that_names_it() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let mut m = manifest::LeanManifest::default();
    m.seq = 1;
    m.entries.insert("a.txt".into(), entry("x"));
    manifest::cas_write(store.as_ref(), &cfg, &m, None, 3, "first").await.unwrap();

    let lp = manifest::load_pointer(store.as_ref(), &cfg).await.unwrap().unwrap();
    assert_eq!(lp.pointer.seq, 1);
    assert_eq!(lp.pointer.entries_seq, Some(1));
    assert_eq!(lp.pointer.entries_key, lp.pointer.entries_key.clone());
    // The generation object exists and the pointer names it.
    assert!(store.head(lp.pointer.entries_key.as_deref().unwrap()).await.is_ok());
    // The legacy key is NOT written by a fresh workspace.
    assert!(store.head(&cfg.manifest_key()).await.is_err());

    let loaded = manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap();
    assert_eq!(loaded.manifest.seq, 1);
    assert!(loaded.manifest.entries.contains_key("a.txt"));
    assert!(!loaded.handle().legacy);
}

/// The point of the whole layout: a takeover rewrites a few hundred
/// bytes, not the project. If this ever regresses, a claim on a 1M-entry
/// workspace goes back to a multi-MB GET + PUT.
#[tokio::test]
async fn a_takeover_rotation_does_not_touch_the_entries_object() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let mut m = manifest::LeanManifest::default();
    m.seq = 4;
    for i in 0..50 {
        m.entries.insert(format!("f{i:03}.txt"), entry("x"));
    }
    manifest::cas_write(store.as_ref(), &cfg, &m, None, 1, "seed").await.unwrap();
    let gen4 = manifest::load_pointer(store.as_ref(), &cfg).await.unwrap().unwrap().pointer.entries_key.unwrap();
    let before = store.head(&gen4).await.unwrap();

    let (rotated, _) = manifest::rotate_for_takeover(store.as_ref(), &cfg, 2).await.unwrap().unwrap();
    assert_eq!(rotated.seq, 5, "rotation must bump the generation");

    let after = store.head(&gen4).await.unwrap();
    assert_eq!(before.etag, after.etag, "the entries object was rewritten by a rotation");
    let lp = manifest::load_pointer(store.as_ref(), &cfg).await.unwrap().unwrap();
    assert_eq!(lp.pointer.seq, 5);
    assert_eq!(lp.pointer.entries_seq, Some(4), "entries_seq must NOT move — a follower reads it to skip the GET");
    assert_eq!(lp.pointer.entries_key.as_deref(), Some(gen4.as_str()));
    // And the document still has every entry: rotation bumps, it does
    // not truncate.
    let loaded = manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap();
    assert_eq!(loaded.manifest.entries.len(), 50);
    assert_eq!(loaded.manifest.seq, 5, "the POINTER is the authority for seq, not the entries object");
}

/// A workspace written by an older binary migrates on its first write,
/// and the legacy key is left UNPARSEABLE rather than deleted.
#[tokio::test]
async fn migration_installs_the_pointer_and_poisons_the_legacy_key() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());

    // Hand-write the legacy layout, as an old flint-sync would.
    let mut old = manifest::LeanManifest::default();
    old.seq = 7;
    old.entries.insert("kept.txt".into(), entry("x"));
    let bytes = old.to_bytes();
    let crc = flint_store::crc64_nvme(&bytes);
    let stamps = flint_store::GenerationStamps {
        generation: 7,
        epoch: 1,
        flush_uuid: "legacy".into(),
        boundary_source: None,
        posix: None,
    };
    store
        .put_whole(&cfg.manifest_key(), bytes.into(), &PutCondition::IfNoneMatchAny, &stamps, crc)
        .await
        .unwrap();

    // A new binary reads it as legacy.
    let loaded = manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap();
    assert!(loaded.handle().legacy, "an un-migrated workspace must be recognised as legacy");
    assert_eq!(loaded.manifest.seq, 7);

    // Its first write migrates.
    let mut next = loaded.manifest.clone();
    next.seq += 1;
    next.entries.insert("added.txt".into(), entry("x"));
    manifest::cas_write(store.as_ref(), &cfg, &next, Some(&loaded.handle()), 2, "migrate")
        .await
        .unwrap();

    let after = manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap();
    assert!(!after.handle().legacy);
    assert_eq!(after.manifest.entries.len(), 2, "migration must not lose entries");

    // THE HAZARD. The legacy key must still EXIST — deleting it would
    // read as `Ok(None)` to an old binary, which means "first write",
    // which a barrier answers with IfNoneMatchAny: it would re-seed over
    // a live project. It exists and it cannot parse, so an old binary
    // gets LeanError::State and refuses.
    let (_, poisoned) = store.get_whole(&cfg.manifest_key(), None).await.unwrap();
    assert!(
        manifest::LeanManifest::parse(&poisoned).is_err(),
        "the legacy key still parses as a manifest — an old syncer would serve a stale project from it"
    );
    assert!(String::from_utf8_lossy(&poisoned).contains("upgrade flint-sync"));
}

/// A pointer whose generation object is missing is a BROKEN workspace,
/// never an empty one. Answering `None` here would be the same re-seed
/// hazard the migration is careful about, arriving by another road.
#[tokio::test]
async fn a_pointer_naming_a_missing_generation_refuses_rather_than_reading_empty() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let mut m = manifest::LeanManifest::default();
    m.seq = 2;
    m.entries.insert("a.txt".into(), entry("x"));
    manifest::cas_write(store.as_ref(), &cfg, &m, None, 1, "seed").await.unwrap();
    let gen2 = manifest::load_pointer(store.as_ref(), &cfg).await.unwrap().unwrap().pointer.entries_key.unwrap();
    store.delete(&gen2).await.unwrap();

    match manifest::load(store.as_ref(), &cfg).await {
        Err(LeanError::State(msg)) => {
            assert!(msg.contains("does not exist"), "unexpected message: {msg}");
        }
        Ok(None) => panic!("a broken pointer read as an EMPTY workspace — the next barrier would re-seed over it"),
        Ok(Some(_)) => panic!("a broken pointer read as a LOADED manifest"),
        Err(e) => panic!("expected a State refusal, got {e:?}"),
    }
}

/// Two writers that reach the same generation cannot both land: the
/// entries object is write-once, so the loser is told at the PUT rather
/// than discovering it after publishing.
#[tokio::test]
async fn a_second_writer_at_the_same_generation_is_refused() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let mut m = manifest::LeanManifest::default();
    m.seq = 1;
    manifest::cas_write(store.as_ref(), &cfg, &m, None, 1, "a").await.unwrap();

    let loaded = manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap();
    let mut mine = loaded.manifest.clone();
    mine.seq += 1;
    manifest::cas_write(store.as_ref(), &cfg, &mine, Some(&loaded.handle()), 1, "b").await.unwrap();

    // A writer that still holds the OLD handle and reaches the same seq.
    let mut theirs = loaded.manifest.clone();
    theirs.seq += 1;
    let err = manifest::cas_write(store.as_ref(), &cfg, &theirs, Some(&loaded.handle()), 1, "c")
        .await
        .unwrap_err();
    assert!(
        matches!(err, LeanError::Store(flint_store::StoreError::PreconditionFailed(_))),
        "expected a precondition failure, got {err:?}"
    );
}

/// Immutable metadata that is never collected is a leak that grows by a
/// whole manifest per publish. The reaper keeps a window behind the live
/// generation — not zero, because a reader resolves the pointer and the
/// object it names in two separate requests.
#[tokio::test]
async fn superseded_generations_are_reaped_but_the_live_one_and_a_window_survive() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let prefix = format!("{PREFIX}/.flint/lean/manifests/");

    let mut handle = None;
    for seq in 1..=12u64 {
        let mut m = manifest::LeanManifest::default();
        m.seq = seq;
        m.entries.insert("a.txt".into(), entry("a.txt"));
        let meta = manifest::cas_write(
            store.as_ref(),
            &cfg,
            &m,
            handle.as_ref(),
            1,
            &format!("flush-{seq}"),
        )
        .await
        .unwrap();
        handle = Some(manifest::ManifestHandle { etag: meta.etag, legacy: false });
    }
    // Twelve publishes, no sweep yet: twelve generations.
    assert_eq!(store.list(&prefix).await.unwrap().len(), 12);

    let removed = manifest::sweep_generations(store.as_ref(), &cfg).await.unwrap();
    let left = store.list(&prefix).await.unwrap();
    // The window is BEHIND the live one, so what survives is the live
    // generation PLUS KEEP_GENERATIONS.
    assert_eq!(left.len(), manifest::KEEP_GENERATIONS + 1);
    assert_eq!(removed, 12 - (manifest::KEEP_GENERATIONS + 1));

    // The live one is still there, and the workspace still reads.
    let lp = manifest::load_pointer(store.as_ref(), &cfg).await.unwrap().unwrap();
    assert!(
        left.iter().any(|o| Some(&o.key) == lp.pointer.entries_key.as_ref()),
        "the reaper deleted the generation the pointer names"
    );
    let loaded = manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap();
    assert_eq!(loaded.manifest.seq, 12);
    assert!(loaded.manifest.entries.contains_key("a.txt"));
}

/// A crash between the entries PUT and the pointer CAS leaves an object
/// no pointer ever named. It is unreachable by construction, so the
/// reaper must collect it with no special case — and must not mistake it
/// for the live one.
#[tokio::test]
async fn the_reaper_collects_an_orphan_from_a_crash_before_the_pointer_cas() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let mut m = manifest::LeanManifest::default();
    m.seq = 1;
    m.entries.insert("a.txt".into(), entry("a.txt"));
    manifest::cas_write(store.as_ref(), &cfg, &m, None, 1, "live").await.unwrap();
    let live = manifest::load_pointer(store.as_ref(), &cfg).await.unwrap().unwrap().pointer.entries_key.unwrap();

    // The orphan: a generation object at a HIGHER seq that no pointer
    // names, exactly as an interrupted publish leaves behind.
    let bytes = m.to_bytes();
    let crc = flint_store::crc64_nvme(&bytes);
    let stamps = flint_store::GenerationStamps {
        generation: 99,
        epoch: 1,
        flush_uuid: "crashed".into(),
        boundary_source: None,
        posix: None,
    };
    let orphan = cfg.generation_key(99, "crashed");
    store.put_whole(&orphan, bytes.into(), &PutCondition::IfNoneMatchAny, &stamps, crc).await.unwrap();

    // A FRESH object above the live generation is indistinguishable
    // from a publish in flight, and must survive: reaping it would
    // break a writer that has put its entries and not yet CAS'd.
    assert_eq!(
        manifest::sweep_generations(store.as_ref(), &cfg).await.unwrap(),
        0,
        "the sweep reaped a generation above the pointer that could still be an in-flight publish"
    );
    assert!(store.head(&orphan).await.is_ok());

    // Age it past the grace and it is wreckage, not a publish.
    // `backdate_epoch` moves any key's Last-Modified, not just an
    // epoch cell's — the store's clock is the only thing that can
    // distinguish wreckage from a publish in flight, so a test about
    // it has to move that clock rather than sleep an hour.
    store.backdate_epoch(&orphan, manifest::ORPHAN_GRACE_SECS + 60);
    assert_eq!(manifest::sweep_generations(store.as_ref(), &cfg).await.unwrap(), 1);
    let left: Vec<String> = store
        .list(&format!("{PREFIX}/.flint/lean/manifests/"))
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    assert!(!left.contains(&orphan), "the orphan survived the sweep");
    assert!(left.contains(&live), "the LIVE generation was reaped");
    assert!(manifest::load(store.as_ref(), &cfg).await.unwrap().is_some());
}

/// THE MEASUREMENT the pointer layout exists for. Under the single-object
/// layout a takeover was a GET and a PUT of the whole manifest — at 1M
/// entries, 264 MiB each way, per claim. Here it must touch no
/// generation object at all.
#[tokio::test]
async fn a_rotation_reads_and_writes_no_generation_object() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let mut m = manifest::LeanManifest::default();
    m.seq = 3;
    for i in 0..200 {
        m.entries.insert(format!("f{i:04}.txt"), entry("f"));
    }
    manifest::cas_write(store.as_ref(), &cfg, &m, None, 1, "seed").await.unwrap();
    let gen = manifest::load_pointer(store.as_ref(), &cfg).await.unwrap().unwrap().pointer.entries_key.unwrap();
    let before = store.head(&gen).await.unwrap();

    store.reset_op_counts();
    manifest::rotate_for_takeover(store.as_ref(), &cfg, 2).await.unwrap().unwrap();
    let ops = store.op_counts();

    // The whole claim is: read the pointer, write the pointer.
    assert!(
        store.total_ops() <= 3,
        "a rotation should be a couple of small requests, not {ops:?}"
    );
    // And the entries object is untouched, byte for byte.
    let after = store.head(&gen).await.unwrap();
    assert_eq!(before.etag, after.etag);
    assert_eq!(
        store.list(&format!("{PREFIX}/.flint/lean/manifests/")).await.unwrap().len(),
        1,
        "a rotation wrote a new generation object — it must reuse the standing one"
    );
}


/// A backend whose EPOCH RENEWAL can be switched to answer 401/403 while
/// every other call keeps working — the §6.3 shape exactly: the broker
/// or the token is gone, the bucket is fine, and the holder is alive.
struct AuthRefusing {
    inner: Arc<MemoryStore>,
    refuse: std::sync::atomic::AtomicBool,
}

impl AuthRefusing {
    fn new(inner: Arc<MemoryStore>) -> Self {
        Self { inner, refuse: std::sync::atomic::AtomicBool::new(false) }
    }
    fn set_refuse(&self, v: bool) {
        self.refuse.store(v, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ObjectStore for AuthRefusing {
    async fn put_whole(
        &self,
        key: &str,
        body: Bytes,
        cond: &PutCondition,
        stamps: &GenerationStamps,
        crc: u64,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.put_whole(key, body, cond, stamps, crc).await
    }
    async fn compose_generation(
        &self,
        spec: &flint_store::ComposeSpec<'_>,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.compose_generation(spec).await
    }
    async fn head(&self, key: &str) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.head(key).await
    }
    async fn get_whole(
        &self,
        key: &str,
        if_match: Option<&str>,
    ) -> flint_store::StoreResult<(flint_store::ObjectMeta, Bytes)> {
        self.inner.get_whole(key, if_match).await
    }
    async fn get_range(
        &self,
        key: &str,
        off: u64,
        len: u64,
        if_match: &str,
    ) -> flint_store::StoreResult<Bytes> {
        self.inner.get_range(key, off, len, if_match).await
    }
    fn min_part_size(&self) -> u64 {
        self.inner.min_part_size()
    }
    fn max_parts(&self) -> usize {
        self.inner.max_parts()
    }
    async fn list(&self, prefix: &str) -> flint_store::StoreResult<Vec<flint_store::ListedObject>> {
        self.inner.list(prefix).await
    }
    async fn delete(&self, key: &str) -> flint_store::StoreResult<()> {
        self.inner.delete(key).await
    }
    async fn head_version(
        &self,
        key: &str,
        v: &str,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.head_version(key, v).await
    }
    async fn get_version(
        &self,
        key: &str,
        v: &str,
    ) -> flint_store::StoreResult<(flint_store::ObjectMeta, Bytes)> {
        self.inner.get_version(key, v).await
    }
    async fn delete_version(&self, key: &str, v: &str) -> flint_store::StoreResult<()> {
        self.inner.delete_version(key, v).await
    }
    async fn list_versions(
        &self,
        prefix: &str,
    ) -> flint_store::StoreResult<Vec<flint_store::ListedVersion>> {
        self.inner.list_versions(prefix).await
    }
    async fn list_uploads(
        &self,
        prefix: &str,
    ) -> flint_store::StoreResult<Vec<flint_store::PendingUpload>> {
        self.inner.list_uploads(prefix).await
    }
    async fn abort_upload(&self, key: &str, id: &str) -> flint_store::StoreResult<()> {
        self.inner.abort_upload(key, id).await
    }
    async fn bootstrap(
        &self,
        prefix: &str,
    ) -> flint_store::StoreResult<flint_store::BootstrapReport> {
        self.inner.bootstrap(prefix).await
    }
    async fn epoch_read(
        &self,
        key: &str,
    ) -> flint_store::StoreResult<Option<flint_store::EpochState>> {
        self.inner.epoch_read(key).await
    }
    async fn epoch_acquire(
        &self,
        key: &str,
        holder: &str,
        observed: Option<&flint_store::EpochState>,
    ) -> flint_store::StoreResult<flint_store::EpochLease> {
        self.inner.epoch_acquire(key, holder, observed).await
    }
    async fn epoch_renew(
        &self,
        key: &str,
        lease: &flint_store::EpochLease,
        echo: Option<&str>,
    ) -> flint_store::StoreResult<flint_store::EpochLease> {
        if self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(flint_store::StoreError::Auth(
                "ExpiredToken: the security token included in the request is expired".into(),
            ));
        }
        self.inner.epoch_renew(key, lease, echo).await
    }
    async fn epoch_release(
        &self,
        key: &str,
        lease: &flint_store::EpochLease,
    ) -> flint_store::StoreResult<()> {
        self.inner.epoch_release(key, lease).await
    }
}

/// §6.3 — a credential refusal is not a fence and not contention, and
/// the holder must record it LOCALLY. The renewal that would carry the
/// fact into the lease echo is the very request being refused, so the
/// store can never learn it; without a local record an operator sees a
/// lease going stale next to a pod that is plainly Running, and nothing
/// that connects the two.
///
/// The four things that can each go wrong independently: the refusal
/// must not fence, must not drop the lease, must not slide its own
/// clock on a second failure, and must survive an ordinary gauge tick.
#[tokio::test]
async fn a_refused_credential_pauses_the_holder_without_fencing_it() {
    let inner = Arc::new(MemoryStore::new());
    let proxy = Arc::new(AuthRefusing::new(inner.clone()));
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let state = SidecarState::open(cfg.state_dir()).unwrap();
    let mut a = Sidecar {
        store: proxy.clone() as Arc<dyn ObjectStore>,
        cfg,
        state,
        lease: None,
        noted_not_regular: Default::default(),
    };
    assert!(claim_until_held(&mut a, 3).await);
    assert!(
        a.load_gauges().unwrap().auth_paused_since_unix.is_none(),
        "a healthy holder started out reading as credential-paused"
    );

    proxy.set_refuse(true);
    let e = lease::renew(&mut a).await.unwrap_err();
    assert!(e.is_auth(), "a 403 renewal did not classify as a credential fault: {e}");
    assert!(
        !matches!(e, LeanError::Fenced(_)),
        "a credential refusal self-fenced a live writer: {e}"
    );
    // A paused holder is still the holder: dropping the lease here
    // would turn the next renewal into a fresh claim.
    assert!(a.lease.is_some(), "the refusal dropped the lease");
    assert!(
        a.load_gauges().unwrap().auth_paused_since_unix.is_some(),
        "the refusal left no local evidence at all"
    );

    // First refusal wins. Planted well in the past on purpose — two
    // renewals inside one second would agree no matter what the code
    // did, and would test nothing.
    let gp = a.cfg.state_dir().join("gauges.json");
    let mut g: serde_json::Value = serde_json::from_slice(&std::fs::read(&gp).unwrap()).unwrap();
    g["auth_paused_since_unix"] = serde_json::json!(1_000_u64);
    std::fs::write(&gp, serde_json::to_vec(&g).unwrap()).unwrap();
    lease::renew(&mut a).await.unwrap_err();
    assert_eq!(
        a.load_gauges().unwrap().auth_paused_since_unix,
        Some(1_000),
        "a second refusal slid the pause clock forward, so the gauge no \
         longer answers the question it exists to answer"
    );

    // The gauge tick is store-free by construction, so it cannot
    // observe credentials. Recomputing this field instead of carrying
    // it would erase the pause on the very next tick — the gauge would
    // exist and always read None.
    a.write_gauges(false, None).unwrap();
    assert_eq!(
        a.load_gauges().unwrap().auth_paused_since_unix,
        Some(1_000),
        "an ordinary gauge tick erased the credential pause"
    );

    // The renewal is the only scheduled probe of our own credentials,
    // so it is also the only thing that can observe recovery.
    proxy.set_refuse(false);
    lease::renew(&mut a).await.expect("renew after the credentials came back");
    assert!(
        a.load_gauges().unwrap().auth_paused_since_unix.is_none(),
        "the pause outlived the credentials being restored"
    );
}

/// The predicate must read THROUGH the `#[from]` wrapper — that is the
/// entire reason it is a predicate and not a `LeanError` variant. A
/// variant would have to be constructed by hand at every `?` in the
/// crate, and would be missed at the one site that mattered.
#[test]
fn is_auth_reads_through_the_from_conversion() {
    fn via_question_mark() -> super::LeanResult<()> {
        Err(flint_store::StoreError::Auth("ExpiredToken".into()))?;
        Ok(())
    }
    assert!(via_question_mark().unwrap_err().is_auth());
    assert!(!LeanError::State("x".into()).is_auth());
    assert!(!LeanError::Fenced("x".into()).is_auth());
    let other: LeanError = flint_store::StoreError::Other("boom".into()).into();
    assert!(!other.is_auth(), "an ordinary store error read as a credential fault");
    let pf: LeanError = flint_store::StoreError::PreconditionFailed("412".into()).into();
    assert!(!pf.is_auth(), "a deposal read as a credential fault");
}

// ── content-defined chunking (chunked-manifest design §3) ────────────

/// Chunk a key list with a SMALL target, so a fixture stays readable
/// instead of needing 4096 entries to produce two chunks.
fn chunks_of(keys: &[String]) -> Vec<Vec<String>> {
    let refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let cuts = super::chunk::chunk_ranges_with(&refs, 8, 2, 32);
    let mut out = Vec::new();
    let mut start = 0;
    for c in cuts {
        out.push(keys[start..c].to_vec());
        start = c;
    }
    out
}

/// Fixed-count chunking, as the CONTROL. This is the shape §3
/// disqualifies, and the test below is only meaningful if this one
/// actually exhibits the failure — otherwise "content-defined survives
/// a front insert" would pass for a fixture too small to shift.
fn fixed_chunks_of(keys: &[String], n: usize) -> Vec<Vec<String>> {
    keys.chunks(n).map(|c| c.to_vec()).collect()
}

fn changed_chunks(a: &[Vec<String>], b: &[Vec<String>]) -> usize {
    let before: std::collections::HashSet<_> = a.iter().collect();
    b.iter().filter(|c| !before.contains(c)).count()
}

/// §3 — the whole reason boundaries are content-defined. Inserting a key
/// at the FRONT of the sorted order must rewrite about one chunk, not
/// all of them.
///
/// Under fixed-count splitting every later key shifts one slot, so every
/// boundary moves and every chunk is rewritten — O(entries) restored on
/// precisely the operation being optimised, and silently, because the
/// chunk sizes still look right. The control arm asserts that failure
/// really happens here, so the real arm cannot pass by being vacuous.
#[test]
fn a_front_insert_rewrites_one_chunk_not_the_project() {
    let base: Vec<String> = (0..300).map(|i| format!("src/f{i:04}.txt")).collect();
    let mut inserted = base.clone();
    inserted.insert(0, "src/a000-brand-new.txt".to_string());
    assert!(inserted.windows(2).all(|w| w[0] < w[1]), "fixture is not sorted");

    let c0 = chunks_of(&base);
    let c1 = chunks_of(&inserted);
    assert!(c0.len() > 4, "fixture produced {} chunks — too few to say anything", c0.len());
    let cd = changed_chunks(&c0, &c1);

    // The control: the same insert under fixed-count splitting.
    let f0 = fixed_chunks_of(&base, 8);
    let f1 = fixed_chunks_of(&inserted, 8);
    let fd = changed_chunks(&f0, &f1);
    assert!(
        fd >= f0.len(),
        "the control did not exhibit the cascade it exists to demonstrate ({fd} of {} chunks \
         changed), so the assertion below proves nothing about this fixture",
        f0.len()
    );

    assert!(
        cd <= 2,
        "a front insert rewrote {cd} of {} content-defined chunks (fixed-count rewrote {fd}) — \
         boundaries are moving with POSITION, which is the failure mode §3 disqualifies",
        c0.len()
    );
}

/// Boundaries follow the KEY SET, so a chunk that did not gain or lose a
/// key is byte-identical however far the change was from it — that is
/// what lets a publish reference the untouched chunks instead of
/// rewriting them, and it is where the asymptotic win actually lives.
#[test]
fn an_edit_far_from_a_chunk_leaves_it_alone() {
    let base: Vec<String> = (0..300).map(|i| format!("src/f{i:04}.txt")).collect();
    let mut edited = base.clone();
    edited.insert(150, "src/f0149-inserted.txt".to_string());
    edited.sort();

    let c0 = chunks_of(&base);
    let c1 = chunks_of(&edited);
    assert_eq!(c0.first(), c1.first(), "the FIRST chunk moved for a change in the middle");
    assert_eq!(c0.last(), c1.last(), "the LAST chunk moved for a change in the middle");
    assert!(
        changed_chunks(&c0, &c1) <= 2,
        "a middle insert rewrote {} chunks",
        changed_chunks(&c0, &c1)
    );
}

/// The floor and the ceiling both bind, and the tail is never dropped.
/// A chunker that lost the final partial run would lose every entry
/// after the last natural boundary — silently, since the chunks it did
/// emit would all be well-formed.
#[test]
fn chunk_sizing_respects_min_max_and_keeps_the_tail() {
    let keys: Vec<String> = (0..500).map(|i| format!("k{i:05}")).collect();
    let refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let cuts = super::chunk::chunk_ranges_with(&refs, 8, 3, 20);

    assert_eq!(*cuts.last().unwrap(), keys.len(), "the tail run was dropped");
    let mut prev = 0;
    for (i, c) in cuts.iter().enumerate() {
        let len = c - prev;
        assert!(len <= 20, "chunk {i} has {len} entries, over the max of 20");
        // Every run but the last must clear the floor; the tail is
        // whatever is left and has no lower bound by construction.
        if i + 1 < cuts.len() {
            assert!(len >= 3, "chunk {i} has {len} entries, under the min of 3");
        }
        prev = *c;
    }
    // Nothing is lost or duplicated.
    let total: usize = cuts.iter().scan(0, |p, c| { let l = c - *p; *p = *c; Some(l) }).sum();
    assert_eq!(total, keys.len(), "chunks do not partition the key stream");
}

/// The boundary rule is on-the-wire format: two binaries that disagree
/// about where a chunk ends produce different objects for identical
/// content and share nothing. Pin it against literal expected output,
/// not against a re-run of the same function, which would agree with
/// itself no matter what it computed.
#[test]
fn chunk_boundaries_are_stable_and_content_addresses_are_not_crc() {
    let keys: Vec<String> = (0..64).map(|i| format!("src/f{i:03}.txt")).collect();
    let refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let a = super::chunk::chunk_ranges_with(&refs, 8, 2, 32);
    let b = super::chunk::chunk_ranges_with(&refs, 8, 2, 32);
    assert_eq!(a, b, "chunking is not deterministic");
    assert!(!a.is_empty() && *a.last().unwrap() == 64);

    // A chunk's address must depend on its BYTES.
    let x = super::chunk::chunk_address(b"{\"a\":1}");
    let y = super::chunk::chunk_address(b"{\"a\":2}");
    assert_ne!(x, y, "two different chunk bodies share an address");
    assert_eq!(x.len(), 32, "address is not 128 bits of hex");
    assert_eq!(x, super::chunk::chunk_address(b"{\"a\":1}"), "address is not stable");
}

// ── the chunked wire format ─────────────────────────────────────────

fn entry_at(k: &str, seq: u64) -> super::manifest::LeanEntry {
    super::manifest::LeanEntry {
        key: format!("files/{k}"),
        etag: format!("e-{k}-{seq}"),
        crc64_b64: None,
        size: 10,
        mode: 0o644,
        mtime_unix: 1_700_000_000,
        generation: seq,
        epoch: 1,
        version_id: None,
    }
}

fn manifest_of(n: usize, seq: u64) -> super::manifest::LeanManifest {
    super::manifest::LeanManifest {
        seq,
        entries: (0..n).map(|i| (format!("src/f{i:05}.txt"), entry_at(&format!("f{i:05}"), seq))).collect(),
        pinned_reads: false,
        boundary_source: None,
    }
}

/// THE HEADLINE, measured rather than argued: a publish that changes
/// three files out of a large project must write bytes proportional to
/// those three files, not to the project.
///
/// Counted at the store, not inferred from the code. The control is the
/// FIRST publish, which necessarily writes every chunk — without it a
/// "few puts" assertion would pass just as well on a fixture that
/// happened to produce one chunk.
#[tokio::test]
async fn a_three_file_publish_writes_chunks_proportional_to_the_change() {
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = inner.clone();
    let dir = tempfile::tempdir().unwrap();
    // Small chunks so a readable fixture produces several of them. The
    // sizing is config precisely so this does not need a 20k-entry
    // manifest to exercise a multi-chunk publish.
    let mut cfg = cfg_for(dir.path());
    cfg.chunk_target = 64;
    cfg.chunk_min = 16;
    cfg.chunk_max = 256;

    let m0 = manifest_of(4000, 1);
    inner.reset_op_counts();
    let meta = manifest::cas_write_chunked(store.as_ref(), &cfg, &m0, None, &[],
        manifest::PublishStamps { epoch: 1, flush_uuid: "u1", boundary_source: None })
        .await
        .expect("first chunked publish");
    let first_puts = *inner.op_counts().get("put_whole").unwrap_or(&0);
    let p0 = manifest::load_pointer(store.as_ref(), &cfg).await.unwrap().unwrap().pointer;
    let chunks0 = match p0.entries().unwrap() {
        super::manifest::Entries::Chunked(c) => c.to_vec(),
        _ => panic!("the pointer is not chunked"),
    };
    assert!(chunks0.len() >= 4, "fixture made {} chunks — too few to measure", chunks0.len());
    assert!(
        first_puts as usize >= chunks0.len(),
        "the first publish wrote {first_puts} objects for {} chunks — it cannot have written \
         them all, so the comparison below is against nothing",
        chunks0.len()
    );

    // Change three files, spread across the key space.
    let mut m1 = m0.clone();
    m1.seq = 2;
    for i in [7usize, 1500, 3900] {
        m1.entries.insert(format!("src/f{i:05}.txt"), entry_at(&format!("f{i:05}"), 99));
    }
    let h = super::manifest::ManifestHandle { etag: meta.etag.clone(), legacy: false };
    inner.reset_op_counts();
    manifest::cas_write_chunked(store.as_ref(), &cfg, &m1, Some(&h), &chunks0,
        manifest::PublishStamps { epoch: 1, flush_uuid: "u2", boundary_source: None })
        .await
        .expect("incremental chunked publish");
    let puts = *inner.op_counts().get("put_whole").unwrap_or(&0);

    eprintln!(
        "chunked publish: {} chunks; full publish {first_puts} objects, 3-file publish {puts}",
        chunks0.len()
    );
    // The claim is a RATIO — proportional to the change, not to the
    // project — so assert it as one. An absolute bound would drift with
    // the fixture and stop meaning anything.
    assert!(
        puts * 5 < first_puts,
        "a three-file publish wrote {puts} objects against the full publish's {first_puts} \
         over {} chunks — untouched chunks are being rewritten, which is O(entries) again",
        chunks0.len()
    );
    // 3 changed chunks + the pointer, with slack for a boundary split.
    assert!(puts <= 6, "a three-file publish wrote {puts} objects");

    // And it is still the same manifest.
    let back = manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap();
    assert_eq!(back.manifest.entries.len(), 4000, "entries were lost across the chunked publish");
    assert_eq!(back.manifest.entries, m1.entries, "the assembled manifest is not what was published");
}

/// A chunk list with a hole must FAIL, never come back as a shorter
/// manifest. `manifest::load` maps a missing object to `Ok(None)` and
/// `None` means first write, so a silently-short manifest is how a
/// project gets re-seeded over — the same hazard the pointer layout
/// closed one level up, restated for chunks.
#[tokio::test]
async fn a_missing_chunk_refuses_rather_than_shortening_the_manifest() {
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = inner.clone();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = cfg_for(dir.path());
    cfg.chunk_target = 64;
    cfg.chunk_min = 16;
    cfg.chunk_max = 256;

    let m = manifest_of(2000, 1);
    manifest::cas_write_chunked(store.as_ref(), &cfg, &m, None, &[],
        manifest::PublishStamps { epoch: 1, flush_uuid: "u1", boundary_source: None }).await.unwrap();
    let p = manifest::load_pointer(store.as_ref(), &cfg).await.unwrap().unwrap().pointer;
    let chunks = match p.entries().unwrap() {
        super::manifest::Entries::Chunked(c) => c.to_vec(),
        _ => panic!("not chunked"),
    };
    assert!(chunks.len() >= 2);
    // Delete a chunk in the MIDDLE: the failure must not depend on it
    // being the first or last thing read.
    store.delete(&cfg.chunk_key(&chunks[chunks.len() / 2].addr)).await.unwrap();

    let msg = match manifest::load(store.as_ref(), &cfg).await {
        Err(e) => e.to_string(),
        Ok(m) => panic!(
            "a manifest with a missing chunk LOADED, with {} entries",
            m.map(|l| l.manifest.entries.len()).unwrap_or(0)
        ),
    };
    assert!(
        msg.contains("hole"),
        "a manifest with a missing chunk did not refuse; it said: {msg}"
    );
}

/// The pointer must never carry both layouts, and must never carry
/// neither. Both are malformed, and PICKING one would let two readers
/// that broke the tie differently disagree about the contents of the
/// same seq — the one thing a single visible object made impossible.
#[test]
fn a_pointer_naming_both_layouts_or_neither_is_refused() {
    let mut p = super::manifest::Pointer {
        seq: 3,
        entries_key: Some("k".into()),
        entries_seq: Some(3),
        chunks: Some(vec![]),
        pinned_reads: false,
        boundary_source: None,
        epoch: 1,
    };
    assert!(p.entries().unwrap_err().to_string().contains("BOTH"));
    p.entries_key = None;
    p.chunks = None;
    assert!(p.entries().unwrap_err().to_string().contains("no entries at all"));
    // An EMPTY project is an empty chunk list, and must resolve.
    p.chunks = Some(vec![]);
    assert!(matches!(p.entries().unwrap(), super::manifest::Entries::Chunked(c) if c.is_empty()));
}

/// Every partition invariant `assemble` checks, each violated on its
/// own. These are the silent ones: a wrong chunk list yields a
/// well-formed manifest that is quietly missing or duplicating entries,
/// and a missing entry reads to every consumer as a deleted file.
#[test]
fn assemble_refuses_every_way_a_chunk_list_can_lie() {
    let entries: std::collections::BTreeMap<String, super::manifest::LeanEntry> =
        (0..40).map(|i| (format!("k{i:03}"), entry_at(&format!("k{i:03}"), 1))).collect();
    let split = super::chunk::split_with(&entries, 4, 2, 8).unwrap();
    assert!(split.len() >= 3, "fixture made {} chunks", split.len());
    let refs: Vec<_> = split.iter().map(|(r, _)| r.clone()).collect();
    let bodies: Vec<Vec<u8>> = split.iter().map(|(_, b)| b.clone()).collect();

    // The control: unmolested, it assembles back to exactly the input.
    let ok = super::chunk::assemble(&refs, &bodies).unwrap();
    assert_eq!(ok, entries, "assemble does not round-trip its own split");

    let must_fail = |r: Vec<super::chunk::ChunkRef>, b: Vec<Vec<u8>>, why: &str| {
        assert!(
            super::chunk::assemble(&r, &b).is_err(),
            "assemble accepted a chunk list that {why}"
        );
    };
    // A count the body does not match.
    let mut r = refs.clone();
    r[1].n += 1;
    must_fail(r, bodies.clone(), "claims more entries than its body holds");
    // A body swapped for another chunk's (address no longer matches).
    let mut b = bodies.clone();
    b[1] = bodies[2].clone();
    must_fail(refs.clone(), b, "returns a body that is not the addressed one");
    // Out of order.
    let mut r = refs.clone();
    let mut b = bodies.clone();
    r.swap(0, 1);
    b.swap(0, 1);
    must_fail(r, b, "is not in increasing key order");
    // A dropped chunk, with its ref left behind.
    must_fail(refs.clone(), bodies[..bodies.len() - 1].to_vec(), "names more chunks than were fetched");
    // A first key that disagrees with the body.
    let mut r = refs.clone();
    r[1].first = "zzz".into();
    must_fail(r, bodies.clone(), "disagrees with its body about the first key");
}
