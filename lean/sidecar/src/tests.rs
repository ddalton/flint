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
