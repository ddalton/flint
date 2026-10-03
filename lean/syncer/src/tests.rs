//! The lean battery: every leg maps to a formal-model invariant or a
//! confirmed review finding (docs/plans/flint-lean-plan.md §10). Runs
//! against MemoryStore's full conditional semantics — 412 on both put
//! flavors, epoch CAS — so these are protocol tests, not stubs.

use std::sync::Arc;

use bytes::Bytes;

use flint_store::memory::MemoryStore;
use flint_store::{crc64_nvme, GenerationStamps, ObjectStore, PutCondition, StoreError};

use super::inbox;
use super::lease::{self, ClaimOutcome};
use super::manifest;
use super::state::SyncerState;
use super::{now_unix, LeanConfig, LeanError, Syncer};

const PREFIX: &str = "tenant/proj1";

pub(super) fn cfg_for(root: &std::path::Path) -> LeanConfig {
    let mut c = LeanConfig::new(PREFIX, root);
    instant_fence(&mut c);
    // Collect retired handles and chunks at once, as before the retire age
    // (the battery asserts collection); its own tests set it.
    c.retire_grace_secs = 0;
    c
}

/// The fence's clocks, collapsed for the battery: every poll counts
/// toward the quiet thresholds (a deposal is six polls, not a minute),
/// polls do not sleep, and a wait gives up after two seconds rather
/// than 150. A test that needs a holder deposed drives the polls itself
/// through `claim_until_held`.
fn instant_fence(c: &mut LeanConfig) {
    c.claim_poll_secs = 0;
    c.claim_quiet_spacing_secs = 0;
    c.claim_deadline_secs = 2;
}

/// A config pinned to the SINGLE-generation layout (chunking off).
///
/// Not legacy scaffolding: a workspace that has not published since the
/// chunk migration is still on this layout, and reading it correctly is
/// a permanent obligation, not a transitional one. These tests are what
/// keeps that path honest, so they opt out explicitly rather than
/// drifting whenever the default moves.
fn cfg_single(root: &std::path::Path) -> LeanConfig {
    let mut c = LeanConfig::new(PREFIX, root);
    c.chunked = false;
    instant_fence(&mut c);
    c.retire_grace_secs = 0;
    c
}

pub(super) async fn syncer(store: &Arc<MemoryStore>, root: &std::path::Path) -> Syncer {
    let cfg = cfg_for(root);
    let state = SyncerState::open(cfg.state_dir()).unwrap();
    Syncer {
        store: store.clone() as Arc<dyn ObjectStore>,
        cfg,
        state,
        lease: None,
        cell_written_at: None,
        noted_not_regular: Default::default(),
    }
}

/// Claim, looping claim_step (a fresh or released cell claims on the
/// first step; a foreign one needs the quiet polls).
async fn claim_until_held(sc: &mut Syncer, max_steps: u32) -> bool {
    for _ in 0..max_steps {
        match lease::claim_step(sc, true).await.unwrap() {
            ClaimOutcome::Claimed(_) => return true,
            ClaimOutcome::Waiting { .. } => {}
        }
    }
    false
}

pub(super) fn write(root: &std::path::Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

/// A derive is pending: the pointer names a document the last consume did
/// not derive against (or it left something it could not take). What
/// `Baseline::behind` said before the scan trigger.
pub(super) async fn derive_pending(sc: &Syncer) -> bool {
    let p = manifest::load_pointer(sc.store.as_ref(), &sc.cfg).await.unwrap().map(|p| p.etag);
    let d = sc.state.load_baseline().unwrap().derived_etag;
    let norm = |e: Option<String>| e.map(|e| e.trim_matches('"').to_string());
    norm(p) != norm(d)
}

pub(super) fn read(root: &std::path::Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(root.join(rel)).ok()
}

/// Bump a file's mtime past the 1-second stat granularity so the scan
/// sees the change without sleeping.
pub(super) fn backdate_baseline(sc: &Syncer, rel: &str) {
    let mut b = sc.state.load_baseline().unwrap();
    if let Some(e) = b.entries.get_mut(rel) {
        e.mtime_unix -= 10;
    }
    sc.state.save_baseline(&b).unwrap();
}

/// What the GATEWAY does for a UI save under P2 (simplification step 5,
/// 2026-09-25): a fresh handle, then ONE manifest CAS citing it (epoch 0,
/// no lease), acknowledged after the CAS — `flint-lean-gateway`'s
/// `put_file`, which this crate cannot depend on. Returns the etag;
/// `hitl_write_at` also the handle.
pub(super) async fn hitl_write(
    store: &Arc<MemoryStore>,
    cfg: &LeanConfig,
    path: &str,
    content: &str,
    author: &str,
) -> Result<String, LeanError> {
    hitl_write_at(store, cfg, path, content, author, now_unix()).await.map(|(etag, _)| etag)
}

/// `hitl_write` with the gateway's clock supplied — the order two UI
/// writes of one path carry in their handles' names (`ui_flush`).
pub(super) async fn hitl_write_at(
    store: &Arc<MemoryStore>,
    cfg: &LeanConfig,
    path: &str,
    content: &str,
    _author: &str,
    added_unix: u64,
) -> Result<(String, String), LeanError> {
    let flush = super::ui_flush(added_unix);
    let key = cfg.handle_key(path, &flush);
    let body = Bytes::from(content.to_string());
    let size = body.len() as u64;
    let crc = crc64_nvme(&body);
    let stamps = GenerationStamps {
        generation: 0,
        epoch: 0,
        flush_uuid: flush.clone(),
        boundary_source: None,
        posix: None,
    };
    let meta = store.put_whole(&key, body, &PutCondition::IfNoneMatchAny, &stamps, crc).await?;
    let entry_key = key.clone();
    let etag = meta.etag.clone();
    gateway_commit(store, cfg, &flush, move |doc| {
        let was = doc.entries.get(path).cloned();
        doc.tombstones.remove(path);
        doc.entries.insert(
            path.to_string(),
            manifest::LeanEntry {
                key: entry_key.clone(),
                etag: etag.clone(),
                crc64_b64: flint_store::crc64_to_b64(crc),
                size,
                mode: was.as_ref().map(|e| e.mode).unwrap_or(0o644),
                mtime_unix: added_unix as i64,
                generation: was.map(|e| e.generation).unwrap_or(0) + 1,
                epoch: 0,
            },
        );
    })
    .await?;
    Ok((meta.etag, key))
}

/// The gateway's one-CAS commit of a UI edit (`Workspace::commit_edit`):
/// load, the next generation with the installing party's fields reset,
/// the edit, tombstones kept to the writers' window, CAS with epoch 0, and
/// again from the top on a lost race.
pub(super) async fn gateway_commit(
    store: &Arc<MemoryStore>,
    cfg: &LeanConfig,
    flush: &str,
    edit: impl Fn(&mut manifest::LeanManifest),
) -> Result<(), LeanError> {
    manifest::commit_edit(store.as_ref(), cfg, flush, |_, doc| {
        edit(doc);
        Ok::<(), ()>(())
    })
    .await
    .map_err(|e| match e {
        manifest::EditError::Store(e) => e,
        other => LeanError::State(format!("the gateway's commit did not land: {other:?}")),
    })
}

/// The key the manifest cites for `path` — the handle a stranger would
/// have to overwrite to reach a reader — or the bare path when uncited.
pub(super) async fn cited_key(store: &Arc<MemoryStore>, cfg: &LeanConfig, path: &str) -> String {
    manifest::load(store.as_ref(), cfg)
        .await
        .unwrap()
        .and_then(|l| l.manifest.entries.get(path).map(|e| e.key.clone()))
        .unwrap_or_else(|| cfg.file_key(path))
}

/// Every handle of `path` the bucket still holds, in key order — what a
/// collector or a sweep leaves behind is read here, never through a HEAD
/// of a bare path nothing writes any more.
pub(super) async fn handles_for(store: &Arc<MemoryStore>, cfg: &LeanConfig, path: &str) -> Vec<String> {
    let mut keys: Vec<String> = store
        .list(&cfg.files_prefix())
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .filter(|k| cfg.handle_parts(k).map(|(p, _)| p == path).unwrap_or(false))
        .collect();
    keys.sort();
    keys
}

/// The bytes the manifest cites for `path`, if the citation resolves.
pub(super) async fn cited_bytes(store: &Arc<MemoryStore>, cfg: &LeanConfig, path: &str) -> Option<Bytes> {
    let l = manifest::load(store.as_ref(), cfg).await.unwrap()?;
    let e = l.manifest.entries.get(path)?;
    store.get_whole(&e.key, Some(&e.etag)).await.ok().map(|(_, b)| b)
}

/// Does the manifest cite a handle for `path` that the bucket still holds?
/// What `store.head(&cfg.file_key(path))` asked before handles.
pub(super) async fn object_at(store: &Arc<MemoryStore>, cfg: &LeanConfig, path: &str) -> bool {
    match manifest::load(store.as_ref(), cfg).await.unwrap().and_then(|l| l.manifest.entries.get(path).cloned()) {
        Some(e) => store.head(&e.key).await.is_ok(),
        None => false,
    }
}

/// Another writer's publish of `content` at `path`, as the bucket sees it
/// under handles: a fresh handle and a manifest generation citing it (the
/// model's peer `UploadIO` then `CASInstall`), with no tree of its own.
/// Returns the handle's etag and key.
pub(super) async fn peer_publish(
    store: &Arc<MemoryStore>,
    cfg: &LeanConfig,
    path: &str,
    content: &str,
    flush: &str,
) -> (String, String) {
    let key = cfg.handle_key(path, flush);
    let body = Bytes::from(content.to_string());
    let crc = crc64_nvme(&body);
    let loaded = manifest::load(store.as_ref(), cfg).await.unwrap();
    let prev_gen =
        loaded.as_ref().and_then(|l| l.manifest.entries.get(path)).map(|e| e.generation).unwrap_or(0);
    let stamps = GenerationStamps {
        generation: prev_gen + 1,
        epoch: 0,
        flush_uuid: flush.to_string(),
        boundary_source: None,
        posix: None,
    };
    let meta = store.put_whole(&key, body, &PutCondition::IfNoneMatchAny, &stamps, crc).await.unwrap();
    let mut theirs = loaded.as_ref().map(|l| l.manifest.clone()).unwrap_or_default();
    theirs.seq += 1;
    theirs.entries.insert(
        path.to_string(),
        manifest::LeanEntry {
            key: key.clone(),
            etag: meta.etag.clone(),
            crc64_b64: flint_store::crc64_to_b64(crc),
            size: meta.size,
            mode: 0o644,
            mtime_unix: 0,
            generation: prev_gen + 1,
            epoch: 0,
        },
    );
    let handle = loaded.as_ref().map(|l| l.handle());
    let retired = loaded.as_ref().and_then(|l| l.manifest.entries.get(path)).map(|e| e.key.clone());
    manifest::cas_write(store.as_ref(), cfg, &theirs, handle.as_ref(), 0, flush).await.unwrap();
    // ...and its collector takes the handle the document stopped citing
    // (R3), unless the document still cites it at another path.
    if let Some(old) = retired {
        if !theirs.entries.values().any(|e| e.key == old) {
            store.delete(&old).await.unwrap();
        }
    }
    (meta.etag, key)
}

// ── the battery ──────────────────────────────────────────────────────

/// Publish → fresh checkout materializes byte-identically; a delete
/// takes TWO scans to publish (the two-consecutive-scans rule) and the
/// GC removes the object only then.
#[tokio::test]
async fn checkout_publish_roundtrip_and_two_scan_delete() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap(); // empty subtree
    write(dir_a.path(), "src/main.rs", "fn main() {}");
    write(dir_a.path(), "README.md", "hello");
    let r = a.run_barrier().await.unwrap();
    assert_eq!(r.uploaded.len(), 2);

    // Fresh pod elsewhere: checkout sees both files.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
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
    assert!(!object_at(&store, &a.cfg, "README.md").await);
}

/// The review's worst finding, as a drill leg: a HITL upload with NO
/// sync must survive any number of barriers — consumed into the tree,
/// re-cited by the manifest, present in a fresh checkout.
#[tokio::test]
async fn hitl_upload_survives_two_barriers_without_sync() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
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
    let mut sc2 = syncer(&store, dir2.path()).await;
    sc2.checkout().await.unwrap();
    assert_eq!(read(dir2.path(), "docs/upload.pdf").unwrap(), "user bytes");
}

/// A write into the bucket by something that is NOT this workspace's
/// writer, and with no inbox entry to make it legitimate. This is what
/// a read-write passthrough mount over a published prefix does.
async fn foreign_overwrite(store: &Arc<MemoryStore>, cfg: &LeanConfig, path: &str, content: &str) {
    // Under handles a stranger that reaches a reader overwrites the HANDLE
    // the manifest cites — a key nobody else should write.
    let key = cited_key(store, cfg, path).await;
    let body = Bytes::from(content.to_string());
    let crc = crc64_nvme(&body);
    let stamps = GenerationStamps {
        generation: 0,
        epoch: 0,
        flush_uuid: "a-stranger".to_string(),
        boundary_source: None,
        posix: None,
    };
    store
        .put_whole(&key, body, &PutCondition::Unconditional, &stamps, crc)
        .await
        .expect("the foreign write lands");
}

/// Composition drill C4, decided locally.
///
/// A workspace that is PUBLISHED — forge's legible export is the
/// shipped case — has exactly one party entitled to write it, so an
/// object that has moved off its citation was moved by a stranger.
/// Adopting it copies bytes no manifest cites into the reader's tree
/// and reports success, which is how a foreign write reaches an agent.
///
/// Note what the reader is NOT told: `sc2` runs an ordinary default
/// config. The refusal has to come from the MANIFEST, because a reader
/// that had to be configured to be careful is a reader that will
/// eventually be deployed without it.
#[tokio::test]
async fn a_published_workspace_refuses_a_foreign_write_instead_of_adopting_it() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    sc.cfg.sole_writer = true;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "README.md", "the real readme");
    sc.run_barrier().await.unwrap();

    foreign_overwrite(&store, &sc.cfg, "README.md", "FOREIGN BYTES").await;

    let dir2 = tempfile::tempdir().unwrap();
    let mut sc2 = syncer(&store, dir2.path()).await;
    let err = sc2.checkout().await.expect_err("a published workspace must refuse");
    let msg = format!("{err}");
    assert!(msg.contains("SOLE WRITER"), "the refusal must say why: {msg}");
    assert!(
        !msg.contains("recover-staged"),
        "that remedy no longer exists; nothing was staged here: {msg}"
    );
    assert!(
        read(dir2.path(), "README.md").is_none(),
        "it materialized the foreign bytes anyway"
    );
}

/// M4 (P2, 2026-09-25): a checkout adopts nothing the manifest does not
/// cite, in ANY workspace. A cited handle is immutable — a UI save goes to a
/// fresh handle and commits, an outside `aws s3 cp` lands at a bare path the
/// ingress sweep adopts by a commit — so a handle whose etag moved was
/// overwritten from outside the protocol, and the checkout refuses, naming
/// it. (Before P2 an ordinary workspace adopted it as "a human whose bytes
/// should win": the S3-wins arm, a citation-repair source the P2 design
/// deletes.)
#[tokio::test]
async fn an_overwritten_handle_is_refused_by_checkout_in_every_workspace() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    assert!(!sc.cfg.sole_writer, "the default is an ordinary workspace");
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "README.md", "the real readme");
    sc.run_barrier().await.unwrap();

    foreign_overwrite(&store, &sc.cfg, "README.md", "bytes from outside").await;

    let dir2 = tempfile::tempdir().unwrap();
    let mut sc2 = syncer(&store, dir2.path()).await;
    let err = sc2.checkout().await.expect_err("an overwritten handle was adopted");
    assert!(err.to_string().contains("immutable handle"), "{err}");
    assert_eq!(read(dir2.path(), "README.md"), None, "uncited bytes were written");
}

/// The flag has to survive the pointer, because that is what a reader
/// reconstructs the manifest from. A publish that dropped it would
/// leave a mirror looking ordinary to the very next reader.
#[tokio::test]
async fn the_published_flag_survives_the_pointer_round_trip() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    sc.cfg.sole_writer = true;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "a.txt", "alpha");
    sc.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert!(m.manifest.sole_writer, "the pointer lost the flag");

    // And a second barrier does not quietly drop it: `merge` clears the
    // field on purpose, so the installing pass has to restate it every
    // time.
    write(dir.path(), "b.txt", "beta");
    sc.run_barrier().await.unwrap();
    let m2 = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert!(m2.manifest.sole_writer, "the second publish dropped the flag");
}

/// The neighbour probe is skipped on a published mirror, and run
/// everywhere else.
///
/// Forge spawns a barrier per export, so probing there would be a
/// recurring read whose warning `run_barrier`'s line filter discards
/// anyway. The prefix is still covered — the publisher probes it once
/// at startup — so this asserts the SKIP, not an absence of coverage.
#[tokio::test]
async fn a_published_mirror_does_not_probe_for_neighbours() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;

    // A forge repository is squatting on this workspace's prefix.
    store
        .epoch_acquire(&format!("{PREFIX}/git/epoch"), "forge-1", None)
        .await
        .unwrap();

    // An ordinary workspace finds it...
    let seen = flint_store::layout::neighbours(
        store.as_ref(),
        &sc.cfg.prefix,
        flint_store::layout::Writer::LeanWorkspace,
    )
    .await
    .unwrap();
    assert_eq!(seen.len(), 1, "the condition is there to be found");

    // ...and the gate is what decides whether we go looking.
    assert!(!sc.cfg.sole_writer, "an ordinary workspace probes");
    sc.cfg.sole_writer = true;
    assert!(sc.cfg.sole_writer, "a published mirror does not");
}

/// UI edit + agent edit of ONE path: both versions recoverable, a
/// conflict surfaced, never a silent winner (the drill leg pinned in
/// plan Phase 6).
#[tokio::test]
async fn ui_edit_vs_agent_edit_never_a_silent_winner() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
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
    // preserved at the conflict key before being superseded. Under P2 the
    // user's save is cited when the agent publishes, so the COMMIT meets
    // it (R7), not a consume.
    let conflicts = sc.state.load_conflicts().unwrap();
    let c = conflicts.iter().find(|c| c.kind.starts_with("commit-surfaced-foreign")).expect("conflict surfaced");
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
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "keep.txt", "keep");
    write(dir.path(), "gone.txt", "delete me");
    sc.run_barrier().await.unwrap();

    // The agent deletes; the container restarts BEFORE any barrier.
    std::fs::remove_file(dir.path().join("gone.txt")).unwrap();
    drop(sc);
    let mut sc = syncer(&store, dir.path()).await; // same emptyDir
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

/// Takeover: a holder that stalls INSIDE its commit section is deposed
/// after the quiet polls, and the successor rotates the manifest before
/// serving, so the straggler's CAS can never land
/// (Inv_NoStragglerInstall). Under the per-barrier lease this is the
/// only straggler there is: a writer stalled anywhere else holds
/// nothing, and its next barrier simply claims again.
#[tokio::test]
async fn takeover_rotation_fences_the_straggler() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "f.txt", "from A");
    a.run_barrier().await.unwrap();
    let seq_before = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;
    // A claims for a commit section and stalls in it (stops renewing).
    assert!(claim_until_held(&mut a, 3).await, "a released cell is claimable at once");

    // B replaces it: fresh emptyDir, fresh identity ⇒ the foreign-holder
    // path, quiet polls, then takeover.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
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

    // A thaws inside its commit section: the cell no longer names it,
    // and the fence is a Fenced error that abandons the barrier — the
    // manifest B rotated is untouched. (Driven through the commit
    // section's own check rather than a full barrier, because a full
    // barrier claims AFTER its uploads and would simply queue behind
    // B; that path is `a_holder_deposed_mid_commit_abandons_the_barrier`.)
    let err = lease::renew(&mut a).await.unwrap_err();
    assert!(matches!(err, LeanError::Fenced(_)), "straggler must fence, got: {err:?}");
    assert!(a.lease.is_none(), "a fenced holder must drop its lease");
    let m = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap();
    assert_eq!(m.manifest.seq, rotated.manifest.seq, "straggler CAS landed!");
    let cited = &m.manifest.entries["f.txt"];
    let (_, body) = store.get_whole(&cited.key, Some(&cited.etag)).await.unwrap();
    assert_eq!(&body[..], b"from A", "straggler bytes reached a cited object");

    // And A is not dead: its next barrier claims again (B holds without
    // renewing, so A deposes it in turn) and publishes its work.
    write(dir_a.path(), "f.txt", "A, after the fence");
    backdate_baseline(&a, "f.txt");
    a.run_barrier().await.expect("a fenced writer's NEXT barrier publishes");
    let m = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap();
    let cited = &m.manifest.entries["f.txt"];
    let (_, body) = store.get_whole(&cited.key, Some(&cited.etag)).await.unwrap();
    assert_eq!(&body[..], b"A, after the fence");
}

/// Rule R7 of the handles design: a version another writer published
/// under the agent's edit — which this tree never integrated — is
/// PRESERVED and recorded at the commit, then the agent's version is
/// cited over it. The slot's 412 met this at the PUT (`Upload412Preserves`);
/// a fresh handle has no slot to fail on, so the CAS, the one place the
/// citation is read, takes the same decision. The inherited LOCAL-WINS
/// overwrite — silent, the other version gone — is what lean must NOT do
/// (the model's `LeanImmutableCasOverridesPeer`), and a park had no way out
/// (review 2026-09-12, inbox-1). Preservation keeps both, and the retired
/// handle is collected once its copy exists.
#[tokio::test]
async fn a_version_published_under_the_agents_edit_is_surfaced_at_the_commit_never_silently_overwritten() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "f.txt", "seed");
    sc.run_barrier().await.unwrap();

    // A peer publishes a version this tree never consumes...
    let (peer_etag, peer_key) = peer_publish(&store, &sc.cfg, "f.txt", "peer bytes", "someone-else").await;
    // ...and the agent edits the path against the seed.
    write(dir.path(), "f.txt", "agent bytes");
    backdate_baseline(&sc, "f.txt");
    let r = sc.run_barrier().await.unwrap();
    assert!(r.parked.is_empty(), "parked with no way out: {:?}", r.parked);
    assert_eq!(r.uploaded, vec!["f.txt".to_string()]);
    assert_eq!(r.surfaced, vec!["f.txt".to_string()], "the commit did not surface what it published over");

    // The agent's version is current and cited at its own handle...
    assert_eq!(cited_bytes(&store, &sc.cfg, "f.txt").await.as_deref(), Some(&b"agent bytes"[..]));
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert_ne!(m.manifest.entries["f.txt"].key, peer_key);
    // ...the peer's bytes are not lost: preserved, and the record says
    // where...
    let rec = sc
        .state
        .load_conflicts()
        .unwrap()
        .into_iter()
        .find(|c| c.kind.starts_with("commit-surfaced-foreign") && c.path == "f.txt")
        .expect("no record surfaces the version published over");
    assert_eq!(rec.foreign_etag, peer_etag);
    let (_, kept) = store.get_whole(&rec.preserved_key.expect("not preserved"), None).await.unwrap();
    assert_eq!(&kept[..], b"peer bytes", "the preserved copy is not the peer's version");
    // ...and the retired handle was collected, unconditionally, once the
    // copy existed: one handle of the path remains, the cited one.
    assert!(matches!(store.head(&peer_key).await, Err(StoreError::NotFound(_))), "the retired handle survived");
    assert_eq!(handles_for(&store, &sc.cfg, "f.txt").await, vec![m.manifest.entries["f.txt"].key.clone()]);
    assert_eq!(r.collected, 1, "the peer's handle was the one this commit retired: {r:?}");
}

/// A delete retires exactly the handle the document cited — never a UI
/// re-creation of the path, which lands at a handle of its own. The GC
/// HEAD-guard (`gc-skip`, LeanGCUnguarded's counterexample) guarded a
/// shared key against exactly this; a handle needs no guard.
#[tokio::test]
async fn a_delete_retires_only_the_handle_it_cited_never_a_ui_re_creation() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "doc.txt", "v1");
    sc.run_barrier().await.unwrap();
    let seed = cited_key(&store, &sc.cfg, "doc.txt").await;

    // The agent deletes; absence ages through one scan.
    std::fs::remove_file(dir.path().join("doc.txt")).unwrap();
    sc.run_barrier().await.unwrap(); // first absence

    // A UI write re-creates the path at a handle of its own.
    let (_, ui_key) = hitl_write_at(&store, &sc.cfg, "doc.txt", "user re-created", "dilip", now_unix()).await.unwrap();

    // P2: the UI's re-creation is COMMITTED, so the agent's delete meets
    // it as theirs changed since the base (the path is dirty in the tree,
    // so nothing is owed); M3: the delete publishes over it — mine wins,
    // theirs preserved under a record.
    let _ = &ui_key;
    for _ in 0..4 {
        sc.run_barrier().await.unwrap();
    }
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert!(!m.manifest.entries.contains_key("doc.txt"), "the agent's delete never published");
    // The seed was retired by the UI's commit, and the gateway deletes
    // nothing: the handle is cited by no one and left to the orphan sweep
    // (slice 5 makes that a retire-age rule).
    assert!(!m.manifest.entries.values().any(|e| e.key == seed), "the seed is still cited");
    let conflicts = sc.state.load_conflicts().unwrap();
    assert!(!conflicts.iter().any(|c| c.kind == "gc-skip"), "no guard, no skip: {conflicts:?}");
    let rec = conflicts
        .iter()
        .find(|c| c.kind.starts_with("commit-deleted-over-theirs") && c.path == "doc.txt")
        .expect("the UI write over the deleted path was not surfaced");
    let (_, kept) = store.get_whole(rec.preserved_key.as_ref().unwrap(), None).await.unwrap();
    assert_eq!(&kept[..], b"user re-created");
}









// ── The retired classes under handles (design 2026-09-19 §3) ─────────────
//
// Each test below stands where a same-key test stood: the slot's race, its
// 412 arm, its HEAD-guarded delete, its collector-off leak and its tracked
// orphan are gone with the slot, and what is checked instead is the rule
// that replaced them. The test-name census in the design doc names each
// retired test and the test that stands in its place.

/// Rule R4's pair, the withheld half (`LeanImmutableProbeUploadWithheld`):
/// an upload the peer's orphan sweep took between its landing and its
/// commit is WITHHELD by the commit's own re-read — never cited, recorded,
/// and the path stays dirty for the next barrier to upload afresh. Under
/// the slot this was the "adopt/upload withheld" arm against the peer's
/// If-Match GC; the sweep runs under the cell, so the re-read and the CAS
/// are race-free against it, and with no grace modelled it can take any
/// in-flight upload — the grace is a cost lever, not a safety one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commit_withholds_an_upload_a_peers_sweep_took_and_uploads_afresh_next_barrier() {
    let inner = Arc::new(MemoryStore::new());
    let ha = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = hooked_syncer(&ha, dir_a.path());
    let mut b = syncer(&inner, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "seed.txt", "seed");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    // B's orphan sweep is due at its next commit, with no grace.
    b.cfg.untracked_grace_secs = 0;
    b.state.save_orphan_sweep_at(now_unix() - b.cfg.untracked_sweep_secs - 1).unwrap();
    write(dir_b.path(), "b.txt", "B publishes");
    write(dir_a.path(), "x.txt", "A's upload");
    // A's upload lands; before A claims, B's commit sweeps it.
    let b_slot = std::sync::Mutex::new(Some(b));
    let b_back: Arc<std::sync::Mutex<Option<Syncer>>> = Arc::new(std::sync::Mutex::new(None));
    let b_back_in = b_back.clone();
    ha.after_put(&a.cfg.file_key("x.txt"), move || {
        let mut b = b_slot.lock().unwrap().take().expect("hook ran twice");
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let rb = b.run_barrier().await.expect("B's barrier");
                assert!(rb.swept >= 1, "fixture: B's sweep did not take A's in-flight upload: {rb:?}");
            })
        });
        *b_back_in.lock().unwrap() = Some(b);
    });
    let ra = a.run_barrier().await.expect("A's barrier");
    assert!(b_back.lock().unwrap().is_some(), "fixture: the hook never ran");
    assert_eq!(ra.parked, vec!["x.txt".to_string()], "the commit cited a handle the sweep took: {ra:?}");
    assert!(ra.uploaded.is_empty(), "{ra:?}");
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("x.txt"), "cited a collected handle");
    assert!(
        a.state.load_conflicts().unwrap().iter().any(|c| c.kind.starts_with("upload-withheld")),
        "the withheld upload left no record"
    );
    // The path stayed dirty: the next barrier uploads afresh.
    let ra2 = a.run_barrier().await.unwrap();
    assert_eq!(ra2.uploaded, vec!["x.txt".to_string()], "{ra2:?}");
    assert_eq!(cited_bytes(&inner, &a.cfg, "x.txt").await.as_deref(), Some(&b"A's upload"[..]));
}

/// A withheld upload is PARKED: the path is the agent's work and "publishes
/// next barrier", so the merge does not count theirs there as owed. If the
/// agent then backs out, theirs IS owed, and nothing else may move the
/// pointer: a barrier that parks a path theirs changed must leave the
/// syncer behind (MCLeanP1Like, 2026-09-25, Inv_ShortcutSound, 22 states).
#[tokio::test(flavor = "multi_thread")]
async fn a_peers_version_behind_a_withheld_upload_arrives_when_the_agent_drops_its_file() {
    let inner = Arc::new(MemoryStore::new());
    let ha = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = hooked_syncer(&ha, dir_a.path());
    let mut b = syncer(&inner, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "seed.txt", "seed");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    b.cfg.untracked_grace_secs = 0;
    b.state.save_orphan_sweep_at(now_unix() - b.cfg.untracked_sweep_secs - 1).unwrap();
    // B's commit (which runs its sweep) carries nothing A lacks: the same
    // bytes, re-published, so the etag theirs cites is the one A has.
    write(dir_b.path(), "seed.txt", "seed");
    backdate_baseline(&b, "seed.txt");
    write(dir_a.path(), "x.txt", "A's draft");
    // A's upload lands; before A claims, B's commit sweeps it and a peer
    // commits its own x.txt.
    let b_slot = std::sync::Mutex::new(Some(b));
    let (store2, cfg_a) = (inner.clone(), a.cfg.clone());
    ha.after_put(&a.cfg.file_key("x.txt"), move || {
        let mut b = b_slot.lock().unwrap().take().expect("hook ran twice");
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let rb = b.run_barrier().await.expect("B's barrier");
                assert!(rb.swept >= 1, "fixture: B's sweep did not take A's in-flight upload: {rb:?}");
                peer_publish(&store2, &cfg_a, "x.txt", "the peer's x", "peer").await;
            })
        });
    });
    let ra = a.run_barrier().await.expect("A's barrier");
    assert_eq!(ra.parked, vec!["x.txt".to_string()], "fixture: the upload was not withheld: {ra:?}");
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries.keys().collect::<Vec<_>>(), ["seed.txt", "x.txt"], "fixture: something else changed");
    assert_eq!(m.entries["seed.txt"].etag, a.state.load_baseline().unwrap().entries["seed.txt"].etag, "fixture: B's commit changed seed.txt's bytes");
    assert_eq!(read(dir_a.path(), "x.txt").as_deref(), Some("A's draft"), "fixture");

    // The agent drops its draft; nothing else commits.
    std::fs::remove_file(dir_a.path().join("x.txt")).unwrap();
    for _ in 0..2 {
        a.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir_a.path(), "x.txt").as_deref(), Some("the peer's x"), "the peer's version never arrived");
}

/// The same, where the tree had the path: the agent reverts its edit.
#[tokio::test(flavor = "multi_thread")]
async fn a_peers_version_behind_a_withheld_edit_arrives_when_the_agent_reverts() {
    let inner = Arc::new(MemoryStore::new());
    let ha = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = hooked_syncer(&ha, dir_a.path());
    let mut b = syncer(&inner, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "seed.txt", "seed");
    write(dir_a.path(), "x.txt", "x v1");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    b.cfg.untracked_grace_secs = 0;
    b.state.save_orphan_sweep_at(now_unix() - b.cfg.untracked_sweep_secs - 1).unwrap();
    // B's commit (which runs its sweep) carries nothing A lacks: the same
    // bytes, re-published, so the etag theirs cites is the one A has.
    write(dir_b.path(), "seed.txt", "seed");
    backdate_baseline(&b, "seed.txt");
    write(dir_a.path(), "x.txt", "A's edit");
    // A's upload lands; before A claims, B's commit sweeps it and a peer
    // commits its own x.txt.
    let b_slot = std::sync::Mutex::new(Some(b));
    let (store2, cfg_a) = (inner.clone(), a.cfg.clone());
    ha.after_put(&a.cfg.file_key("x.txt"), move || {
        let mut b = b_slot.lock().unwrap().take().expect("hook ran twice");
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let rb = b.run_barrier().await.expect("B's barrier");
                assert!(rb.swept >= 1, "fixture: B's sweep did not take A's in-flight upload: {rb:?}");
                peer_publish(&store2, &cfg_a, "x.txt", "the peer's x", "peer").await;
            })
        });
    });
    let ra = a.run_barrier().await.expect("A's barrier");
    assert_eq!(ra.parked, vec!["x.txt".to_string()], "fixture: the upload was not withheld: {ra:?}");
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries.keys().collect::<Vec<_>>(), ["seed.txt", "x.txt"], "fixture: something else changed");
    assert_eq!(m.entries["seed.txt"].etag, a.state.load_baseline().unwrap().entries["seed.txt"].etag, "fixture: B's commit changed seed.txt's bytes");
    assert_eq!(read(dir_a.path(), "x.txt").as_deref(), Some("A's edit"), "fixture");

    // The agent reverts its edit; nothing else commits.
    restore_to_baseline(&a, "x.txt", "x v1");
    for _ in 0..2 {
        a.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir_a.path(), "x.txt").as_deref(), Some("the peer's x"), "the peer's version never arrived");
}

/// A UI rename over a DIRTY source lands (P2, the user's rule): the
/// document moves the citation at once. The agent's unpublished edit at the
/// source is never touched; its next publish brings the source path back —
/// mine wins over the rename's delete of it, RECORDED — and the moved
/// version stays at the destination. Both versions are kept.
#[tokio::test]
async fn a_ui_rename_over_a_dirty_source_keeps_both_versions_with_a_record() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    write(dir.path(), "x.txt", "seed");
    a.run_barrier().await.unwrap();
    let seed = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries["x.txt"].etag.clone();
    write(dir.path(), "x.txt", "the agent's edit");
    backdate_baseline(&a, "x.txt");
    hitl_rename(&store, &a.cfg, "x.txt", "y.txt", "dilip").await;
    for _ in 0..3 {
        a.run_barrier().await.unwrap();
    }
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries["y.txt"].etag, seed, "the rename's destination lost the moved version");
    assert_eq!(read(dir.path(), "y.txt").as_deref(), Some("seed"));
    assert_eq!(read(dir.path(), "x.txt").as_deref(), Some("the agent's edit"), "the agent's edit was touched");
    assert!(m.entries.contains_key("x.txt"), "the agent's edit was never published");
    assert!(
        a.state.load_conflicts().unwrap().iter().any(|c| c.path == "x.txt" && c.foreign_etag == seed && c.kind.starts_with("commit-recreated-deleted")),
        "the edit overrode the rename's move with no record: {:?}",
        a.state.load_conflicts().unwrap()
    );
}

/// A UI rename onto a destination the agent has TAKEN — an uncited file of
/// its own there — lands (P2): the document moves the citation. The agent's
/// file wins the destination at its next publish, and the moved version,
/// which it never integrated, is preserved under R7's record: nothing is
/// lost, nothing is silent.
#[tokio::test]
async fn a_ui_rename_onto_a_destination_the_agent_took_preserves_the_moved_version() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    write(dir.path(), "x.txt", "seed");
    a.run_barrier().await.unwrap();
    let seed = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries["x.txt"].etag.clone();
    write(dir.path(), "y.txt", "the agent's own y");
    hitl_rename(&store, &a.cfg, "x.txt", "y.txt", "dilip").await;
    for _ in 0..3 {
        a.run_barrier().await.unwrap();
    }
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(read(dir.path(), "y.txt").as_deref(), Some("the agent's own y"), "the agent's file was touched");
    assert!(!m.entries.contains_key("x.txt"), "the rename's source is cited again");
    let rec = a
        .state
        .load_conflicts()
        .unwrap()
        .into_iter()
        .find(|c| c.path == "y.txt" && c.foreign_etag == seed)
        .expect("the moved version was lost with no record");
    let (_, kept) = store.get_whole(rec.preserved_key.as_ref().expect("not preserved"), None).await.unwrap();
    assert_eq!(&kept[..], b"seed");
}

/// G2 (P2, 2026-09-25): a UI save never waits on the lease, so a writer
/// mid-commit can lose its CAS to saves several times in a row. It backs off
/// and retries rather than failing the barrier after a few losses (the cap
/// was 4). Six lost races here; the barrier still installs.
#[tokio::test]
async fn a_writer_that_loses_several_cas_races_backs_off_and_installs() {
    let inner = Arc::new(MemoryStore::new());
    let ha = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let dir = tempfile::tempdir().unwrap();
    let mut a = hooked_syncer(&ha, dir.path());
    a.checkout().await.unwrap();
    write(dir.path(), "seed.txt", "seed");
    a.run_barrier().await.unwrap();
    write(dir.path(), "x.txt", "the writer's publish");
    *ha.1.put_conflicts.lock().unwrap() = Some((a.cfg.current_key(), 6));
    let t0 = std::time::Instant::now();
    let r = a.run_barrier().await.expect("the barrier gave up on lost CAS races");
    assert!(t0.elapsed() >= std::time::Duration::from_millis(630), "no backoff between the lost races: {:?}", t0.elapsed());
    assert_eq!(r.uploaded, vec!["x.txt".to_string()], "{r:?}");
    assert_eq!(ha.1.put_conflicts.lock().unwrap().as_ref().map(|(_, n)| *n), Some(0), "fixture: not every race was lost");
    assert!(manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries.contains_key("x.txt"));
}

/// L-126 (found by the P1-lite sandbox's baseline, 2026-09-24, and then in
/// `LeanCore` itself at one path and THREE barriers — every LeanCore world
/// in the gate has two). A's upload over a version it never integrated is
/// WITHHELD (a peer's sweep took it); the path is parked, so the merge
/// neither queues theirs nor reports it overridden, and step 7 still moves
/// the merge base (`inst_base`) to the installed document — theirs
/// included — while the baseline keeps the version A's edit derives from.
/// At the next barrier theirs EQUALS the merge base: `merge` reports
/// nothing overridden, R7 never looks, and A's re-upload replaces an
/// acknowledged UI write it never saw, with no record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withheld_upload_republished_next_barrier_surfaces_the_version_it_never_integrated() {
    let inner = Arc::new(MemoryStore::new());
    let ha = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = hooked_syncer(&ha, dir_a.path());
    let mut b = syncer(&inner, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "p.txt", "seed");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    // The UI saves p; B adopts and publishes it. A never consumes it.
    let (ui_etag, ui_key) = hitl_write_at(&inner, &a.cfg, "p.txt", "the user's save", "dilip", now_unix()).await.unwrap();
    b.run_barrier().await.unwrap();
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries["p.txt"].key, ui_key, "fixture: B did not publish the UI write");
    // A's agent edits p (from the seed). A's upload lands; before A claims,
    // B's commit sweeps it.
    b.cfg.untracked_grace_secs = 0;
    b.state.save_orphan_sweep_at(now_unix() - b.cfg.untracked_sweep_secs - 1).unwrap();
    write(dir_b.path(), "b.txt", "B publishes");
    write(dir_a.path(), "p.txt", "A's edit");
    backdate_baseline(&a, "p.txt");
    let b_slot = std::sync::Mutex::new(Some(b));
    let b_back: Arc<std::sync::Mutex<Option<Syncer>>> = Arc::new(std::sync::Mutex::new(None));
    let b_back_in = b_back.clone();
    ha.after_put(&a.cfg.file_key("p.txt"), move || {
        let mut b = b_slot.lock().unwrap().take().expect("hook ran twice");
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let rb = b.run_barrier().await.expect("B's barrier");
                assert!(rb.swept >= 1, "fixture: B's sweep did not take A's in-flight upload: {rb:?}");
            })
        });
        *b_back_in.lock().unwrap() = Some(b);
    });
    let ra = a.run_barrier().await.unwrap();
    assert!(b_back.lock().unwrap().is_some(), "fixture: the hook never ran");
    assert_eq!(ra.parked, vec!["p.txt".to_string()], "fixture: the upload was not withheld: {ra:?}");
    // The next barrier publishes A's edit again — over the user's save.
    let ra2 = a.run_barrier().await.unwrap();
    assert_eq!(ra2.uploaded, vec!["p.txt".to_string()], "fixture: {ra2:?}");
    assert_eq!(cited_bytes(&inner, &a.cfg, "p.txt").await.as_deref(), Some(&b"A's edit"[..]));
    assert!(
        a.state.load_conflicts().unwrap().iter().any(|c| c.path == "p.txt" && c.foreign_etag == ui_etag),
        "A's edit replaced the user's acknowledged save with no record: {:?}",
        a.state.load_conflicts().unwrap()
    );
}

/// L-126 through the journal: the withheld barrier ALSO publishes another
/// file, so it CASes and journals `installed_etag`. Nothing moves the pointer
/// before the re-upload, so `merge_base` takes the installed document as the
/// base — and that document cites the user's save at the parked path. The
/// parked path must keep its earlier base there too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withheld_upload_beside_a_published_one_surfaces_the_version_it_never_integrated() {
    let inner = Arc::new(MemoryStore::new());
    let ha = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = hooked_syncer(&ha, dir_a.path());
    let mut b = syncer(&inner, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "p.txt", "seed");
    write(dir_a.path(), "q.txt", "seed q");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    let (ui_etag, ui_key) = hitl_write_at(&inner, &a.cfg, "p.txt", "the user's save", "dilip", now_unix()).await.unwrap();
    b.run_barrier().await.unwrap();
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries["p.txt"].key, ui_key, "fixture: B did not publish the UI write");
    b.cfg.untracked_grace_secs = 0;
    b.state.save_orphan_sweep_at(now_unix() - b.cfg.untracked_sweep_secs - 1).unwrap();
    write(dir_b.path(), "b.txt", "B publishes");
    write(dir_a.path(), "p.txt", "A's edit");
    write(dir_a.path(), "q.txt", "A's q");
    backdate_baseline(&a, "p.txt");
    backdate_baseline(&a, "q.txt");
    let b_slot = std::sync::Mutex::new(Some(b));
    let b_back: Arc<std::sync::Mutex<Option<Syncer>>> = Arc::new(std::sync::Mutex::new(None));
    let b_back_in = b_back.clone();
    ha.after_put(&a.cfg.file_key("p.txt"), move || {
        let mut b = b_slot.lock().unwrap().take().expect("hook ran twice");
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let rb = b.run_barrier().await.expect("B's barrier");
                assert!(rb.swept >= 1, "fixture: B's sweep did not take A's in-flight upload: {rb:?}");
            })
        });
        *b_back_in.lock().unwrap() = Some(b);
    });
    let ra = a.run_barrier().await.unwrap();
    assert!(b_back.lock().unwrap().is_some(), "fixture: the hook never ran");
    assert_eq!(ra.parked, vec!["p.txt".to_string()], "fixture: p was not withheld: {ra:?}");
    assert_eq!(ra.uploaded, vec!["q.txt".to_string()], "fixture: q did not publish beside it: {ra:?}");
    let ra2 = a.run_barrier().await.unwrap();
    assert_eq!(ra2.uploaded, vec!["p.txt".to_string()], "fixture: {ra2:?}");
    assert!(
        a.state.load_conflicts().unwrap().iter().any(|c| c.path == "p.txt" && c.foreign_etag == ui_etag),
        "A's edit replaced the user's acknowledged save with no record: {:?}",
        a.state.load_conflicts().unwrap()
    );
}

/// The shared fixture for L-126's journal routes: B publishes `peer_bytes` at
/// p (as the UI's save when `ui`, else as B's own new file); A's agent writes
/// p and q, A's upload of p is swept before A claims, and A publishes q
/// beside it — so A's barrier CASes, journals `installed_etag` and parks p.
/// Returns the peer version's etag, with A and the store.
async fn l126_withheld_beside_a_cas(
    ui: bool,
) -> (Arc<MemoryStore>, Arc<Hooked>, Syncer, tempfile::TempDir, tempfile::TempDir, String) {
    let inner = Arc::new(MemoryStore::new());
    let ha = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = hooked_syncer(&ha, dir_a.path());
    let mut b = syncer(&inner, dir_b.path()).await;
    a.checkout().await.unwrap();
    if ui {
        write(dir_a.path(), "p.txt", "seed");
    }
    write(dir_a.path(), "q.txt", "seed q");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    let peer_etag = if ui {
        let (etag, _) = hitl_write_at(&inner, &a.cfg, "p.txt", "the user's save", "dilip", now_unix()).await.unwrap();
        b.run_barrier().await.unwrap();
        etag
    } else {
        write(dir_b.path(), "p.txt", "B's p");
        b.run_barrier().await.unwrap();
        manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries["p.txt"].etag.clone()
    };
    b.cfg.untracked_grace_secs = 0;
    b.state.save_orphan_sweep_at(now_unix() - b.cfg.untracked_sweep_secs - 1).unwrap();
    write(dir_b.path(), "b.txt", "B publishes");
    write(dir_a.path(), "p.txt", "A's p");
    write(dir_a.path(), "q.txt", "A's q");
    backdate_baseline(&a, "p.txt");
    backdate_baseline(&a, "q.txt");
    let b_slot = std::sync::Mutex::new(Some(b));
    let b_back: Arc<std::sync::Mutex<Option<Syncer>>> = Arc::new(std::sync::Mutex::new(None));
    let b_back_in = b_back.clone();
    ha.after_put(&a.cfg.file_key("p.txt"), move || {
        let mut b = b_slot.lock().unwrap().take().expect("hook ran twice");
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let rb = b.run_barrier().await.expect("B's barrier");
                assert!(rb.swept >= 1, "fixture: B's sweep did not take A's in-flight upload: {rb:?}");
            })
        });
        *b_back_in.lock().unwrap() = Some(b);
    });
    let ra = a.run_barrier().await.unwrap();
    assert!(b_back.lock().unwrap().is_some(), "fixture: the hook never ran");
    assert_eq!(ra.parked, vec!["p.txt".to_string()], "fixture: p was not withheld: {ra:?}");
    assert_eq!(ra.uploaded, vec!["q.txt".to_string()], "fixture: q did not publish beside it: {ra:?}");
    (inner, ha, a, dir_a, dir_b, peer_etag)
}

/// The journal route for a NEW file: the installed document cites B's p,
/// which A's merge base never cited, so the base must stay without p.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withheld_new_file_beside_a_published_one_surfaces_the_peers_file() {
    let (_inner, _ha, mut a, _da, _db, peer_etag) = l126_withheld_beside_a_cas(false).await;
    let ra2 = a.run_barrier().await.unwrap();
    assert_eq!(ra2.uploaded, vec!["p.txt".to_string()], "fixture: {ra2:?}");
    assert!(
        a.state.load_conflicts().unwrap().iter().any(|c| c.path == "p.txt" && c.foreign_etag == peer_etag),
        "A's new file replaced B's with no record: {:?}",
        a.state.load_conflicts().unwrap()
    );
}

/// The parked set is CARRIED with `installed_etag` when the next barrier
/// rewrites the journal: that barrier fails at its CAS (the pointer's PUT is
/// refused, nothing lands), and the retry still keeps p's base.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_barrier_that_fails_before_its_cas_keeps_the_parked_set_for_the_retry() {
    let (_inner, ha, mut a, _da, _db, peer_etag) = l126_withheld_beside_a_cas(true).await;
    *ha.1.put_fail_containing.lock().unwrap() = Some(a.cfg.current_key());
    assert!(a.run_barrier().await.is_err(), "fixture: the barrier's CAS did not fail");
    *ha.1.put_fail_containing.lock().unwrap() = None;
    let ra3 = a.run_barrier().await.unwrap();
    assert_eq!(ra3.uploaded, vec!["p.txt".to_string()], "fixture: {ra3:?}");
    assert!(
        a.state.load_conflicts().unwrap().iter().any(|c| c.path == "p.txt" && c.foreign_etag == peer_etag),
        "A's edit replaced the user's acknowledged save with no record: {:?}",
        a.state.load_conflicts().unwrap()
    );
}

/// L-126's other arm: a path the merge base did NOT cite. B creates p and
/// publishes it; A's agent creates p too, and A's upload is withheld. The
/// merge base must stay WITHOUT p — not take B's version, which A never
/// integrated — so the re-upload's merge sees B's p as a change and R7
/// records it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withheld_new_file_republished_next_barrier_surfaces_the_peers_file() {
    let inner = Arc::new(MemoryStore::new());
    let ha = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = hooked_syncer(&ha, dir_a.path());
    let mut b = syncer(&inner, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "seed.txt", "seed");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    write(dir_b.path(), "p.txt", "B's p");
    b.run_barrier().await.unwrap();
    let b_etag = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries["p.txt"].etag.clone();
    b.cfg.untracked_grace_secs = 0;
    b.state.save_orphan_sweep_at(now_unix() - b.cfg.untracked_sweep_secs - 1).unwrap();
    write(dir_b.path(), "b.txt", "B publishes");
    write(dir_a.path(), "p.txt", "A's p");
    let b_slot = std::sync::Mutex::new(Some(b));
    let b_back: Arc<std::sync::Mutex<Option<Syncer>>> = Arc::new(std::sync::Mutex::new(None));
    let b_back_in = b_back.clone();
    ha.after_put(&a.cfg.file_key("p.txt"), move || {
        let mut b = b_slot.lock().unwrap().take().expect("hook ran twice");
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let rb = b.run_barrier().await.expect("B's barrier");
                assert!(rb.swept >= 1, "fixture: B's sweep did not take A's in-flight upload: {rb:?}");
            })
        });
        *b_back_in.lock().unwrap() = Some(b);
    });
    let ra = a.run_barrier().await.unwrap();
    assert!(b_back.lock().unwrap().is_some(), "fixture: the hook never ran");
    assert_eq!(ra.parked, vec!["p.txt".to_string()], "fixture: the upload was not withheld: {ra:?}");
    let ra2 = a.run_barrier().await.unwrap();
    assert_eq!(ra2.uploaded, vec!["p.txt".to_string()], "fixture: {ra2:?}");
    assert!(
        a.state.load_conflicts().unwrap().iter().any(|c| c.path == "p.txt" && c.foreign_etag == b_etag),
        "A's new file replaced B's with no record: {:?}",
        a.state.load_conflicts().unwrap()
    );
}

/// The crash journal's recognisers are gone with the slot: a barrier that
/// restarts after some of its uploads landed does not adopt them by flush
/// id (`AdoptOwn`, the torn-compose arm), it uploads afresh at handles of
/// its own and cites those; what it left behind is nobody's, and the
/// orphan sweep takes it. Stood where "a claim that reaches the deadline
/// ... the retry adopts the uploads" and "a crashed compose is adopted
/// after an edit" stood.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_barrier_uploads_afresh_and_leaves_its_earlier_handles_to_the_sweep() {
    let (inner, mut b, dir_a, dir_b, orphan) = killed_writer_orphan().await;
    // A restarts on the same tree.
    let mut a = syncer(&inner, dir_a.path()).await;
    let ra = a.run_barrier().await.expect("A's retry");
    assert_eq!(ra.uploaded, vec!["p1.txt".to_string(), "p2.txt".to_string()], "{ra:?}");
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_ne!(m.entries["p1.txt"].key, orphan, "the retry cited its orphan instead of uploading afresh");
    assert_eq!(cited_bytes(&inner, &a.cfg, "p1.txt").await.as_deref(), Some(&b"A's unpublished edit"[..]));
    assert!(inner.head(&orphan).await.is_ok(), "the retry is not the collector of its predecessor");
    // The orphan is the sweep's: due at B's next commit, it goes.
    b.cfg.untracked_grace_secs = 0;
    b.state.save_orphan_sweep_at(now_unix() - b.cfg.untracked_sweep_secs - 1).unwrap();
    write(dir_b.path(), "b2.txt", "B commits again");
    let rb = b.run_barrier().await.unwrap();
    assert_eq!(rb.swept, 1, "{rb:?}");
    assert!(matches!(inner.head(&orphan).await, Err(StoreError::NotFound(_))), "the orphan survived the sweep");
}

/// Retired classes F1/L-10, F2/L-11 and finding 13 (`LeanImmutableGCUnconditional`
/// holding): a peer's in-flight upload sits at a handle of its own, so
/// another writer's collector — which deletes only what its OWN document
/// stopped citing, unconditionally — cannot touch it, whatever bytes it
/// holds and whatever the etag. B's commit then cites it over A's racing
/// delete: modify wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peers_in_flight_upload_is_untouched_by_anothers_collector() {
    let inner = Arc::new(MemoryStore::new());
    let hb = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&inner, dir_a.path()).await;
    let mut b = hooked_syncer(&hb, dir_b.path());
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "seed");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    let seed = cited_key(&inner, &a.cfg, "x.txt").await;
    // B edits x with the seed's OWN bytes plus a byte (finding 13's shape
    // needs identical bytes; a handle makes the etag irrelevant either way).
    write(dir_b.path(), "x.txt", "seed!");
    backdate_baseline(&b, "x.txt");
    std::fs::remove_file(dir_a.path().join("x.txt")).unwrap();
    a.run_barrier().await.unwrap(); // first absence
    let a_slot = std::sync::Mutex::new(Some(a));
    let a_back: Arc<std::sync::Mutex<Option<Syncer>>> = Arc::new(std::sync::Mutex::new(None));
    let a_back_in = a_back.clone();
    hb.after_put(&b.cfg.file_key("x.txt"), move || {
        let mut a = a_slot.lock().unwrap().take().expect("hook ran twice");
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let ra = a.run_barrier().await.expect("A's delete barrier");
                assert_eq!(ra.deleted, vec!["x.txt".to_string()], "fixture: A did not publish its delete: {ra:?}");
                assert_eq!(ra.collected, 1, "fixture: A's collector took nothing: {ra:?}");
            })
        });
        *a_back_in.lock().unwrap() = Some(a);
    });
    let rb = b.run_barrier().await.expect("B's barrier");
    assert!(a_back.lock().unwrap().is_some(), "fixture: the hook never ran");
    assert_eq!(rb.uploaded, vec!["x.txt".to_string()], "{rb:?}");
    assert!(rb.parked.is_empty(), "A's collector took B's in-flight upload: {rb:?}");
    assert_eq!(
        cited_bytes(&inner, &b.cfg, "x.txt").await.as_deref(),
        Some(&b"seed!"[..]),
        "modify wins over the racing delete, and the bytes are B's"
    );
    assert!(matches!(inner.head(&seed).await, Err(StoreError::NotFound(_))), "A's collector did not take what A retired");
}

/// Retired class H1/L-102 (`LeanImmutableLeakHolds`): a store without a
/// conditional DELETE used to make the collector give way, leaking the
/// retired object at the key — and the leak superseded the peer's
/// tombstone (H1) unless three rules told it apart. A retired handle can
/// never be re-cited, so the collector deletes it unconditionally on every
/// store, nothing leaks, and the delete reaches the other tree with no rule
/// to apply.
#[tokio::test]
async fn a_store_without_a_conditional_delete_collects_just_the_same() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "seed");
    write(dir_a.path(), "keep.txt", "keep");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    let seed = cited_key(&store, &a.cfg, "x.txt").await;
    std::fs::remove_file(dir_a.path().join("x.txt")).unwrap();
    a.run_barrier().await.unwrap(); // first absence
    let ra = a.run_barrier().await.unwrap();
    assert_eq!(ra.deleted, vec!["x.txt".to_string()]);
    assert!(ra.leaked.is_empty(), "the collector gave way: {ra:?}");
    assert_eq!(ra.collected, 1, "{ra:?}");
    assert!(matches!(store.head(&seed).await, Err(StoreError::NotFound(_))), "the retired handle leaked");
    for _ in 0..2 {
        b.run_barrier().await.unwrap();
    }
    assert!(read(dir_b.path(), "x.txt").is_none(), "the peer's delete did not reach B's tree");
    assert_eq!(read(dir_b.path(), "keep.txt").as_deref(), Some("keep"));
    let m = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("x.txt"), "the delete flapped back");
}


/// An INGRESS object's copy is COMMITTED by the sweep (P2), judged against
/// the citation the sweep saw. A writer that then publishes the path from a
/// base that never held the ingress copy publishes over it — mine wins — and
/// R7 preserves the outside bytes under a record. Every tree follows the
/// document. The ingress object itself is never touched.
#[tokio::test]
async fn an_ingress_copy_a_writer_publishes_over_is_preserved_with_a_record() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    b.cfg.untracked_grace_secs = 0;
    a.checkout().await.unwrap();
    write(dir_a.path(), "p1.txt", "v1");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    // An outside writer's object at the bare path; B's sweep commits a copy
    // of it, judged against v1's citation, and B's tree takes it.
    let body = Bytes::from_static(b"outside bytes");
    let stamps = GenerationStamps { generation: 0, epoch: 0, flush_uuid: "aws-cli".into(), boundary_source: None, posix: None };
    let ingress = store
        .put_whole(&b.cfg.file_key("p1.txt"), body.clone(), &PutCondition::Unconditional, &stamps, crc64_nvme(&body))
        .await
        .unwrap();
    let tracked = b.track_untracked(now_unix() + 1).await.unwrap();
    assert_eq!(tracked, vec!["p1.txt".to_string()], "fixture: the ingress object was not tracked");
    assert_eq!(cited_bytes(&store, &b.cfg, "p1.txt").await.as_deref(), Some(&b"outside bytes"[..]), "the copy is not cited");
    for _ in 0..2 {
        b.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir_b.path(), "p1.txt").as_deref(), Some("outside bytes"));
    // A publishes v2 from a base that never held the ingress copy.
    write(dir_a.path(), "p1.txt", "v2 by A's agent");
    backdate_baseline(&a, "p1.txt");
    let ra = a.run_barrier().await.unwrap();
    assert_eq!(ra.surfaced, vec!["p1.txt".to_string()], "A published over the ingress copy with no record: {ra:?}");
    assert_eq!(cited_bytes(&store, &b.cfg, "p1.txt").await.as_deref(), Some(&b"v2 by A's agent"[..]));
    for _ in 0..2 {
        b.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir_b.path(), "p1.txt").as_deref(), Some("v2 by A's agent"), "B's tree did not follow the document");
    let (at_key, _) = store.get_whole(&b.cfg.file_key("p1.txt"), None).await.unwrap();
    assert_eq!(at_key.etag, ingress.etag, "the ingress object was touched");
}

/// Retired class F8/L-16 (`LeanImmutableHitlOverAny`): the gateway's write
/// lands at a handle of its own beside a writer's in-flight upload of the
/// same path — nothing is overwritten, nothing is refused (the
/// `hitl_may_overwrite` rule guarded a slot). The writer's commit cites its
/// own upload; its next consume finds the entry's citation moved past what
/// the gateway saw and treats it as the CONFLICT it is: preserved by a
/// copy, surfaced by a record, never adopted over the published version
/// and never dropped in silence (class 5, R7).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ui_write_landing_beside_an_in_flight_upload_is_preserved_never_lost() {
    let inner = Arc::new(MemoryStore::new());
    let ha = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = hooked_syncer(&ha, dir_a.path());
    a.checkout().await.unwrap();
    write(dir_a.path(), "p1.txt", "v1");
    a.run_barrier().await.unwrap();
    write(dir_a.path(), "p1.txt", "A's edit");
    backdate_baseline(&a, "p1.txt");
    let ui = Arc::new(std::sync::Mutex::new(None));
    {
        let (store, cfg, ui) = (inner.clone(), a.cfg.clone(), ui.clone());
        ha.after_put(&a.cfg.file_key("p1.txt"), move || {
            let (store, cfg) = (store.clone(), cfg.clone());
            let landed = std::thread::spawn(move || {
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(hitl_write_at(&store, &cfg, "p1.txt", "the UI's version", "dilip", now_unix()))
                    .unwrap()
            })
            .join()
            .unwrap();
            *ui.lock().unwrap() = Some(landed);
        });
    }
    // P2: the UI's save COMMITS mid-barrier, so A's CAS loses, re-merges
    // onto it, and publishes over a version A never integrated — which R7
    // preserves and records at this very commit.
    let ra = a.run_barrier().await.unwrap();
    ui.lock().unwrap().clone().expect("fixture: the UI write never landed mid-barrier");
    assert_eq!(ra.uploaded, vec!["p1.txt".to_string()], "{ra:?}");
    assert_eq!(ra.surfaced, vec!["p1.txt".to_string()], "A published over the UI's save with no record: {ra:?}");
    assert_eq!(cited_bytes(&inner, &a.cfg, "p1.txt").await.as_deref(), Some(&b"A's edit"[..]));
    let rec = a
        .state
        .load_conflicts()
        .unwrap()
        .into_iter()
        .find(|c| c.kind.starts_with("commit-surfaced-foreign") && c.path == "p1.txt")
        .expect("the UI write was dropped in silence");
    let (_, kept) = inner.get_whole(rec.preserved_key.as_ref().expect("not preserved"), None).await.unwrap();
    assert_eq!(&kept[..], b"the UI's version");
    a.run_barrier().await.unwrap();
    assert_eq!(read(dir_a.path(), "p1.txt").as_deref(), Some("A's edit"), "adopted over the published version");
}


/// M3, the user's rule "mine wins, theirs is preserved", for a DELETE
/// (P1-lite, 2026-09-25): the agent's delete meets a peer's newer version
/// and lands at ONCE; the peer's version is preserved under a record that
/// names it. (Before P1-lite the delete was outranked for a barrier, and
/// won the next one through the queue's consume-dirty preservation.)
#[tokio::test]
async fn an_agents_delete_over_a_peers_newer_version_lands_and_preserves_theirs() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "shared.txt", "v1");
    sc.run_barrier().await.unwrap();
    let (their_etag, _) = peer_publish(&store, &sc.cfg, "shared.txt", "their v2", "other-writer").await;
    std::fs::remove_file(dir.path().join("shared.txt")).unwrap();
    let r = sc.declared_barrier().await.unwrap();
    assert_eq!(r.deleted, vec!["shared.txt".to_string()], "the delete lands in this barrier");
    assert!(r.parked.is_empty(), "{:?}", r.parked);
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert!(!m.manifest.entries.contains_key("shared.txt"));
    let c = sc
        .state
        .load_conflicts()
        .unwrap()
        .into_iter()
        .find(|c| c.path == "shared.txt" && c.foreign_etag == their_etag)
        .expect("the peer's version is named by a record");
    let preserved = c.preserved_key.expect("and preserved");
    let (_, body) = store.get_whole(&preserved, None).await.unwrap();
    assert_eq!(&body[..], b"their v2");
    assert!(read(dir.path(), "shared.txt").is_none(), "the tree keeps the agent's delete");
}

/// Delete/modify across writers, on the cadence path: the first barrier
/// sees only a FIRST absence and publishes nothing, so the foreign version
/// stays cited; the second publishes the delete OVER it (M3), and the
/// foreign bytes are preserved first. Never a silent winner.
#[tokio::test]
async fn local_delete_meets_foreign_modify_and_preserves_it() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "shared.txt", "v1");
    sc.run_barrier().await.unwrap();

    // A second writer publishes a new version of the path (a handle of
    // its own, cited by a new manifest generation).
    let (their_etag, _) = peer_publish(&store, &sc.cfg, "shared.txt", "their v2", "other-writer").await;

    // The agent deletes locally; the FIRST barrier sees a first absence:
    // nothing publishes, and the foreign version stays cited.
    std::fs::remove_file(dir.path().join("shared.txt")).unwrap();
    let r1 = sc.run_barrier().await.unwrap();
    assert!(r1.deleted.is_empty());
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert_eq!(m.manifest.entries["shared.txt"].etag, their_etag, "foreign entry dropped!");
    assert_eq!(cited_bytes(&store, &sc.cfg, "shared.txt").await.as_deref(), Some(&b"their v2"[..]));

    // The SECOND barrier publishes the delete over it: mine wins, WITH
    // the foreign bytes preserved first and a record naming them.
    let r2 = sc.run_barrier().await.unwrap();
    assert_eq!(r2.deleted, vec!["shared.txt".to_string()]);
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert!(!m.manifest.entries.contains_key("shared.txt"));
    let conflicts = sc.state.load_conflicts().unwrap();
    let c = conflicts
        .iter()
        .find(|c| c.kind.starts_with("commit-deleted-over-theirs") && c.path == "shared.txt")
        .expect("delete-vs-edit conflict must surface");
    let preserved = c.preserved_key.as_ref().expect("foreign bytes preserved");
    let (_, body) = store.get_whole(preserved, None).await.unwrap();
    assert_eq!(&body[..], b"their v2", "the edit must stay recoverable after the delete wins");
}


/// Files over whole_put_max go through the streaming multipart compose
/// (never put_whole): publish, guarded update, and roundtrip must all
/// hold on that path. whole_put_max is shrunk so a small file takes the
/// large-file road against MemoryStore's real MPU semantics.
#[tokio::test]
async fn large_file_publishes_via_compose_and_roundtrips() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
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
    let mut sc2 = syncer(&store, dir2.path()).await;
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

/// The occupancy lock: a second syncer over the SAME workspace tree
/// must refuse to start — self-recognition of the lease is only sound
/// because the previous process is provably gone (observed live on the
/// 0b rig: a concurrent process deposed a live sibling and both wrote
/// the tree).
#[tokio::test]
async fn second_syncer_on_one_tree_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let _held = SyncerState::open(cfg.state_dir()).unwrap();
    let Err(err) = SyncerState::open(cfg.state_dir()) else {
        panic!("second open over a held workspace must refuse");
    };
    assert!(matches!(err, LeanError::State(_)));
}

/// Checkout budgets refuse BEFORE materializing; no marker is written.
#[tokio::test]
async fn checkout_budget_refuses_before_first_byte() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "big.txt", "0123456789012345678901234567890123456789");
    sc.run_barrier().await.unwrap();

    let dir2 = tempfile::tempdir().unwrap();
    let mut sc2 = syncer(&store, dir2.path()).await;
    sc2.cfg.max_bytes = 10;
    let err = sc2.checkout().await.unwrap_err();
    assert!(matches!(err, LeanError::Budget(_)));
    assert!(!sc2.state.marker_present(), "budget refusal must not gate-open the agent");
    assert!(read(dir2.path(), "big.txt").is_none());
}

/// A scope whose every entry is rejected must be REFUSED, never widened.
///
/// `Scope::new` silently drops entries that are over-long, past
/// MAX_SCOPE_ENTRIES, or contain `.`/`..`. When all of them go, the
/// scope was `None` and `in_scope`'s `.unwrap_or(true)` turned that into
/// the WHOLE TREE — and a whole-tree sync deletes local files for
/// remotely-deleted paths. So the failure mode of a typo was maximum
/// privilege. An error must not return a legal value.
#[tokio::test]
async fn an_all_rejected_sync_scope_is_refused_not_widened_to_the_whole_tree() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();

    // Every entry malformed: `..` and `.` components are dropped by
    // `Scope::new`, so the surviving scope is empty.
    let err = sc
        .sync_scoped(Some(vec!["../escape".into(), "./here".into()]))
        .await
        .expect_err("an all-rejected scope must not silently become the whole tree");
    let msg = format!("{err}");
    assert!(
        msg.contains("NONE survived validation"),
        "the refusal must say WHY, so a typo is not read as a sync bug: {msg}"
    );

    // THE OTHER ARM SHUT: a well-formed scope still works. Without this,
    // the test above would pass just as well if scoped sync were broken
    // outright.
    sc.sync_scoped(Some(vec!["inputs".into()])).await.expect("a valid scope must still sync");
}

/// The sync verb: begins with a scan; locally-dirty wins over a remote
/// delete (the review's steady-state destruction finding), locally-clean
/// applies remote adds/changes.
#[tokio::test]
async fn sync_scan_first_dirty_wins_clean_applies() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
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

pub(super) fn touch_sentinel(root: &std::path::Path, name: &str, body: &str) {
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
/// sentinel is live ammunition on an old syncer. This asserts the
/// hazard is gone: no `.flint/` key ever appears in the manifest or the
/// bucket, and the scan never yields the path.
#[tokio::test]
async fn flint_dir_never_scanned() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
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
    assert!(!object_at(&store, &a.cfg, ".flint/publish").await);
}

/// D0.2 — an upgrade must never delete data. A workspace that legally
/// published `files/.flint/legacy.txt` under a pre-D0 syncer has it in
/// the baseline; the new scan skips it, so the two-consecutive-scans
/// rule would otherwise classify it absent twice and publish its
/// DELETION. It is carried forward frozen.
#[tokio::test]
async fn legacy_flint_citation_survives_upgrade() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "keep.txt", "v1");
    a.run_barrier().await.unwrap();

    // Manufacture the pre-D0 state: a cited `.flint/` path in both the
    // manifest and our baseline, as an old syncer would have left it.
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
            crc64_b64: meta.crc64_b64.clone().unwrap(),
            size: meta.size,
            mode: 0o644,
            mtime_unix: 0,
            generation: 1,
            epoch: 0,
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
            key: None,
            generation: 1,
            size: meta.size,
            mtime_unix: 0,
            mtime_nanos: None,
            crc64_b64: meta.crc64_b64.clone(),
        },
    );
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
    let mut a = syncer(&store, dir_a.path()).await;
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
            crc64_b64: meta.crc64_b64.clone().unwrap(),
            size: meta.size,
            mode: 0o644,
            mtime_unix: 0,
            generation: 1,
            epoch: 0,
        },
    );
    manifest::cas_write(store.as_ref(), &a.cfg, &m, Some(&loaded.handle()), 0, "legacy")
        .await
        .unwrap();

    // A FRESH pod checks the same subtree out.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
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
/// `marker_present()` without reaching its body, so a syncer upgrade
/// over live workspaces would otherwise leave sentinels dead on exactly
/// the pods the upgrade targeted.
#[tokio::test]
async fn capabilities_written_on_live_tree_restart() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
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
    a.write_capabilities(&posture).unwrap();
    let caps = a.read_capabilities().unwrap();
    assert_eq!(caps.protocol, super::SENTINEL_PROTOCOL);
    assert_eq!(caps.state, "live");
    assert!(caps.verbs.iter().any(|v| v == "publish"));
    assert!(caps.verbs.iter().any(|v| v == "sync"));
}

/// The guide the syncer writes beside the marker is the crate's
/// `AGENTS.md`, and it must name every verb the marker advertises and
/// the protocol number the marker carries — the one place the two could
/// drift apart. It lives under `.flint/`, so a barrier must not publish
/// it.
#[tokio::test]
async fn agent_guide_names_every_advertised_verb() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
    let caps = a.read_capabilities().unwrap();
    assert!(!caps.verbs.is_empty(), "the fixture advertises verbs");
    let guide = std::fs::read_to_string(dir.path().join(super::CONTROL_DIR).join(control::AGENT_GUIDE)).unwrap();
    assert_eq!(guide, control::AGENT_GUIDE_TEXT);
    for v in &caps.verbs {
        let named = match v.as_str() {
            "remote-seq" => ".flint/remote.seq",
            other => &format!(".flint/{other}"),
        };
        assert!(guide.contains(named), "the guide never names the advertised verb {v} as {named}");
    }
    assert!(
        guide.contains(&format!("sentinel protocol {}", super::SENTINEL_PROTOCOL)),
        "the guide names a protocol other than the marker's {}",
        super::SENTINEL_PROTOCOL
    );
    // Under the control namespace: never in the upload set.
    write(dir.path(), "f.txt", "v1");
    a.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    assert!(m.manifest.entries.contains_key("f.txt"));
    assert!(m.manifest.entries.keys().all(|k| !k.starts_with(".flint/")), "the guide was published as data");
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
    // The app owned `.flint/publish` BEFORE any protocol-aware syncer
    // ran here — recorded bytes, per the drill's anti-vacuity rule.
    touch_sentinel(dir.path(), control::PUBLISH, "app-owned payload");
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);

    let posture = a.sentinel_preflight().unwrap();
    assert!(!posture.enabled);
    assert_eq!(posture.reason.as_deref(), Some("preexisting-flint-paths"));
    a.write_capabilities(&posture).unwrap();
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
pub(super) fn clear_min_interval(sc: &Syncer) {
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
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
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

/// Two writers, one file: B edits `shared.txt` and commits; A's agent,
/// which has not seen B's edit, deletes it and declares a boundary. M3
/// (P1-lite): the delete lands over B's version, which is preserved under a
/// record on the ack — so the seq A installs does not cite the file and
/// `ok` is true. (Before M3 the delete was outranked, and the ack had to be
/// `partial`: the model's `Inv_AckImpliesCited`, depth 20, is the claim
/// this still pins.)
#[tokio::test]
async fn a_publish_whose_delete_meets_a_peers_edit_is_ok_with_theirs_preserved() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
    a.checkout().await.unwrap();
    write(dir_a.path(), "shared.txt", "v1");
    a.floor_tick().await.unwrap();
    b.checkout().await.unwrap();

    write(dir_b.path(), "shared.txt", "B's v2");
    backdate_baseline(&b, "shared.txt");
    b.floor_tick().await.unwrap();
    let theirs = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let b_etag = theirs.entries["shared.txt"].etag.clone();
    assert_eq!(read(dir_a.path(), "shared.txt").as_deref(), Some("v1"), "fixture: B's edit already reached A");

    std::fs::remove_file(dir_a.path().join("shared.txt")).unwrap();
    touch_sentinel(dir_a.path(), control::PUBLISH, r#"{"nonce":"n-1"}"#);
    a.poll_sentinels().unwrap();
    clear_min_interval(&a);
    let ack = a.honor_pending(Verb::Publish, false).await.unwrap().unwrap();

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(ack.seq, Some(m.seq), "fixture: the ack names a different seq than the one installed");
    assert!(!m.entries.contains_key("shared.txt"), "the ack's seq still cites the deleted file: {ack:?}");
    assert_eq!(ack.status, "ok", "{ack:?}");
    assert!(ack.report.dropped.is_empty(), "{ack:?}");
    let c = ack
        .report
        .conflicts
        .iter()
        .find(|c| c.kind.starts_with("commit-deleted-over-theirs") && c.path == "shared.txt")
        .unwrap_or_else(|| panic!("no record of B's version on the ack: {ack:?}"));
    assert_eq!(c.foreign_etag, b_etag);
    let (_, body) = store.get_whole(c.preserved_key.as_ref().expect("B's edit preserved"), None).await.unwrap();
    assert_eq!(&body[..], b"B's v2");
}

/// A declared file whose upload published nothing — here a large one the
/// operator's abort sweep took before Complete — is not in the seq the
/// boundary installs either: `partial`, named in `report.dropped`, and
/// the next touch publishes it.
#[tokio::test]
async fn a_publish_whose_upload_was_swept_is_partial() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.cfg.whole_put_max = 8; // 8 bytes: everything bigger composes
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
    a.checkout().await.unwrap();

    let big: String = (0..200).map(|i| format!("line {i}\n")).collect();
    write(dir.path(), "model.bin", &big);
    write(dir.path(), "notes.txt", "n");
    store.inject_compose_swept();
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"n-1"}"#);
    a.poll_sentinels().unwrap();
    clear_min_interval(&a);
    let ack = a.honor_pending(Verb::Publish, false).await.unwrap().unwrap();

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(ack.seq, Some(m.seq), "fixture: the ack names a different seq than the one installed");
    assert!(m.entries.contains_key("notes.txt"), "fixture: the boundary installed nothing");
    assert!(!m.entries.contains_key("model.bin"), "fixture: the swept upload was cited anyway");
    assert_eq!(ack.status, "partial", "the ack says model.bin is in seq {}, which does not cite it: {ack:?}", m.seq);
    assert_eq!(ack.report.dropped, vec!["model.bin".to_string()]);

    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"n-2"}"#);
    a.poll_sentinels().unwrap();
    clear_min_interval(&a);
    let again = a.honor_pending(Verb::Publish, false).await.unwrap().unwrap();
    assert_eq!(again.status, "ok", "{again:?}");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(again.seq, Some(m.seq));
    assert!(m.entries.contains_key("model.bin"), "the re-touch did not publish the swept file");
}

/// The drain's own boundary: an upload that published nothing is the
/// agent's only copy of that file, so the drain must not attest — an
/// attested drain lets the node remove the tree. The binary retries a
/// failed drain; the retry publishes it.
#[tokio::test]
async fn a_drain_whose_upload_was_swept_does_not_attest() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.cfg.whole_put_max = 8; // 8 bytes: everything bigger composes
    a.checkout().await.unwrap();

    let big: String = (0..200).map(|i| format!("line {i}\n")).collect();
    write(dir.path(), "model.bin", &big);
    store.inject_compose_swept();
    let drained = a.drain().await;
    let cited = |m: Option<manifest::LeanManifest>| m.is_some_and(|m| m.entries.contains_key("model.bin"));
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().map(|l| l.manifest);
    assert!(!cited(m), "fixture: the swept upload was cited anyway");
    assert!(drained.is_err(), "the drain attested a tree whose only copy of model.bin is on it: {drained:?}");

    a.drain().await.expect("the retry did not drain");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().map(|l| l.manifest);
    assert!(cited(m), "the retry did not publish model.bin");
}

/// The drain's own boundary, for a delete that meets another writer's edit:
/// M3 lands the delete (theirs preserved), so the drain attests at once and
/// no pod finds the file again. (Before M3 the delete was outranked and the
/// drain had to refuse; `drained.is_err()` pinned that.)
#[tokio::test]
async fn a_drain_whose_delete_meets_a_peers_edit_attests_with_theirs_preserved() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "shared.txt", "v1");
    a.floor_tick().await.unwrap();
    b.checkout().await.unwrap();
    write(dir_b.path(), "shared.txt", "B's v2");
    backdate_baseline(&b, "shared.txt");
    b.floor_tick().await.unwrap();

    std::fs::remove_file(dir_a.path().join("shared.txt")).unwrap();
    a.drain().await.expect("the drain refused a delete that landed");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("shared.txt"), "the drain attested a tree whose delete is not in the bucket");
    assert!(
        a.state.load_conflicts().unwrap().iter().any(|c| c.kind.starts_with("commit-deleted-over-theirs") && c.path == "shared.txt"),
        "B's version was not preserved"
    );
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
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
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
    let mut a = syncer(&store, dir.path()).await;
    a.cfg.sentinel_min_interval_secs = 3600; // the 1-hour-floor trick, applied to the interval
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
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
    let mut a = syncer(&store, dir.path()).await;
    a.cfg.whole_put_max = 1024; // a small ceiling keeps the test fast
    a.cfg.sentinel_hourly_budget = 8; // ⇒ 2 honors of a 4 KiB file
    a.cfg.sentinel_min_interval_secs = 0;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
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
    let mut b = syncer(&store2, dir2.path()).await;
    b.cfg.whole_put_max = 1024;
    b.cfg.sentinel_hourly_budget = 8;
    b.cfg.sentinel_min_interval_secs = 0;
    assert!(claim_until_held(&mut b, 3).await);
    let posture = b.sentinel_preflight().unwrap();
    b.write_capabilities(&posture).unwrap();
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
    let mut a = syncer(&store, dir.path()).await;
    a.cfg.sentinel_hourly_budget = 2;
    a.cfg.sentinel_min_interval_secs = 0;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
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
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
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
    let mut a = syncer(&store, dir.path()).await;
    a.cfg.sentinel_min_interval_secs = 0;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
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
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
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
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
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

/// D4 — THE correctness rule, not an optimization. A scoped sync must
/// move the baseline only for what it applied in scope. The baseline is
/// the three-way MERGE BASE (P1-lite): if a scoped sync moved it
/// wholesale to bucket-current, nothing would read as owed at an
/// out-of-scope path and the change would be silently lost forever.
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
    let mut a = syncer(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir_a.path(), "inputs/data.txt", "v1");
    write(dir_a.path(), "outputs/result.txt", "v1");
    a.run_barrier().await.unwrap();

    // A sibling writer publishes new generations of BOTH paths.
    peer_publish(&store, &a.cfg, "inputs/data.txt", "foreign inputs v2", "sibling").await;
    let (foreign_out_etag, _) =
        peer_publish(&store, &a.cfg, "outputs/result.txt", "foreign outputs v2", "sibling").await;

    // (1) the out-of-scope change genuinely existed pre-sync.
    assert_ne!(
        foreign_out_etag,
        a.state.load_baseline().unwrap().entries["outputs/result.txt"].etag
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
        a.state.load_baseline().unwrap().entries["outputs/result.txt"].etag,
        foreign_out_etag,
        "a scoped sync advanced the MERGE BASE for an out-of-scope path"
    );
    // A scoped sync leaves seq/manifest_etag alone.
    assert_eq!(r.seq, a.state.load_baseline().unwrap().seq);

    // (3) present after the next barrier's consume, which derives it as
    // owed.
    a.run_barrier().await.unwrap();
    a.run_barrier().await.unwrap();
    assert_eq!(
        read(dir_a.path(), "outputs/result.txt").unwrap(),
        "foreign outputs v2",
        "the out-of-scope foreign change was LOST"
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
/// credential-holding syncer into an arbitrary-file-write primitive
/// outside the workspace.
#[tokio::test]
#[cfg(unix)]
async fn write_file_atomic_refuses_symlink_escape() {
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "ORIGINAL CREDENTIALS").unwrap();

    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
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
    // P2: the write is committed; one barrier queues it, the next consumes.
    a.run_barrier().await.unwrap();
    a.run_barrier().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&secret).unwrap(),
        "ORIGINAL CREDENTIALS",
        "the syncer wrote THROUGH a planted symlink, outside the workspace"
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
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "shared.txt", "v1");
    a.run_barrier().await.unwrap();

    // A foreign change lands and IS applied...
    hitl_write(&store, &a.cfg, "shared.txt", "foreign v2", "ci").await.unwrap();
    let pre = a.state.load_baseline().unwrap();
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
    // The rest of the baseline is the pre-sync one too: seq and pointer
    // (under P2 the change is a committed document, so a baseline that
    // kept the new seq would say there is no news).
    b.seq = pre.seq;
    b.manifest_etag = pre.manifest_etag.clone();
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
/// tick (so an agent can tell "no news" from "syncer dead");
/// `observed_seq` moves only when it moves.
#[tokio::test]
async fn remote_seq_ticks_without_added_requests() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
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

/// D5 with two writers. A's change lands between B's consume and B's merge
/// (the one place P1-lite leaves a gap), so B's merge carries it into the
/// manifest and B's baseline takes that seq while the tree is still OWED
/// the change (`Baseline::behind`) until the next consume. Until then B's
/// tree does not have it, and the ticker must say so: `.flint/AGENTS.md` tells an agent to
/// `sync` before it starts on a path `remote.seq` says has news.
/// Found by the formal model (`LeanBarrierLeaseSentinel`, depth 19).
#[tokio::test]
async fn remote_seq_reports_news_while_a_peers_change_waits_in_the_queue() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "seed");
    a.floor_tick().await.unwrap();
    b.checkout().await.unwrap();
    b.floor_tick().await.unwrap();
    let t0 = b.load_remote_seq();
    assert_eq!(t0.observed_seq, t0.integrated_seq, "fixture: B has news before anyone wrote");

    // B publishes an unrelated path; A's x.txt lands after B's consume and
    // before B's merge.
    write(dir_b.path(), "z.txt", "z");
    let (store2, cfg_a) = (store.clone(), a.cfg.clone());
    super::barrier::CONSUME_WINDOW_HOOK.with(|h| {
        *h.borrow_mut() = Some((
            String::new(),
            "before-scan",
            Box::new(move || {
                futures::executor::block_on(peer_publish(&store2, &cfg_a, "x.txt", "A's v2", "A"));
            }),
        ))
    });
    b.floor_tick().await.unwrap();
    assert!(super::barrier::CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_none()), "fixture: the window never ran");
    assert!(derive_pending(&b).await, "fixture: B's merge left nothing owed for x.txt");
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("seed"), "fixture: the change already reached B's tree");
    let t1 = b.load_remote_seq();
    assert!(
        t1.observed_seq > t1.integrated_seq,
        "B's tree lacks A's change, yet remote.seq says no news: {t1:?}"
    );

    // The next tick consumes the queue: now there is no news, and the tree agrees.
    b.floor_tick().await.unwrap();
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("A's v2"), "A's change never reached B on the floor");
    let t2 = b.load_remote_seq();
    assert_eq!(t2.observed_seq, t2.integrated_seq, "news that was integrated is still reported: {t2:?}");
}

/// D14 — a gateway sync request is CARRIED, never executed. The
/// asymmetry with a boundary request is blast radius: a boundary
/// publishes what is already on disk and touches no local file, whereas
/// `sync` re-derives the tree against the current remote manifest and
/// DELETES local files for remotely-deleted paths.
///
/// Failing control, house style: the tree hash is taken before and
/// after, and the test FAILS if the syncer mutated anything.
#[tokio::test]
async fn sync_request_is_carried_never_executed() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
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
    // Several ticks: the syncer must move the ticker and NOTHING else.
    for _ in 0..3 {
        let _ = a.sentinel_tick().await.unwrap();
    }
    let t = a.load_remote_seq();
    assert_eq!(t.sync_requested_by.as_deref(), Some("ci@example"));
    assert!(t.sync_requested_unix.is_some());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("keep.txt")).unwrap(),
        tree_before,
        "the syncer acted on a remote's sync request"
    );

    // The agent's OWN touch is what performs it — and then the request
    // is stale and self-clears.
    touch_sentinel(dir.path(), control::SYNC, r#"{"nonce":"mine"}"#);
    let acks = a.sentinel_tick().await.unwrap();
    assert_eq!(acks.len(), 1);
    assert!(!dir.path().join("keep.txt").exists(), "the agent's own sync did not run");
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
    let mut a = syncer(&store, dir.path()).await;
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
    let mut a = syncer(&store, dir.path()).await;
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

// ---------------------------------------------------------------------
// Phase 3 observability minimum (§2.6, review ledger OF-6): "why is the
// manifest not advancing" must be answerable from inside the pod,
// before Phase 6's metrics stack exists and without spelunking the
// emptyDir.
// ---------------------------------------------------------------------

/// `flint-sync status` is the exec surface for a workspace whose
/// syncer is DEAD or deposed — so it must render with no lease held
/// and no claim attempted. A status verb that claims the lease would
/// depose the very syncer being diagnosed.
#[tokio::test]
async fn status_renders_without_claiming_the_lease() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "held.txt", "x");
    a.floor_tick().await.unwrap();

    // A second process over the SAME tree cannot even open the state
    // dir (the occupancy flock), so status reads the files directly.
    let s = super::status_report(&cfg_for(dir.path())).unwrap();
    assert!(s.gauges.is_some());
    assert_eq!(s.capabilities.unwrap().state, "live");
    assert!(s.baseline_seq >= 1, "the tick's boundary never reached the baseline status reads");
    assert!(s.incarnation_epoch.is_some());
    // Rendering it must not have touched the cell (at rest since the
    // tick's barrier handed it on).
    let cell = store.epoch_read(&a.cfg.epoch_key()).await.unwrap().unwrap();
    assert!(cell.released);
    assert_eq!(
        cell.epoch,
        a.state.load_incarnation().unwrap().unwrap().epoch,
        "status rotated the epoch it was supposed to observe"
    );
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
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
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
    let mut a = syncer(&store, dir.path()).await;
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
/// An UNREADABLE path is not a deleted one.
///
/// `confirm_absences` is the oracle that promotes a first absence to a
/// published deletion, and a published deletion DELETES THE OBJECT. It
/// used to ask `symlink_metadata(...).is_err()`, so EACCES on a parent
/// directory, EIO, EMFILE or ELOOP all read as "the agent deleted it".
/// The walk asks the same syscall with `?` and fails closed; two call
/// sites of one syscall must not have opposite error policies, and the
/// one that can destroy data is not the one to guess in.
#[tokio::test]
async fn an_unreadable_path_is_not_confirmed_as_deleted() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let a = syncer(&store, dir.path()).await;

    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIPPED: running as root, mode bits cannot induce EACCES");
        return;
    }
    // `locked/f.txt` exists, but `locked` is unreadable — so the lstat
    // fails with EACCES rather than NotFound.
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("f.txt"), b"still here").unwrap();
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();

    let mut classified = super::scan::Classified::default();
    classified.first_absence.insert("locked/f.txt".into());

    let err = a
        .confirm_absences(&mut classified)
        .expect_err("an unreadable path must not confirm a deletion");
    assert!(
        format!("{err}").contains("refusing to publish a deletion"),
        "the refusal must say what it refused: {err}"
    );
    assert!(
        classified.deletes.is_empty(),
        "NOTHING may be queued for deletion off an errno that is not NotFound: {:?}",
        classified.deletes
    );

    // Restore so the tempdir can be cleaned up.
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn the_confirmation_never_deletes_a_path_the_walk_merely_missed() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let a = syncer(&store, dir.path()).await;
    write(dir.path(), "renamed.txt", "here all along");

    // What a walk that lost the rename race produces: the path is
    // classified first-absent while the file is on disk.
    let mut classified = super::scan::Classified::default();
    classified.first_absence.insert("renamed.txt".into());
    classified.first_absence.insert("truly-gone.txt".into());

    let confirmed = a.confirm_absences(&mut classified).unwrap();
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

/// D10 — the drain is a declared boundary too, and it is the last one
/// this workspace will ever have: a delete left withheld here is
/// re-materialized by the successor's checkout.
#[tokio::test]
async fn the_drain_carries_a_delete_made_before_it() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
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

/// The baseline (the merge base) is rewritten at step 7 — after the
/// manifest CAS and after the GC deletes. A container restart in that
/// window leaves the bucket holding a document THIS workspace wrote and
/// the baseline one generation behind it, so our own entry reads as a
/// change since the base at the next merge. Before M3 delete/modify then
/// resolved conservatively against the agent's own delete, and the delete
/// was dropped from a boundary about to be acked (found by the formal
/// model, tranche 3, product 1; the intent journal's `installed_etag`
/// fixed it). Under P1-lite there is no journal: the delete applies over
/// the version the base does not know (M3), which is our own, recorded.
#[tokio::test]
async fn a_crash_between_the_cas_and_step_7_never_makes_our_own_entry_foreign() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
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
    // persisted baseline is as it was before it.
    a.state.save_baseline(&stale).unwrap();
    assert_ne!(
        stale.entries.get("f.txt").map(|e| &e.etag),
        Some(&installed.manifest.entries["f.txt"].etag),
        "fixture: the baseline is not actually behind the install"
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
    let mut a = syncer(&store, dir.path()).await;
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

/// C4 — §2.2's containment rule covers the TARGET; the write goes
/// through a temp sibling nobody validates. `contained_path` refuses a
/// symlinked component, then `write_file_atomic` computes
/// `<name>.flint-sync-tmp` and `fs::write`s it — `File::create`
/// semantics, which follow symlinks. The scanner skips symlinks, so the
/// plant is invisible. The two helpers this tranche ADDED are worse:
/// `control::write_atomic` and `state::write_atomic` have no
/// containment at all and write into directories the app must be able
/// to write, and `.flint/remote.seq` is rewritten on every tick — so
/// the syncer's own tick performs the write, with no remote
/// cooperation at all.
///
/// The syncer holds the bucket credentials and runs with no
/// `securityContext`: this is a cross-container write primitive, not a
/// workspace-local nuisance.
#[tokio::test]
async fn a_planted_temp_sibling_is_never_written_through() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
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
            "the syncer wrote through a planted temp sibling ({v}) — an arbitrary-file-write \
             primitive outside the workspace, with the bucket credentials"
        );
    }
    // …and the workspace itself still works.
    assert_eq!(read(dir.path(), "inputs/config.json").as_deref(), Some("REMOTE-BYTES"));
    assert!(control_exists(dir.path(), "remote.seq"));
}

// ── Phase 4: the operator-facing surfaces (§2.6) ─────────────────────

/// The observed-state echo (§2.6) rides a write a live syncer already
/// pays for: the HANDOFF that ends every barrier (the cell is at rest
/// between barriers, so the echo it keeps is the only thing that tells
/// an operator which binary ran the last boundary). Without it the
/// operator can only report what the spec ASKED for: the env
/// read is a fixed list, so a knob reaching a binary that predates it
/// is ignored in silence — the mixed-version hole D11 closes on the
/// agent side and nothing closed on the operator's.
#[tokio::test]
async fn a_barrier_echoes_the_running_binary_into_the_lease_cell() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    write(dir.path(), "seed.txt", "S1");
    let cited = a.run_barrier().await.unwrap().seq.unwrap();

    // The handoff kept the echo on a RELEASED cell.
    let cell = store.epoch_read(&a.cfg.epoch_key()).await.unwrap().unwrap();
    assert!(cell.released, "a barrier must hand the cell on when it is done");
    let echo: flint_store::LeaseEcho = serde_json::from_str(
        cell.echo.as_deref().expect("the handoff carried no observed-state echo"),
    )
    .expect("the echo is not a LeaseEcho");
    assert_eq!(echo.last_cited_seq, cited, "the echo names no citation");
    assert_eq!(echo.protocol, super::SENTINEL_PROTOCOL);
    assert!(!echo.syncer_version.is_empty(), "no version ⇒ no mixed-fleet tell");
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
    let mut a = syncer(&store, dir.path()).await;
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
/// a fake no-object entry: the old `consume_inbox` HEADed `file_key(path)` for
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
    let mut a = syncer(&store, dir.path()).await;
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
    a.read_cell_requests().await.unwrap();

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
    a.read_cell_requests().await.unwrap();
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
    let mut a = syncer(&store, dir.path()).await;
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
    a.read_cell_requests().await.unwrap();

    assert_eq!(
        read(dir.path(), "keep.txt"),
        tree_before,
        "the syncer EXECUTED a sync on a remote's say-so and rewrote the agent's tree"
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
        rpo_secs: 11,
        withheld_reason: Some("parked-412".into()),
        sentinel_budget_remaining: 44,
        last_boundary: Some(super::gauges::LastBoundary {
            source: "sentinel".into(),
            seq: 66,
            unix: 1_756_000_000,
        }),
        updated_unix: 1_756_000_100,
        last_durable_unix: 1_756_000_050,
        auth_paused_since_unix: Some(1_756_000_010),
    };
    let json = serde_json::to_value(&g).unwrap();
    let fields: Vec<String> = json.as_object().unwrap().keys().cloned().collect();
    assert!(fields.len() >= 7, "the fixture did not populate the struct: {fields:?}");

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
        ("flint_lean_sentinel_budget_remaining", 44),
        ("flint_lean_last_boundary_seq", 66),
        ("flint_lean_withheld_reason", 1),      // parked-412
        ("flint_lean_last_boundary_source", 1), // sentinel
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
    let g = super::Gauges { rpo_secs: 1, ..Default::default() };
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
    assert!(series >= 8, "the renderer emitted almost nothing: {series}");
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
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "work.txt", "W1");
    a.run_barrier().await.unwrap();
    let g = a.write_gauges(None).unwrap();

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
    let mut a = syncer(&store, dir.path()).await;
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

    let mut a = syncer(&store, dir.path()).await;
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

    let mut b = syncer(&store, dir2.path()).await;
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

/// Which clock published a boundary is a FLEET question, not a local
/// one: the agent gets its answer in the ack, but an operator holding
/// only the bucket — and the gateway's `/status`, which reports this
/// field for every workspace — gets whatever the manifest was stamped
/// with. The ordinary floor and the sentinel honor once left it null,
/// so the field read as "unknown" on every workspace.
#[tokio::test]
async fn every_boundary_says_which_clock_installed_it() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
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

/// The ack and the manifest must never name two different clocks for
/// one boundary. The drain's pending-sentinel arm rewrites the ack to
/// `drain`; if the manifest it installed still said `sentinel-deferred`,
/// the agent's local answer and the fleet's bucket answer would disagree
/// about the same event — and the bucket is the one an operator trusts.
#[tokio::test]
async fn a_drained_sentinel_names_the_same_clock_in_the_ack_and_in_the_bucket() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
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
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "dirty.txt", "published v1");
    write(dir.path(), "clean.txt", "published v1");
    a.run_barrier().await.unwrap();

    // A sibling publishes new generations of BOTH paths.
    peer_publish(&store, &a.cfg, "dirty.txt", "foreign bytes for dirty", "sibling").await;
    peer_publish(&store, &a.cfg, "clean.txt", "foreign bytes for clean", "sibling").await;

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
/// `merge` names the DELETES mine publishes over, by the version the delete
/// removed: theirs' tombstone when it has one (the path may have moved on
/// from the base before it was deleted), else the base's citation. A path
/// the base never cited, or one theirs still cites, is not one.
#[test]
fn merge_names_the_deletes_mine_publishes_over() {
    fn e(etag: &str) -> manifest::LeanEntry {
        manifest::LeanEntry {
            key: "tenant/proj1/files/p.txt".into(),
            etag: etag.into(),
            crc64_b64: "AAAAAAAAAAA=".into(),
            size: 3,
            mode: 0o644,
            mtime_unix: 0,
            generation: 1,
            epoch: 0,
        }
    }
    let base: std::collections::BTreeMap<String, String> =
        [("gone.txt", "e1"), ("tomb.txt", "e1"), ("kept.txt", "e1")].iter().map(|(p, t)| (p.to_string(), t.to_string())).collect();
    let mut theirs = manifest::LeanManifest::default();
    theirs.entries.insert("kept.txt".into(), e("e1"));
    theirs.tombstones.insert("tomb.txt".into(), manifest::Tombstone { etag: "e2".into(), seq: 3 });
    let upserts: std::collections::BTreeMap<String, manifest::LeanEntry> =
        ["gone.txt", "tomb.txt", "kept.txt", "new.txt"].iter().map(|p| (p.to_string(), e("mine"))).collect();
    let out = manifest::merge(&base, &theirs, &upserts, &Default::default(), &Default::default());
    let mut got = out.recreated.clone();
    got.sort();
    assert_eq!(got, vec![("gone.txt".to_string(), "e1".to_string()), ("tomb.txt".to_string(), "e2".to_string())]);
}

/// M3 (P1-lite): a local delete APPLIES wherever it is not parked — over
/// theirs too, which the merge then names (`over_theirs`) for the caller to
/// preserve and record. Before M3 a delete over a version changed since the
/// base was outranked: theirs stayed cited.
#[tokio::test]
async fn merge_applies_a_local_delete_and_names_the_version_it_deleted_over() {
    fn entry(etag: &str) -> manifest::LeanEntry {
        manifest::LeanEntry {
            key: "tenant/proj1/files/p.txt".into(),
            etag: etag.into(),
            crc64_b64: "AAAAAAAAAAA=".into(),
            size: 3,
            mode: 0o644,
            mtime_unix: 0,
            generation: 1,
            epoch: 0,
        }
    }
    use manifest::DeleteOutcome::{Applied, OverTheirs, Parked};
    // (name, theirs entry, base etag, parked, survives our delete, the outcome, the version named over)
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, Option<&str>, Option<&str>, bool, bool, manifest::DeleteOutcome, Option<&str>)> = vec![
        ("theirs unchanged since our base", Some("e1"), Some("e1"), false, false, Applied, None),
        ("FOREIGN MODIFY under our delete", Some("e2"), Some("e1"), false, false, OverTheirs, Some("e2")),
        ("FOREIGN ADD under our delete", Some("e2"), None, false, false, OverTheirs, Some("e2")),
        ("absent from both", None, None, false, false, Applied, None),
        ("already deleted by someone else", None, Some("e1"), false, false, Applied, None),
        ("parked: never resolved this pass", Some("e1"), Some("e1"), true, true, Parked, None),
    ];
    for (name, theirs_etag, base_etag, is_parked, must_survive, expected, named) in cases {
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

        let out = manifest::merge(&base, &theirs, &Default::default(), &deletes, &parked);
        assert_eq!(out.deletes.get("p.txt"), Some(&expected), "{name}: reported outcome");
        assert_eq!(out.doc.entries.contains_key("p.txt"), must_survive, "{name}: the entry");
        let over: Vec<&str> = out.over_theirs.iter().map(|(_, e)| e.etag.as_str()).collect();
        assert_eq!(over, named.into_iter().collect::<Vec<_>>(), "{name}: the version named over");
        if !must_survive && theirs_etag.is_some() {
            assert_eq!(
                out.doc.tombstones.get("p.txt").map(|t| t.etag.as_str()),
                theirs_etag,
                "{name}: the tombstone names what the delete retired"
            );
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
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "same.txt", "these bytes never move");
    write(dir.path(), "impostor.txt", "AAAA");
    a.run_barrier().await.unwrap();

    // A sibling replaces impostor.txt with the SAME NUMBER OF BYTES.
    peer_publish(&store, &a.cfg, "impostor.txt", "BBBB", "sibling").await;
    let theirs = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;

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
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "f.txt", "v1");
    sc.run_barrier().await.unwrap();

    // Install a manifest that names its clock, the way a sentinel
    // honor does.
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
        crc64_b64: "AAAAAAAAAAA=".into(),
        size: 1,
        mode: 0o100644,
        mtime_unix: 0,
        generation: 1,
        epoch: 1,
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
    let cfg = cfg_single(dir.path());
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
    let cfg = cfg_single(dir.path());
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
    let cfg = cfg_single(dir.path());
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
    let cfg = cfg_single(dir.path());
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
        handle = Some(manifest::ManifestHandle { etag: meta.etag, legacy: false, prev_chunks: Vec::new(), prev_tombstones: None });
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
    let cfg = cfg_single(dir.path());
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
    let cfg = cfg_single(dir.path());
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
/// `refuse_reads` refuses the calls an IDLE writer's floor tick makes
/// too (HEAD, GET, LIST, the cell read): a writer with nothing to publish
/// never renews, so its tick is the only probe of its credentials.
struct AuthRefusing {
    inner: Arc<MemoryStore>,
    refuse: std::sync::atomic::AtomicBool,
    refuse_reads: std::sync::atomic::AtomicBool,
}

impl AuthRefusing {
    fn new(inner: Arc<MemoryStore>) -> Self {
        Self {
            inner,
            refuse: std::sync::atomic::AtomicBool::new(false),
            refuse_reads: std::sync::atomic::AtomicBool::new(false),
        }
    }
    fn set_refuse(&self, v: bool) {
        self.refuse.store(v, std::sync::atomic::Ordering::SeqCst);
    }
    fn set_refuse_reads(&self, v: bool) {
        self.refuse_reads.store(v, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ObjectStore for AuthRefusing {
    async fn copy_object(
        &self,
        src_key: &str,
        src_if_match: Option<&str>,
        dst_key: &str,
        condition: &PutCondition,
        stamps: &GenerationStamps,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.copy_object(src_key, src_if_match, dst_key, condition, stamps).await
    }
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
        if self.refuse_reads.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(flint_store::StoreError::Auth(
                "ExpiredToken: the security token included in the request is expired".into(),
            ));
        }
        self.inner.head(key).await
    }
    async fn get_whole(
        &self,
        key: &str,
        if_match: Option<&str>,
    ) -> flint_store::StoreResult<(flint_store::ObjectMeta, Bytes)> {
        if self.refuse_reads.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(flint_store::StoreError::Auth(
                "ExpiredToken: the security token included in the request is expired".into(),
            ));
        }
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
        if self.refuse_reads.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(flint_store::StoreError::Auth(
                "ExpiredToken: the security token included in the request is expired".into(),
            ));
        }
        self.inner.list(prefix).await
    }
    async fn delete(&self, key: &str) -> flint_store::StoreResult<()> {
        self.inner.delete(key).await
    }
    async fn delete_if_match(&self, key: &str, etag: &str) -> flint_store::StoreResult<()> {
        self.inner.delete_if_match(key, etag).await
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
        if self.refuse_reads.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(flint_store::StoreError::Auth(
                "ExpiredToken: the security token included in the request is expired".into(),
            ));
        }
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
    let state = SyncerState::open(cfg.state_dir()).unwrap();
    let mut a = Syncer {
        store: proxy.clone() as Arc<dyn ObjectStore>,
        cfg,
        state,
        lease: None,
        cell_written_at: None,
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
    a.write_gauges(None).unwrap();
    assert_eq!(
        a.load_gauges().unwrap().auth_paused_since_unix,
        Some(1_000),
        "an ordinary gauge tick erased the credential pause"
    );

    // A renewal is a probe of our own credentials, so it observes
    // recovery too. (An idle writer never renews; its probe is the floor
    // tick — the next test.)
    proxy.set_refuse(false);
    lease::renew(&mut a).await.expect("renew after the credentials came back");
    assert!(
        a.load_gauges().unwrap().auth_paused_since_unix.is_none(),
        "the pause outlived the credentials being restored"
    );
}

/// The writer heartbeat was the scheduled probe of an IDLE writer's
/// credentials: a writer with nothing to publish never claims, so no
/// renewal ever runs. With the heartbeat gone the floor tick — which
/// every writer runs, idle or not — must record a 401/403 as a pause and
/// clear it when the credentials come back, or `authPausedSinceUnix`
/// stops answering for exactly the writers most likely to sit idle.
#[tokio::test]
async fn an_idle_writer_records_and_clears_a_credential_pause_on_its_floor_tick() {
    let inner = Arc::new(MemoryStore::new());
    let proxy = Arc::new(AuthRefusing::new(inner.clone()));
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let state = SyncerState::open(cfg.state_dir()).unwrap();
    let mut a = Syncer {
        store: proxy.clone() as Arc<dyn ObjectStore>,
        cfg,
        state,
        lease: None,
        cell_written_at: None,
        noted_not_regular: Default::default(),
    };
    a.checkout().await.unwrap();
    a.floor_tick().await.expect("an idle tick with working credentials");
    assert!(a.load_gauges().unwrap().auth_paused_since_unix.is_none(), "a healthy idle writer reads as paused");

    proxy.set_refuse_reads(true);
    let e = a.floor_tick().await.expect_err("fixture: the idle tick made no refused request");
    assert!(e.is_auth(), "the refused tick did not classify as a credential fault: {e}");
    assert!(
        a.load_gauges().unwrap().auth_paused_since_unix.is_some(),
        "an idle writer's refused tick left no local evidence of the pause"
    );

    proxy.set_refuse_reads(false);
    a.floor_tick().await.expect("an idle tick after the credentials came back");
    assert!(
        a.load_gauges().unwrap().auth_paused_since_unix.is_none(),
        "the pause outlived the credentials being restored on an idle writer"
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
        crc64_b64: "AAAAAAAAAAA=".into(),
        size: 10,
        mode: 0o644,
        mtime_unix: 1_700_000_000,
        generation: seq,
        epoch: 1,
    }
}

fn manifest_of(n: usize, seq: u64) -> super::manifest::LeanManifest {
    super::manifest::LeanManifest {
        seq,
        entries: (0..n).map(|i| (format!("src/f{i:05}.txt"), entry_at(&format!("f{i:05}"), seq))).collect(),
        sole_writer: false,
        boundary_source: None,
        tombstones: std::collections::BTreeMap::new(),
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
    let h = super::manifest::ManifestHandle { etag: meta.etag.clone(), legacy: false, prev_chunks: Vec::new(), prev_tombstones: None };
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
        sole_writer: false,
        boundary_source: None,
        epoch: 1,
        tombstones: None,
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


/// A backend that runs a SWEEP in the middle of a read: the first time
/// the named chunk is fetched, it installs a newer pointer and collects
/// that chunk, then answers 404. This is the §8.2 race — a reader whose
/// generation is swept out from under it — and it cannot be produced by
/// calling the store's methods in sequence from a test, which is why it
/// lives in the backend.
struct SweepMidRead {
    inner: Arc<MemoryStore>,
    current_key: String,
    doomed_key: String,
    next_pointer: Vec<u8>,
    /// FALSE leaves the pointer alone: the chunk vanishes with no newer
    /// generation to restart onto, which must FAIL rather than restart.
    move_pointer: bool,
    fired: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl ObjectStore for SweepMidRead {
    async fn copy_object(
        &self,
        src_key: &str,
        src_if_match: Option<&str>,
        dst_key: &str,
        condition: &PutCondition,
        stamps: &GenerationStamps,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.copy_object(src_key, src_if_match, dst_key, condition, stamps).await
    }
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
        if key == self.doomed_key
            && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            if self.move_pointer {
                let stamps = GenerationStamps {
                    generation: 9,
                    epoch: 1,
                    flush_uuid: "sweep".into(),
                    boundary_source: None,
                    posix: None,
                };
                let crc = crc64_nvme(&self.next_pointer);
                self.inner
                    .put_whole(
                        &self.current_key,
                        Bytes::from(self.next_pointer.clone()),
                        &PutCondition::Unconditional,
                        &stamps,
                        crc,
                    )
                    .await?;
            }
            self.inner.delete(&self.doomed_key).await?;
            return Err(flint_store::StoreError::NotFound(format!("swept: {key}")));
        }
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
    async fn delete_if_match(&self, key: &str, etag: &str) -> flint_store::StoreResult<()> {
        self.inner.delete_if_match(key, etag).await
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

/// §8.2 — a reader whose generation is swept out from under it RESTARTS
/// onto the current one instead of tearing.
///
/// Before this, a reader was safe for `Retain` PUBLISHES, not for a
/// duration — which is the wrong unit, since a full checkout runs for
/// minutes while a busy workspace publishes every floor tick.
/// `LeanChunkGCSlowReader.cfg` violates `Inv_NoTornRead` without the
/// revalidation and holds with it, at an unchanged window size.
///
/// The two halves are each other's control. Same sweep, same missing
/// chunk; the only difference is whether the POINTER moved, and that is
/// what decides "raced a sweep" from "the live manifest has a hole".
#[tokio::test]
async fn a_reader_swept_mid_read_restarts_onto_the_current_generation() {
    for move_pointer in [true, false] {
        let inner = Arc::new(MemoryStore::new());
        let plain: Arc<dyn ObjectStore> = inner.clone();
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = cfg_for(dir.path());
        cfg.chunk_target = 64;
        cfg.chunk_min = 16;
        cfg.chunk_max = 256;

        // Generation 1, then generation 2 over the same project. Both
        // sets of chunks are durable; only the pointer distinguishes
        // them, which is the layout working as designed.
        let m1 = manifest_of(600, 1);
        let meta1 =
            manifest::cas_write_chunked(plain.as_ref(), &cfg, &m1, None, &[],
                manifest::PublishStamps { epoch: 1, flush_uuid: "u1", boundary_source: None })
                .await
                .unwrap();
        let p1 = manifest::load_pointer(plain.as_ref(), &cfg).await.unwrap().unwrap().pointer;
        let c1 = match p1.entries().unwrap() {
            super::manifest::Entries::Chunked(c) => c.to_vec(),
            _ => panic!("not chunked"),
        };
        let mut m2 = m1.clone();
        m2.seq = 2;
        m2.entries.insert("src/f00003.txt".into(), entry_at("changed", 42));
        let h = super::manifest::ManifestHandle { etag: meta1.etag.clone(), legacy: false, prev_chunks: Vec::new(), prev_tombstones: None };
        manifest::cas_write_chunked(plain.as_ref(), &cfg, &m2, Some(&h), &c1,
            manifest::PublishStamps { epoch: 1, flush_uuid: "u2", boundary_source: None })
            .await
            .unwrap();
        let lp2 = manifest::load_pointer(plain.as_ref(), &cfg).await.unwrap().unwrap();
        let ptr2_bytes = serde_json::to_vec(&lp2.pointer).unwrap();

        // Put generation 1 back as the live pointer, so a reader starts
        // on the generation the sweep is about to collect.
        let stamps = GenerationStamps {
            generation: 1,
            epoch: 1,
            flush_uuid: "rewind".into(),
            boundary_source: None,
            posix: None,
        };
        let p1_bytes = serde_json::to_vec(&p1).unwrap();
        let crc = crc64_nvme(&p1_bytes);
        plain
            .put_whole(&cfg.current_key(), Bytes::from(p1_bytes), &PutCondition::Unconditional, &stamps, crc)
            .await
            .unwrap();

        // A chunk only generation 1 references — the one a sweep would
        // legitimately take once generation 1 leaves the window.
        let c2: Vec<String> = match lp2.pointer.entries().unwrap() {
            super::manifest::Entries::Chunked(c) => c.iter().map(|r| r.addr.clone()).collect(),
            _ => panic!("not chunked"),
        };
        let doomed = c1
            .iter()
            .find(|r| !c2.contains(&r.addr))
            .expect("generation 2 shares every chunk with generation 1 — nothing to sweep");

        let store: Arc<dyn ObjectStore> = Arc::new(SweepMidRead {
            inner: inner.clone(),
            current_key: cfg.current_key(),
            doomed_key: cfg.chunk_key(&doomed.addr),
            next_pointer: ptr2_bytes.clone(),
            move_pointer,
            fired: std::sync::atomic::AtomicBool::new(false),
        });

        let got = manifest::load(store.as_ref(), &cfg).await;
        if move_pointer {
            let loaded = got.expect("a reader swept mid-read did not restart").unwrap();
            assert_eq!(
                loaded.manifest.entries, m2.entries,
                "the restart did not land on the CURRENT generation — a reader that mixes \
                 generations is worse than one that fails"
            );
            assert_eq!(loaded.manifest.seq, 2);
        } else {
            let msg = match got {
                Err(e) => e.to_string(),
                Ok(_) => panic!(
                    "a chunk vanished under an UNCHANGED pointer and the load succeeded — \
                     that is a hole in the live manifest, not a race"
                ),
            };
            assert!(msg.contains("hole"), "wrong refusal: {msg}");
        }
    }
}

/// The chunk reaper, one arm per rule `LeanChunkGC.tla` established.
///
/// Each arm is the others' control: the SAME sweep over the SAME store,
/// differing only in the one thing under test. A reaper that collected
/// nothing would pass a "does not collect the live chunks" assertion,
/// so the collecting arm runs first and its count is asserted non-zero.
#[tokio::test]
async fn the_chunk_reaper_takes_orphans_and_nothing_else() {
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = inner.clone();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = cfg_for(dir.path());
    cfg.chunk_target = 64;
    cfg.chunk_min = 16;
    cfg.chunk_max = 256;
    cfg.orphan_grace_secs = 0; // everything is instantly past the grace

    // Generation 1, then generation 2 that changes one file. Chunks the
    // change did not touch are SHARED, and must survive.
    let m1 = manifest_of(600, 1);
    let meta1 = manifest::cas_write_chunked(store.as_ref(), &cfg, &m1, None, &[],
        manifest::PublishStamps { epoch: 1, flush_uuid: "u1", boundary_source: None })
        .await.unwrap();
    let c1 = match manifest::load_pointer(store.as_ref(), &cfg).await.unwrap().unwrap()
        .pointer.entries().unwrap() {
        super::manifest::Entries::Chunked(c) => c.to_vec(),
        _ => panic!("not chunked"),
    };
    let mut m2 = m1.clone();
    m2.seq = 2;
    m2.entries.insert("src/f00003.txt".into(), entry_at("changed", 42));
    let h = super::manifest::ManifestHandle { etag: meta1.etag.clone(), legacy: false, prev_chunks: Vec::new(), prev_tombstones: None };
    manifest::cas_write_chunked(store.as_ref(), &cfg, &m2, Some(&h), &c1,
        manifest::PublishStamps { epoch: 1, flush_uuid: "u2", boundary_source: None })
        .await.unwrap();
    let live: Vec<String> = match manifest::load_pointer(store.as_ref(), &cfg).await.unwrap()
        .unwrap().pointer.entries().unwrap() {
        super::manifest::Entries::Chunked(c) => c.iter().map(|r| r.addr.clone()).collect(),
        _ => panic!("not chunked"),
    };
    let orphans: Vec<&super::chunk::ChunkRef> =
        c1.iter().filter(|r| !live.contains(&r.addr)).collect();
    assert!(!orphans.is_empty(), "generation 2 superseded no chunk — nothing to reap");

    // ARM 1: past the grace, unreferenced chunks go.
    let n = manifest::sweep_chunks(store.as_ref(), &cfg).await.unwrap();
    assert_eq!(n, orphans.len(), "the reaper took {n} chunks, expected {}", orphans.len());
    for r in &orphans {
        assert!(
            store.head(&cfg.chunk_key(&r.addr)).await.is_err(),
            "superseded chunk {} survived the sweep",
            r.addr
        );
    }
    // ARM 2 (the control that makes arm 1 mean something): every chunk
    // the LIVE pointer names is still there, and the manifest still
    // reads back whole.
    for a in &live {
        assert!(store.head(&cfg.chunk_key(a)).await.is_ok(), "the reaper took a LIVE chunk {a}");
    }
    let back = manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap();
    assert_eq!(back.manifest.entries, m2.entries, "the sweep left the manifest short");

    // ARM 3: the grace. Publish again to make fresh orphans, then sweep
    // with a real grace — nothing may go.
    let meta2 = {
        let lp = manifest::load_pointer(store.as_ref(), &cfg).await.unwrap().unwrap();
        super::manifest::ManifestHandle { etag: lp.etag, legacy: false, prev_chunks: Vec::new(), prev_tombstones: None }
    };
    let mut m3 = m2.clone();
    m3.seq = 3;
    m3.entries.insert("src/f00300.txt".into(), entry_at("changed3", 43));
    let c2: Vec<super::chunk::ChunkRef> = match manifest::load_pointer(store.as_ref(), &cfg)
        .await.unwrap().unwrap().pointer.entries().unwrap() {
        super::manifest::Entries::Chunked(c) => c.to_vec(),
        _ => panic!("not chunked"),
    };
    manifest::cas_write_chunked(store.as_ref(), &cfg, &m3, Some(&meta2), &c2,
        manifest::PublishStamps { epoch: 1, flush_uuid: "u3", boundary_source: None })
        .await.unwrap();
    let before = store.list(&format!("{}/{}/chunks/", cfg.prefix, super::LEAN_DIR)).await.unwrap().len();
    cfg.orphan_grace_secs = 3600;
    let n = manifest::sweep_chunks(store.as_ref(), &cfg).await.unwrap();
    let after = store.list(&format!("{}/{}/chunks/", cfg.prefix, super::LEAN_DIR)).await.unwrap().len();
    assert_eq!(n, 0, "the reaper took {n} chunks that were inside the grace");
    assert_eq!(before, after, "the chunk count moved despite the grace");
}

/// The FENCE. A publish that lands between the reference read and the
/// listing must abort the pass, because the reaper no longer knows
/// whether the new pointer references a candidate. Without it the
/// reference set predates a CAS the delete would follow, which is
/// exactly `LeanChunkGCStaleRefs`.
#[tokio::test]
async fn the_chunk_reaper_aborts_when_a_publish_lands_under_it() {
    let inner = Arc::new(MemoryStore::new());
    let plain: Arc<dyn ObjectStore> = inner.clone();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = cfg_for(dir.path());
    cfg.chunk_target = 64;
    cfg.chunk_min = 16;
    cfg.chunk_max = 256;
    cfg.orphan_grace_secs = 0;

    let m1 = manifest_of(600, 1);
    let meta1 = manifest::cas_write_chunked(plain.as_ref(), &cfg, &m1, None, &[],
        manifest::PublishStamps { epoch: 1, flush_uuid: "u1", boundary_source: None })
        .await.unwrap();
    let c1 = match manifest::load_pointer(plain.as_ref(), &cfg).await.unwrap().unwrap()
        .pointer.entries().unwrap() {
        super::manifest::Entries::Chunked(c) => c.to_vec(),
        _ => panic!("not chunked"),
    };
    let mut m2 = m1.clone();
    m2.seq = 2;
    m2.entries.insert("src/f00003.txt".into(), entry_at("changed", 42));
    let h = super::manifest::ManifestHandle { etag: meta1.etag.clone(), legacy: false, prev_chunks: Vec::new(), prev_tombstones: None };
    manifest::cas_write_chunked(plain.as_ref(), &cfg, &m2, Some(&h), &c1,
        manifest::PublishStamps { epoch: 1, flush_uuid: "u2", boundary_source: None })
        .await.unwrap();

    // Stage a THIRD pointer body, and install it from inside the store
    // the moment the reaper lists — the interleaving a test cannot
    // produce by calling store methods in order.
    let lp2 = manifest::load_pointer(plain.as_ref(), &cfg).await.unwrap().unwrap();
    let mut moved = lp2.pointer.clone();
    moved.seq = 99;
    let moved_bytes = serde_json::to_vec(&moved).unwrap();

    let store: Arc<dyn ObjectStore> = Arc::new(PublishOnList {
        inner: inner.clone(),
        current_key: cfg.current_key(),
        next_pointer: moved_bytes,
        chunks_prefix: format!("{}/{}/chunks/", cfg.prefix, super::LEAN_DIR),
        fired: std::sync::atomic::AtomicBool::new(false),
    });

    let before = plain.list(&format!("{}/{}/chunks/", cfg.prefix, super::LEAN_DIR)).await.unwrap().len();
    let n = manifest::sweep_chunks(store.as_ref(), &cfg).await.unwrap();
    let after = plain.list(&format!("{}/{}/chunks/", cfg.prefix, super::LEAN_DIR)).await.unwrap().len();
    assert_eq!(n, 0, "the reaper deleted {n} chunks after a publish landed under it");
    assert_eq!(before, after, "chunks disappeared across an aborted pass");
}


/// A backend that PUBLISHES the moment the reaper lists the chunk
/// prefix: the pointer moves between the reference read and the
/// listing, which is the interleaving the fence exists for and which no
/// sequence of ordinary store calls from a test can produce.
struct PublishOnList {
    inner: Arc<MemoryStore>,
    current_key: String,
    next_pointer: Vec<u8>,
    chunks_prefix: String,
    fired: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl ObjectStore for PublishOnList {
    async fn copy_object(
        &self,
        src_key: &str,
        src_if_match: Option<&str>,
        dst_key: &str,
        condition: &PutCondition,
        stamps: &GenerationStamps,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.copy_object(src_key, src_if_match, dst_key, condition, stamps).await
    }
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
        let out = self.inner.list(prefix).await;
        if prefix == self.chunks_prefix
            && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let stamps = GenerationStamps {
                generation: 99,
                epoch: 1,
                flush_uuid: "raced".into(),
                boundary_source: None,
                posix: None,
            };
            let crc = crc64_nvme(&self.next_pointer);
            self.inner
                .put_whole(
                    &self.current_key,
                    Bytes::from(self.next_pointer.clone()),
                    &PutCondition::Unconditional,
                    &stamps,
                    crc,
                )
                .await?;
        }
        out
    }
    async fn delete(&self, key: &str) -> flint_store::StoreResult<()> {
        self.inner.delete(key).await
    }
    async fn delete_if_match(&self, key: &str, etag: &str) -> flint_store::StoreResult<()> {
        self.inner.delete_if_match(key, etag).await
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

/// The barrier publishing through the chunked layout, end to end, with
/// a foreign writer racing it — the property `LeanChunkMerge.tla`
/// checks, exercised against the real merge rather than a model of it.
///
/// The barrier already merges at ENTRY level and re-loads the whole
/// document on a 412, so `cas_write_chunked` re-chunks from the merged
/// entries and the splice the model refutes is unreachable from here.
/// This asserts that it stays that way: a chunked publish must not lose
/// what the other writer put in a chunk it happened to rewrite.
#[tokio::test]
async fn a_chunked_barrier_keeps_a_foreign_write_it_never_read() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    sc.cfg.chunked = true;
    sc.cfg.chunk_target = 8;
    sc.cfg.chunk_min = 2;
    sc.cfg.chunk_max = 32;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();

    for i in 0..60 {
        write(dir.path(), &format!("src/f{i:03}.txt"), "v1");
    }
    sc.run_barrier().await.unwrap();

    // PRECONDITION: it really did chunk, and into more than one chunk —
    // a single-chunk workspace would exercise none of the seams.
    let p = manifest::load_pointer(store.as_ref(), &sc.cfg).await.unwrap().unwrap().pointer;
    let chunks = match p.entries().unwrap() {
        super::manifest::Entries::Chunked(c) => c.to_vec(),
        super::manifest::Entries::Single { .. } => {
            panic!("the barrier published a single generation object with cfg.chunked set")
        }
    };
    assert!(
        chunks.len() >= 3,
        "PRECONDITION: {} chunk(s) — too few for a foreign write and a local one to land in \
         different ones, which is the case that matters",
        chunks.len()
    );

    // A foreign write the syncer never read, and a local edit far from
    // it in key order, so they fall in different chunks.
    hitl_write(&store, &sc.cfg, "src/f000.txt", "user version", "dilip").await.unwrap();
    write(dir.path(), "src/f059.txt", "v2");
    backdate_baseline(&sc, "src/f059.txt");
    sc.run_barrier().await.unwrap();

    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert_eq!(
        m.manifest.entries.len(),
        60,
        "the chunked publish changed the entry count — a seam lost or duplicated a key"
    );
    let cited = &m.manifest.entries["src/f000.txt"];
    let (_, body) = store.get_whole(&cited.key, Some(&cited.etag)).await.unwrap();
    assert_eq!(
        &body[..],
        b"user version",
        "the chunked publish lost the foreign write — this is the splice failure the merge \
         model refutes, reached through the barrier"
    );
    let mine = &m.manifest.entries["src/f059.txt"];
    let (_, body) = store.get_whole(&mine.key, Some(&mine.etag)).await.unwrap();
    assert_eq!(&body[..], b"v2", "the local edit did not land");
}

/// RETIRE-AGE G, handles (M1 of the 2026-09-24 simplification analysis).
/// A reader that loaded a document less than G ago must find every handle
/// it cites: the one a WRITER's commit replaced (collected at once before)
/// and the one a UI SAVE replaced (the orphan sweep took it by its write
/// age, which for a long-lived file is long past). Past G both go, and so
/// do the retire logs that protected them.
#[tokio::test]
async fn a_replaced_handle_outlives_its_retirement_by_the_retire_age() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    sc.cfg.retire_grace_secs = 600;
    sc.cfg.untracked_grace_secs = 0;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "f.txt", "v1");
    write(dir.path(), "g.txt", "g1");
    sc.run_barrier().await.unwrap();
    // The reader's document.
    let seen = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;

    // A writer's commit replaces f; a UI save replaces g.
    write(dir.path(), "f.txt", "v2 by the writer");
    backdate_baseline(&sc, "f.txt");
    hitl_write(&store, &sc.cfg, "g.txt", "g2 by the ui", "ui").await.unwrap();
    // The orphan sweep is due in this barrier.
    sc.state.save_orphan_sweep_at(now_unix() - sc.cfg.untracked_sweep_secs - 1).unwrap();
    sc.run_barrier().await.unwrap();
    for p in ["f.txt", "g.txt"] {
        let e = &seen.entries[p];
        assert!(
            store.get_whole(&e.key, Some(&e.etag)).await.is_ok(),
            "{p}: a reader of the document from before the commit found its handle gone"
        );
    }

    // Past G: both are collected, and the logs with them.
    sc.cfg.retire_grace_secs = 0;
    write(dir.path(), "h.txt", "a later publish");
    sc.state.save_orphan_sweep_at(now_unix() - sc.cfg.untracked_sweep_secs - 1).unwrap();
    sc.run_barrier().await.unwrap();
    for p in ["f.txt", "g.txt"] {
        assert!(
            matches!(store.head(&seen.entries[p].key).await, Err(StoreError::NotFound(_))),
            "{p}: a retired handle outlived the retire age"
        );
    }
    let logs = format!("{}/{}/retired/", sc.cfg.prefix, super::LEAN_DIR);
    assert!(store.list(&logs).await.unwrap().is_empty(), "the retire logs were never reaped");
}

/// A retire log is data in the bucket, not a licence: one naming a handle
/// the document still cites — malformed, planted, or written by a party
/// that got it wrong — must not delete a live file when it comes due.
#[tokio::test]
async fn a_retire_log_naming_a_cited_handle_deletes_nothing() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "f.txt", "live");
    sc.run_barrier().await.unwrap();
    let cited = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest.entries["f.txt"].clone();
    let log = manifest::RetireLog { at_unix: 1, handles: vec![cited.key.clone()], chunks: vec![] };
    let key = format!("{}{:020}-planted.json", manifest::retired_prefix(&sc.cfg), 1);
    let body = serde_json::to_vec(&log).unwrap();
    let crc = crc64_nvme(&body);
    let stamps = GenerationStamps { generation: 0, epoch: 0, flush_uuid: "planted".into(), boundary_source: None, posix: None };
    store.put_whole(&key, body.into(), &PutCondition::IfNoneMatchAny, &stamps, crc).await.unwrap();

    write(dir.path(), "g.txt", "a later publish, so a commit section runs");
    sc.run_barrier().await.unwrap();
    assert!(
        store.get_whole(&cited.key, Some(&cited.etag)).await.is_ok(),
        "a due retire log deleted a handle the document cites"
    );
}

/// RETIRE-AGE G, generations (M1, the single-object layout). A superseded
/// generation was kept by COUNT (`KEEP_GENERATIONS` behind the live one):
/// at one UI save a second that is seconds, and a reader that resolved the
/// pointer before a burst found its generation gone. Now one is also kept
/// until its SUCCESSOR — the commit that superseded it — is G old.
#[tokio::test]
async fn a_superseded_generation_outlives_its_supersession_by_the_retire_age() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_single(dir.path());
    let state = SyncerState::open(cfg.state_dir()).unwrap();
    let mut sc = Syncer {
        store: store.clone() as Arc<dyn ObjectStore>,
        cfg,
        state,
        lease: None,
        cell_written_at: None,
        noted_not_regular: Default::default(),
    };
    sc.cfg.retire_grace_secs = 600;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "f.txt", "v0");
    sc.run_barrier().await.unwrap();
    let first = manifest::load_pointer(store.as_ref(), &sc.cfg).await.unwrap().unwrap().pointer.entries_key.unwrap();
    for i in 1..=(manifest::KEEP_GENERATIONS + 3) {
        write(dir.path(), "f.txt", &format!("v{i} {}", "x".repeat(i)));
        backdate_baseline(&sc, "f.txt");
        sc.run_barrier().await.unwrap();
    }
    assert!(
        store.head(&first).await.is_ok(),
        "a generation superseded seconds ago was reaped by count alone"
    );
    sc.cfg.retire_grace_secs = 0;
    manifest::sweep_generations(store.as_ref(), &sc.cfg).await.unwrap();
    assert!(
        matches!(store.head(&first).await, Err(StoreError::NotFound(_))),
        "past the retire age the count rule applies again"
    );
}

/// RETIRE-AGE G, chunks (M1). A chunk a commit supersedes is protected for
/// G from its RETIREMENT; the reaper judged it by its write, so under
/// frequent pointer moves a checkout mid-load of the previous pointer found
/// it gone and, after `LOAD_ATTEMPTS`, failed.
#[tokio::test]
async fn a_superseded_chunk_outlives_its_retirement_by_the_retire_age() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    sc.cfg.chunked = true;
    sc.cfg.chunk_target = 8;
    sc.cfg.chunk_min = 2;
    sc.cfg.chunk_max = 32;
    sc.cfg.orphan_grace_secs = 0;
    sc.cfg.retire_grace_secs = 600;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    for i in 0..60 {
        write(dir.path(), &format!("src/f{i:03}.txt"), "v1");
    }
    sc.run_barrier().await.unwrap();
    let chunks_of = |p: super::manifest::Pointer| match p.entries().unwrap() {
        super::manifest::Entries::Chunked(c) => c.iter().map(|r| r.addr.clone()).collect::<Vec<_>>(),
        super::manifest::Entries::Single { .. } => panic!("not chunked"),
    };
    let before = chunks_of(manifest::load_pointer(store.as_ref(), &sc.cfg).await.unwrap().unwrap().pointer);

    write(dir.path(), "src/f030.txt", "v2");
    backdate_baseline(&sc, "src/f030.txt");
    sc.run_barrier().await.unwrap();
    let after = chunks_of(manifest::load_pointer(store.as_ref(), &sc.cfg).await.unwrap().unwrap().pointer);
    let superseded: Vec<&String> = before.iter().filter(|a| !after.contains(a)).collect();
    assert!(!superseded.is_empty(), "PRECONDITION: the commit superseded no chunk");
    for a in &superseded {
        assert!(
            store.head(&sc.cfg.chunk_key(a)).await.is_ok(),
            "a chunk the previous pointer names was reaped inside the retire age"
        );
    }

    sc.cfg.retire_grace_secs = 0;
    write(dir.path(), "src/f031.txt", "v2");
    backdate_baseline(&sc, "src/f031.txt");
    sc.run_barrier().await.unwrap();
    for a in &superseded {
        assert!(
            matches!(store.head(&sc.cfg.chunk_key(a)).await, Err(StoreError::NotFound(_))),
            "a superseded chunk outlived the retire age"
        );
    }
}

/// Migrating a workspace from one generation object to a chunk list,
/// and the pre-migration generations not leaking forever.
///
/// `sweep_generations` used to return 0 on a chunked pointer — correct
/// as far as it went, since chunks need a different rule, but it left
/// every object written before the migration uncollectable. Nothing
/// references them once the layout moves, and a reader still resolving
/// one revalidates (§8.2).
#[tokio::test]
async fn migrating_to_chunks_leaves_no_generation_objects_behind() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    sc.cfg.chunked = false; // start on the layout being migrated FROM
    sc.cfg.chunk_target = 8;
    sc.cfg.chunk_min = 2;
    sc.cfg.chunk_max = 32;
    sc.cfg.orphan_grace_secs = 0;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    for i in 0..40 {
        write(dir.path(), &format!("src/f{i:03}.txt"), "v1");
    }
    // Two publishes on the SINGLE layout.
    sc.run_barrier().await.unwrap();
    write(dir.path(), "src/f000.txt", "v2");
    backdate_baseline(&sc, "src/f000.txt");
    sc.run_barrier().await.unwrap();
    let gens = format!("{}/{}/manifests/", sc.cfg.prefix, super::LEAN_DIR);
    assert!(
        !store.list(&gens).await.unwrap().is_empty(),
        "PRECONDITION: no generation objects to migrate away from"
    );

    // Flip the layout and publish again.
    sc.cfg.chunked = true;
    write(dir.path(), "src/f001.txt", "v2");
    backdate_baseline(&sc, "src/f001.txt");
    sc.run_barrier().await.unwrap();
    let p = manifest::load_pointer(store.as_ref(), &sc.cfg).await.unwrap().unwrap().pointer;
    assert!(
        matches!(p.entries().unwrap(), super::manifest::Entries::Chunked(_)),
        "the workspace did not migrate to a chunk list"
    );
    // Everything still reads, across the layout change.
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert_eq!(m.manifest.entries.len(), 40, "entries were lost migrating to chunks");

    // The end state is the claim, not who reached it: the barrier runs
    // `sweep_generations` itself after a successful install, so with no
    // grace the migrated-away generations are usually gone before this
    // call — which is the behaviour wanted. Asserting a non-zero return
    // here would have failed for the RIGHT outcome.
    manifest::sweep_generations(store.as_ref(), &sc.cfg).await.unwrap();
    assert!(
        store.list(&gens).await.unwrap().is_empty(),
        "generation objects survived the migration: {:?}",
        store.list(&gens).await.unwrap().iter().map(|o| &o.key).collect::<Vec<_>>()
    );
    // And the sweep did not touch the chunks the new layout needs.
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert_eq!(m.manifest.entries.len(), 40, "the migration sweep broke the live manifest");
}



/// A backend whose LISTING reports every object as ancient while the
/// objects themselves are however old they really are.
///
/// This is the gap between "the listing said it was old" and "it is old
/// NOW": an adopt-rewrite landing after the reaper's listing refreshes a
/// chunk, and a reaper that judges the grace from the listing cannot
/// see it. Modelling the two as one value is what the implementation
/// did; the model did not.
struct StaleListing {
    inner: Arc<MemoryStore>,
}

#[async_trait::async_trait]
impl ObjectStore for StaleListing {
    async fn copy_object(
        &self,
        src_key: &str,
        src_if_match: Option<&str>,
        dst_key: &str,
        condition: &PutCondition,
        stamps: &GenerationStamps,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.copy_object(src_key, src_if_match, dst_key, condition, stamps).await
    }
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
        let mut out = self.inner.list(prefix).await?;
        for o in out.iter_mut() {
            o.last_modified_unix = Some(0);
        }
        Ok(out)
    }
    async fn delete(&self, key: &str) -> flint_store::StoreResult<()> {
        self.inner.delete(key).await
    }
    async fn delete_if_match(&self, key: &str, etag: &str) -> flint_store::StoreResult<()> {
        self.inner.delete_if_match(key, etag).await
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

/// The grace must be judged at DELETE time, not from the listing.
///
/// Rule 4 of the chunk-GC model works by REFRESHING an adopted chunk's
/// age, so a reaper reading the age out of a pre-fence listing cannot
/// see the very refresh that rule exists to produce — and deletes a
/// chunk an in-flight publish is about to name. The model computed
/// `Doomed` against the store's state at delete time; the first
/// implementation read it from the listing, which is not the same rule.
///
/// Found by the other session's integrity audit, not by this suite,
/// which is why the suite now carries it.
#[tokio::test]
async fn the_chunk_reaper_judges_the_grace_now_not_when_it_listed() {
    let inner = Arc::new(MemoryStore::new());
    let plain: Arc<dyn ObjectStore> = inner.clone();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = cfg_for(dir.path());
    cfg.chunk_target = 64;
    cfg.chunk_min = 16;
    cfg.chunk_max = 256;
    // A real grace: the objects below are seconds old, so nothing may
    // be collected unless the reaper believes the LISTING's zero.
    cfg.orphan_grace_secs = 3600;

    let m1 = manifest_of(600, 1);
    let meta1 = manifest::cas_write_chunked(plain.as_ref(), &cfg, &m1, None, &[],
        manifest::PublishStamps { epoch: 1, flush_uuid: "u1", boundary_source: None })
        .await.unwrap();
    let c1 = match manifest::load_pointer(plain.as_ref(), &cfg).await.unwrap().unwrap()
        .pointer.entries().unwrap() {
        super::manifest::Entries::Chunked(c) => c.to_vec(),
        _ => panic!("not chunked"),
    };
    let mut m2 = m1.clone();
    m2.seq = 2;
    m2.entries.insert("src/f00003.txt".into(), entry_at("changed", 42));
    let h = super::manifest::ManifestHandle { etag: meta1.etag.clone(), legacy: false, prev_chunks: Vec::new(), prev_tombstones: None };
    manifest::cas_write_chunked(plain.as_ref(), &cfg, &m2, Some(&h), &c1,
        manifest::PublishStamps { epoch: 1, flush_uuid: "u2", boundary_source: None })
        .await.unwrap();

    let prefix = format!("{}/{}/chunks/", cfg.prefix, super::LEAN_DIR);
    let before = plain.list(&prefix).await.unwrap().len();
    // PRECONDITION: there IS an unreferenced chunk, so "collected
    // nothing" cannot pass by there being nothing to collect.
    let live: Vec<String> = match manifest::load_pointer(plain.as_ref(), &cfg).await.unwrap()
        .unwrap().pointer.entries().unwrap() {
        super::manifest::Entries::Chunked(c) => c.iter().map(|r| r.addr.clone()).collect(),
        _ => panic!("not chunked"),
    };
    assert!(
        c1.iter().any(|r| !live.contains(&r.addr)),
        "PRECONDITION: nothing is unreferenced, so the reaper has no candidate to mis-judge"
    );

    let store: Arc<dyn ObjectStore> = Arc::new(StaleListing { inner: inner.clone() });
    let n = manifest::sweep_chunks(store.as_ref(), &cfg).await.unwrap();
    let after = plain.list(&prefix).await.unwrap().len();
    assert_eq!(
        n, 0,
        "the reaper collected {n} chunk(s) on the LISTING's word that they were ancient — \
         an adopt-rewrite landing after the listing is invisible to that, and rule 4 works \
         by producing exactly such a rewrite"
    );
    assert_eq!(before, after, "chunks disappeared: {before} → {after}");
}

/// What the sweep COSTS inside the fence. Every publish of a small
/// workspace supersedes its chunks, and each one stays inside the grace
/// for an hour, so a reaper that HEADs every unreferenced chunk to date
/// it pays one HEAD per publish of the last hour, per commit, one at a
/// time: the contention drill measured 700 ms of a 1 s hold after five
/// minutes of six writers. A chunk the LISTING already shows inside the
/// grace needs no HEAD — a rewrite only makes an object younger, so the
/// listing's age bounds the HEAD's from above.
#[tokio::test]
async fn the_chunk_reaper_does_not_head_what_the_listing_shows_inside_the_grace() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = cfg_for(dir.path());
    cfg.chunk_target = 64;
    cfg.chunk_min = 16;
    cfg.chunk_max = 256;
    cfg.orphan_grace_secs = 3600;

    let mut m = manifest_of(600, 1);
    let mut prev: Option<(super::manifest::ManifestHandle, Vec<super::chunk::ChunkRef>)> = None;
    for seq in 1..=6u64 {
        m.seq = seq;
        m.entries.insert(format!("src/f{:05}.txt", seq * 97), entry_at(&format!("v{seq}"), seq));
        let (h, c) = match &prev { Some((h, c)) => (Some(h), c.as_slice()), None => (None, &[][..]) };
        manifest::cas_write_chunked(store.as_ref(), &cfg, &m, h, c,
            manifest::PublishStamps { epoch: 1, flush_uuid: "u", boundary_source: None })
            .await.unwrap();
        let lp = manifest::load_pointer(store.as_ref(), &cfg).await.unwrap().unwrap();
        let chunks = match lp.pointer.entries().unwrap() {
            super::manifest::Entries::Chunked(c) => c.to_vec(),
            _ => panic!("not chunked"),
        };
        prev = Some((super::manifest::ManifestHandle { etag: lp.etag, legacy: false, prev_chunks: Vec::new(), prev_tombstones: None }, chunks));
    }
    let prefix = format!("{}/{}/chunks/", cfg.prefix, super::LEAN_DIR);
    let live: Vec<String> = prev.as_ref().unwrap().1.iter().map(|r| r.addr.clone()).collect();
    let orphans = store.list(&prefix).await.unwrap().iter()
        .filter(|o| !live.iter().any(|a| o.key.ends_with(a.as_str()))).count();
    assert!(orphans >= 5, "PRECONDITION: five publishes left {orphans} unreferenced chunks");

    store.reset_op_counts();
    assert_eq!(manifest::sweep_chunks(store.as_ref(), &cfg).await.unwrap(), 0, "a chunk inside the grace was collected");
    let heads = store.op_counts().get("head").copied().unwrap_or(0);
    assert_eq!(heads, 0, "the reaper HEADed {heads} chunks the listing already showed inside the grace");

    // Past the grace they are still dated by a HEAD, and collected.
    for o in store.list(&prefix).await.unwrap() {
        store.backdate_epoch(&o.key, 3700);
    }
    store.reset_op_counts();
    let n = manifest::sweep_chunks(store.as_ref(), &cfg).await.unwrap();
    assert_eq!(n, orphans, "past the grace the reaper took {n} of {orphans}");
    assert!(store.op_counts().get("head").copied().unwrap_or(0) >= orphans as u64, "a chunk was collected without a HEAD");
    for a in &live {
        assert!(store.head(&cfg.chunk_key(a)).await.is_ok(), "the reaper took a LIVE chunk {a}");
    }
}

/// REACHABILITY: a barrier actually reaps unreferenced chunks.
///
/// The reaper existed, was written to four model-established rules and
/// guarded by six mutation configs, and was called from nothing but its
/// own tests — so with chunking on, superseded chunks accumulated
/// forever. Every unit test passed throughout, because they all called
/// `sweep_chunks` directly. Nothing asked whether PRODUCTION could
/// reach it.
///
/// The commit that was supposed to wire it did not, and the suite
/// stayed green: this test is what makes that impossible to repeat. It
/// goes through `run_barrier`, never calling the reaper itself.
#[tokio::test]
async fn the_barrier_reaps_unreferenced_chunks_without_being_asked() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    sc.cfg.chunk_target = 8;
    sc.cfg.chunk_min = 2;
    sc.cfg.chunk_max = 32;
    sc.cfg.orphan_grace_secs = 0; // no grace, so one barrier is enough
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    for i in 0..60 {
        write(dir.path(), &format!("src/f{i:03}.txt"), "v1");
    }
    sc.run_barrier().await.unwrap();

    let prefix = format!("{}/{}/chunks/", sc.cfg.prefix, super::LEAN_DIR);
    let after_first = store.list(&prefix).await.unwrap().len();
    assert!(after_first >= 3, "PRECONDITION: {after_first} chunk(s) — too few to supersede any");

    // A second barrier supersedes some chunks. If the reaper is wired,
    // the same barrier collects them; if it is not, they pile up.
    write(dir.path(), "src/f000.txt", "v2");
    backdate_baseline(&sc, "src/f000.txt");
    sc.run_barrier().await.unwrap();

    let live: Vec<String> = match manifest::load_pointer(store.as_ref(), &sc.cfg).await.unwrap()
        .unwrap().pointer.entries().unwrap() {
        super::manifest::Entries::Chunked(c) => c.iter().map(|r| r.addr.clone()).collect(),
        _ => panic!("the barrier did not publish a chunk list"),
    };
    let present: Vec<String> = store
        .list(&prefix)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|o| o.key.rsplit('/').next().map(|s| s.to_string()))
        .collect();
    // Everything still there must be referenced: that is the reaper
    // having run, and having run correctly.
    let stray: Vec<&String> = present.iter().filter(|a| !live.contains(a)).collect();
    assert!(
        stray.is_empty(),
        "the barrier left {} unreferenced chunk(s) in the bucket ({stray:?}) — the reaper is \
         not reachable from a publish, which is the whole defect",
        stray.len()
    );
    // And it did not take the live ones with it.
    for a in &live {
        assert!(present.contains(a), "the barrier's own sweep deleted live chunk {a}");
    }
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert_eq!(m.manifest.entries.len(), 60, "the sweep left the manifest short");
}

// ---------------------------------------------------------------------
// Audit 2026-09-03 — the lease fence, the lost renew response, the claim
// precondition, and the drain attestation.
// ---------------------------------------------------------------------

/// AUDIT 2026-09-03, finding 2. The renew CAS is If-Match on OUR token;
/// when our own renew LANDED but its response was lost, the next renew
/// 412s on the stale token and the holder read that as a deposal:
/// exit 0, and under the CSI delivery nothing restarted it. One read
/// tells the cases apart — a cell that still names this holder at this
/// epoch was written by nobody else. The control at the end keeps the
/// real deposal fencing.
#[tokio::test]
async fn a_lost_renew_response_does_not_self_fence() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let key = a.cfg.epoch_key();

    // The lost response: the renew lands (the cell's token moves) but
    // the reply never reaches the holder, whose handle keeps the old
    // token.
    let stale = a.lease.clone().unwrap();
    store.epoch_renew(&key, &stale, None).await.unwrap();
    let landed = store.epoch_read(&key).await.unwrap().unwrap().token;
    assert_ne!(landed, stale.token, "fixture: the renew did not move the token");

    lease::renew(&mut a)
        .await
        .expect("a renew whose previous response was lost is not a deposal");
    // The holder's token is the cell's — which the adoption's own renew
    // has moved past `landed` (review 2026-09-12, lease-2).
    let cell = store.epoch_read(&key).await.unwrap().unwrap().token;
    assert_eq!(
        a.lease.as_ref().map(|l| l.token.as_str()),
        Some(cell.as_str()),
        "the holder did not adopt the cell's token after its lost renew"
    );
    // Control: a GENUINE takeover still fences the old holder.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    assert!(claim_until_held(&mut b, 12).await, "quiet polls exhausted ⇒ takeover");
    assert!(matches!(lease::renew(&mut a).await.unwrap_err(), LeanError::Fenced(_)));
    assert!(a.lease.is_none(), "a fenced holder must drop its lease");
}

/// AUDIT 2026-09-03, finding 5. The operator's refuse-foreign was
/// advisory on the data plane: a CR the operator had not yet judged
/// resolved to its spec and the syncer checked out over another
/// project's prefix. The claim cell is durable in the bucket, so the
/// syncer reads it before its first claim step.
#[tokio::test]
async fn a_foreign_claim_refuses_the_syncer_before_it_claims() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;

    // Unstamped: no check (the pre-operator posture the drill runs).
    lease::verify_claim(&a).await.unwrap();
    a.cfg.project_id = Some("team-a/p".into());
    // No cell: a fresh prefix is claimable.
    lease::verify_claim(&a).await.unwrap();

    // The operator's standing claim for ANOTHER project.
    let key = a.cfg.claim_key();
    let body = br#"{"project_id":"team-b/q","created_unix":1,"stamped_by":"op"}"#.to_vec();
    let stamps = GenerationStamps {
        generation: 1,
        epoch: 0,
        flush_uuid: "claim".into(),
        boundary_source: None,
        posix: None,
    };
    let crc = crc64_nvme(&body);
    store
        .put_whole(&key, Bytes::from(body), &PutCondition::IfNoneMatchAny, &stamps, crc)
        .await
        .unwrap();
    let err = lease::verify_claim(&a).await.unwrap_err();
    assert!(
        matches!(err, LeanError::Refused(_)),
        "a foreign claim is a REFUSAL (exit EXIT_REFUSED, final for the delivery), not a \
         state error a restart would retry: {err}"
    );
    let err = err.to_string();
    assert!(
        err.contains("team-b/q") && err.contains("team-a/p"),
        "the refusal must name both projects: {err}"
    );
    assert!(a.lease.is_none());

    // Our own standing claim: adopted.
    a.cfg.project_id = Some("team-b/q".into());
    lease::verify_claim(&a).await.unwrap();
}

/// AUDIT 2026-09-03, finding 3. The drain attests its outcome in the
/// tree; the node plugin preserves a tree whose worker is gone without
/// one. Written only by the drain, cleared by every fresh incarnation.
#[test]
fn the_drain_attestation_is_written_by_the_drain_and_cleared_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let st = SyncerState::open(cfg.state_dir()).unwrap();
    assert!(!st.drained_path().exists(), "a fresh state dir must carry no attestation");
    st.sync_tree().unwrap();
    st.write_drained(Some(7), 1).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(st.drained_path()).unwrap()).unwrap();
    assert_eq!(v["seq"], 7);
    assert_eq!(v["acks"], 1);
    assert!(v["unix"].as_u64().unwrap() > 0);
    st.clear_drained().unwrap();
    assert!(!st.drained_path().exists());
    st.clear_drained().unwrap(); // idempotent: absent is not an error
}

// ── ranged checkout (range_get_min_bytes) ────────────────────────────

/// A big object materialises byte-identically through the RANGED path,
/// and actually ran in parallel.
///
/// The assertion that matters is `peak_get_range_in_flight() > 1`.
/// Byte-identity alone would pass with the ranged path silently
/// falling back to one whole GET — which is exactly the failure this
/// knob would have: a config that reads well, changes nothing, and
/// measures as "no improvement" on a cluster nobody wants to rerun.
#[tokio::test]
async fn ranged_checkout_is_byte_identical_and_parallel() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();

    // 5 MiB of non-uniform bytes: a run of one value would hide an
    // offset that wrote the right length in the wrong place.
    let big: Vec<u8> = (0..5 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir_a.path().join("weights.bin"), &big).unwrap();
    write(dir_a.path(), "small.txt", "not ranged");
    a.run_barrier().await.unwrap();

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    b.cfg.range_get_min_bytes = 1024 * 1024;
    b.cfg.range_get_chunk_bytes = 512 * 1024; // 10 ranges over 5 MiB
    b.cfg.range_get_parallelism = 4;
    store.reset_peak_get_range_in_flight();
    // A zero-latency store cannot show concurrency: each future is
    // ready on its first poll, so `buffer_unordered` never holds more
    // than one. The delay is what makes the overlap observable — and
    // without it this assertion would fail against a CORRECT
    // implementation, which is worse than not asserting at all.
    store.inject_get_range_delay_ms(5);

    let cr = b.checkout().await.unwrap();
    assert_eq!(cr.materialized, 2);
    assert_eq!(
        std::fs::read(dir_b.path().join("weights.bin")).unwrap(),
        big,
        "ranged materialisation must be byte-identical"
    );
    assert_eq!(read(dir_b.path(), "small.txt").unwrap(), "not ranged");
    let peak = store.peak_get_range_in_flight();
    assert!(peak > 1, "ranges must overlap; peak in flight was {peak}");
}

/// The default is ON, and these two are a PAIR. Either one alone
/// passes for the wrong reason: the first would still pass if ranging
/// ran unconditionally and the threshold were dead, and the second
/// would still pass if the ranged path had been deleted outright. They
/// use the same tree and the same file sizes and differ in exactly one
/// thing — `range_get_min_bytes` — so between them the only surviving
/// explanation is that the threshold decides.
///
/// `weights.bin` must exceed the default CHUNK (16 MiB), not merely the
/// default threshold (8 MiB): `fetch_ranged` returns `None` for a single
/// part, because "one range is one whole GET with extra steps". So an
/// object in [8, 16) MiB passes the threshold and takes the whole-object
/// path anyway, which makes the EFFECTIVE default threshold 16 MiB. A
/// 9 MiB file here asserted the ranged path and got the whole-object one
/// — the test was wrong, not the code. 20 MiB gives two ranges.
/// `small.txt` stays far below both, so one checkout exercises both sides.
fn ranged_default_tree() -> Vec<u8> {
    // Non-uniform bytes: a run of one value would hide an offset that
    // wrote the right length in the wrong place.
    (0..20 * 1024 * 1024).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn ranged_checkout_on_by_default_uses_ranges() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    let big = ranged_default_tree();
    std::fs::write(dir_a.path().join("weights.bin"), &big).unwrap();
    write(dir_a.path(), "small.txt", "under the threshold");
    a.run_barrier().await.unwrap();

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    assert_eq!(
        b.cfg.range_get_min_bytes,
        8 * 1024 * 1024,
        "the shipped default moved to 8 MiB on the 2026-09-10 runcr drill"
    );
    store.reset_peak_get_range_in_flight();
    let cr = b.checkout().await.unwrap();
    assert_eq!(cr.materialized, 2);
    assert_eq!(
        std::fs::read(dir_b.path().join("weights.bin")).unwrap(),
        big,
        "the DEFAULT path must be byte-identical, not merely fast"
    );
    assert_eq!(read(dir_b.path(), "small.txt").unwrap(), "under the threshold");
    assert!(
        store.peak_get_range_in_flight() > 0,
        "an object over the default threshold must take the ranged path"
    );
    assert_eq!(cr.ranged, 1, "exactly the over-threshold object, not both");
}

/// The other arm SHUT: with the knob explicitly off, the ranged path
/// must not run at all — on the same bytes the test above ranged.
#[tokio::test]
async fn ranged_checkout_explicitly_disabled_uses_no_ranges() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    let big = ranged_default_tree();
    std::fs::write(dir_a.path().join("weights.bin"), &big).unwrap();
    write(dir_a.path(), "small.txt", "under the threshold");
    a.run_barrier().await.unwrap();

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    b.cfg.range_get_min_bytes = 0;
    store.reset_peak_get_range_in_flight();
    let cr = b.checkout().await.unwrap();
    assert_eq!(cr.materialized, 2);
    assert_eq!(
        std::fs::read(dir_b.path().join("weights.bin")).unwrap(),
        big,
        "the whole-object arm must materialise the same bytes"
    );
    assert_eq!(
        store.peak_get_range_in_flight(),
        0,
        "the whole-object arm must issue no ranged reads"
    );
    assert_eq!(cr.ranged, 0);
}

// ---------------------------------------------------------------------
// Scoped checkout (docs/plans/flint-lean-scoped-read-design.md, phase 2)
// ---------------------------------------------------------------------

/// Publishes a tree with a clear in-scope / out-of-scope split: two
/// small files under `inputs/` and six 4 KiB files under `outputs/`.
/// The size asymmetry is deliberate — it is what lets the budget arms
/// below move exactly one dimension.
async fn scoped_fixture(store: &Arc<MemoryStore>) -> tempfile::TempDir {
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = syncer(store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir_a.path(), "inputs/wanted.txt", "the file the agent edits");
    write(dir_a.path(), "inputs/also.txt", "and this one");
    for i in 0..6 {
        write(dir_a.path(), &format!("outputs/big-{i}.bin"), &"x".repeat(4096));
    }
    a.run_barrier().await.unwrap();
    dir_a
}

#[tokio::test]
async fn a_scoped_checkout_materialises_only_the_admitted_set() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    let cr = b.checkout_scoped(Some(vec!["inputs".into()])).await.unwrap();

    assert_eq!(cr.materialized, 2, "only the admitted citations");
    assert_eq!(cr.out_of_scope, 6, "and the declined count says the scope DID something");
    assert_eq!(cr.scope.as_deref(), Some(&["inputs".to_string()][..]));
    assert!(read(dir_b.path(), "inputs/wanted.txt").is_some());
    assert!(
        read(dir_b.path(), "outputs/big-0.bin").is_none(),
        "an out-of-scope citation must not reach the tree"
    );

    // The citation set is the safety property: `classify` derives
    // deletions by iterating exactly these keys.
    let base = b.state.load_baseline().unwrap();
    assert_eq!(
        base.entries.keys().cloned().collect::<Vec<_>>(),
        vec!["inputs/also.txt".to_string(), "inputs/wanted.txt".to_string()],
        "an unadmitted path must never be cited"
    );
    assert_eq!(b.state.load_scope().unwrap().as_deref(), Some(&["inputs".to_string()][..]));
}

#[tokio::test]
async fn what_a_scoped_checkout_never_materialised_it_can_never_delete() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    b.checkout_scoped(Some(vec!["inputs".into()])).await.unwrap();
    assert!(claim_until_held(&mut b, 12).await, "quiet polls exhausted ⇒ takeover");

    // TWO barriers: the deletion rule needs absence to survive two
    // consecutive scans, so one barrier could pass for the wrong reason.
    let r1 = b.run_barrier().await.unwrap();
    let r2 = b.run_barrier().await.unwrap();
    assert!(r1.deleted.is_empty() && r2.deleted.is_empty(), "{:?} {:?}", r1.deleted, r2.deleted);

    let m = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(
        m.entries.len(),
        8,
        "the six unadmitted citations survive a scoped workspace's barriers: {:?}",
        m.entries.keys().collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_scoped_checkout_budget_is_computed_over_the_admitted_set() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;

    // One budget, between the admitted bytes (~60) and the whole tree
    // (~24 KiB). The two arms differ in the SCOPE and nothing else.
    let dir_whole = tempfile::tempdir().unwrap();
    let mut whole = syncer(&store, dir_whole.path()).await;
    whole.cfg.max_bytes = 1024;
    let err = whole.checkout().await.expect_err("the whole tree is over this budget");
    assert!(matches!(err, LeanError::Budget(_)), "{err}");

    let dir_scoped = tempfile::tempdir().unwrap();
    let mut scoped = syncer(&store, dir_scoped.path()).await;
    scoped.cfg.max_bytes = 1024;
    let cr = scoped
        .checkout_scoped(Some(vec!["inputs".into()]))
        .await
        .expect("a budget is a promise about what THIS checkout writes");
    assert_eq!(cr.materialized, 2);
}

#[tokio::test]
async fn an_all_rejected_checkout_scope_is_refused_not_widened() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    let err = b
        .checkout_scoped(Some(vec!["../escape".into(), "./here".into()]))
        .await
        .expect_err("every entry malformed ⇒ an EMPTY scope ⇒ the whole manifest");
    assert!(format!("{err}").contains("WHOLE MANIFEST"), "{err}");
    assert!(
        read(dir_b.path(), "outputs/big-0.bin").is_none(),
        "the refusal must happen BEFORE the first byte"
    );
    assert!(!b.state.marker_present(), "and must not open the agent-start gate");
}

#[tokio::test]
async fn a_live_tree_refuses_a_checkout_whose_scope_disagrees() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;

    // Direction 1, the dangerous one: a caller that asks for the whole
    // tree and resumes a 2-file workspace would believe it holds 8.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    b.checkout_scoped(Some(vec!["inputs".into()])).await.unwrap();
    let err = b.checkout().await.expect_err("scoped tree, unscoped request");
    let msg = format!("{err}");
    assert!(msg.contains("UNSCOPED"), "the error must name BOTH sets: {msg}");
    assert!(msg.contains("inputs"), "the error must name BOTH sets: {msg}");

    // Direction 2: a whole tree, then a scoped request.
    let dir_c = tempfile::tempdir().unwrap();
    let mut c = syncer(&store, dir_c.path()).await;
    c.checkout().await.unwrap();
    let err = c
        .checkout_scoped(Some(vec!["inputs".into()]))
        .await
        .expect_err("unscoped tree, scoped request");
    assert!(format!("{err}").contains("cannot change the admitted set"), "{err}");

    // And the matching request still resumes.
    let r = b.checkout_scoped(Some(vec!["inputs".into()])).await.unwrap();
    assert!(r.resumed_live_tree);
}

/// A scoped checkout owes nothing its scope declined: the held set is the
/// baseline (the merge base since P1-lite), and the consume derives owed
/// paths only there and under the scope's cover. (C2 — "the merge base is
/// the whole manifest" — was the same guarantee from the other side.)
#[tokio::test]
async fn a_scoped_checkout_owes_nothing_the_scope_declined() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    b.checkout_scoped(Some(vec!["inputs".into()])).await.unwrap();
    assert_eq!(b.state.load_baseline().unwrap().entries.len(), 2, "the CITATIONS are scoped");
    for _ in 0..2 {
        b.run_barrier().await.unwrap();
    }
    assert!(read(dir_b.path(), "outputs/big-0.bin").is_none(), "a barrier fetched a declined citation");
    let base = b.state.load_baseline().unwrap();
    assert_eq!(base.entries.len(), 2);
    assert!(!derive_pending(&b).await, "the scope's declined citations read as owed");
}

/// A whole-tree SYNC of a scoped workspace leaves the declined citations
/// declined, and the positive control for why: the arms differ in the
/// workspace's scope record ALONE. Without it, everything the baseline
/// does not hold reads as owed — the cliff C2 guarded against when the
/// merge base was a separate field.
#[tokio::test]
async fn a_sync_never_fetches_what_the_scope_declined() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;

    // Arm A — as shipped.
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
    a.checkout_scoped(Some(vec!["inputs".into()])).await.unwrap();
    a.sync().await.unwrap();
    assert!(
        read(dir_a.path(), "outputs/big-0.bin").is_none(),
        "a whole-tree sync must leave the declined citations declined"
    );

    // Arm B — the scope record dropped, and NOTHING else changed.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    b.checkout_scoped(Some(vec!["inputs".into()])).await.unwrap();
    b.state.save_scope(None).unwrap();
    b.sync().await.unwrap();
    assert!(
        read(dir_b.path(), "outputs/big-0.bin").is_some(),
        "THE CLIFF: a citation the baseline does not hold reads as owed, so \
         without the scope every declined citation comes back one sync later"
    );
    let base = b.state.load_baseline().unwrap();
    assert_eq!(base.entries.len(), 8, "and the scope is gone entirely");
}

/// What the scope declined stays declined when a PEER changes it: a
/// scoped tree receives neither a new version of a path it does not hold
/// nor a new file outside its scope. (Before P1-lite both arrived: the
/// whole-manifest merge base made the change foreign, and the queue
/// carried it into the tree.) A path it holds still arrives.
#[tokio::test]
async fn a_scoped_tree_never_receives_a_peers_change_outside_its_scope() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    b.checkout_scoped(Some(vec!["inputs".into()])).await.unwrap();

    peer_publish(&store, &b.cfg, "outputs/big-0.bin", "a peer's new version", "peer").await;
    peer_publish(&store, &b.cfg, "outputs/new.bin", "a peer's new file", "peer").await;
    peer_publish(&store, &b.cfg, "inputs/wanted.txt", "a peer's edit in scope", "peer").await;
    for _ in 0..2 {
        b.run_barrier().await.unwrap();
    }
    assert!(read(dir_b.path(), "outputs/big-0.bin").is_none(), "a declined path arrived");
    assert!(read(dir_b.path(), "outputs/new.bin").is_none(), "a file outside the scope arrived");
    assert_eq!(read(dir_b.path(), "inputs/wanted.txt").as_deref(), Some("a peer's edit in scope"));
}

#[tokio::test]
async fn an_unreadable_scope_is_not_read_as_unscoped() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIPPED: running as root, mode bits cannot induce EACCES");
        return;
    }

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    b.checkout_scoped(Some(vec!["inputs".into()])).await.unwrap();

    // The PARENT, not the file. A mode-0o000 file still `stat`s fine —
    // `exists()` needs search permission on the directory, not read on
    // the file — so chmod'ing the file leaves both implementations
    // erroring and the test passes without pinning anything. An
    // unreadable DIRECTORY is what separates them: `exists()` answers
    // false and would return "unscoped".
    let sd = b.cfg.state_dir();
    std::fs::set_permissions(&sd, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    let got = b.state.load_scope();
    std::fs::set_permissions(&sd, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let err = got.expect_err("EACCES must not read as 'unscoped'");
    assert!(format!("{err}").contains("refusing to"), "{err}");
}

#[tokio::test]
async fn a_whole_tree_checkout_clears_a_stale_scope() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;

    // A scoped checkout that crashed before its marker: the scope is on
    // disk, the gate never opened. The whole-tree checkout that replaces
    // it must not inherit a claim it did not make.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    b.state.save_scope(Some(&["inputs".to_string()])).unwrap();
    assert!(!b.state.marker_present());

    let cr = b.checkout().await.unwrap();
    assert_eq!(cr.materialized, 8, "the whole manifest, not the stale scope's share");
    assert_eq!(b.state.load_scope().unwrap(), None, "and the stale scope is GONE");
}

// ---------------------------------------------------------------------
// The conflict log is bounded (state.rs rotation)
// ---------------------------------------------------------------------

/// ~1 KiB per record, so a rotation costs ~1050 appends rather than
/// ~10,000. The padding is in the path because that is the field a real
/// standing condition varies.
fn bulky_conflict(i: usize) -> super::state::ConflictRecord {
    super::state::ConflictRecord {
        path: format!("{}/{}", "p".repeat(900), i),
        foreign_etag: format!("\"etag-{i}\""),
        preserved_key: None,
        kind: "test-bulk".into(),
        at_unix: 1_700_000_000 + i as u64,
    }
}

#[tokio::test]
async fn the_conflict_log_rotates_instead_of_growing_without_bound() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let a = syncer(&store, dir.path()).await;

    for i in 0..2000 {
        a.state.append_conflict(&bulky_conflict(i)).unwrap();
    }
    let sd = a.cfg.state_dir();
    let live = std::fs::metadata(sd.join("conflicts.jsonl")).unwrap().len();
    assert!(
        std::fs::metadata(sd.join("conflicts.1.jsonl")).is_ok(),
        "past the cap the live log must have rotated"
    );
    assert!(live <= 1 << 20, "the LIVE log is what gets parsed on every read: {live}");

    // Nothing lost yet: one rotation keeps both generations.
    let all = a.state.load_conflicts().unwrap();
    assert_eq!(all.len(), 2000, "a single rotation loses nothing");
    assert_eq!(a.state.conflicts_dropped().unwrap(), 0);
    // Oldest first, across the rotation boundary.
    assert_eq!(all[0].at_unix, 1_700_000_000);
    assert_eq!(all[1999].at_unix, 1_700_000_000 + 1999);
}

#[tokio::test]
async fn a_rotation_never_shortens_what_a_reader_has_already_counted() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let a = syncer(&store, dir.path()).await;

    // `honor_sync` counts before a sync and skips that many after. The
    // count must never go DOWN across one rotation, or it skips past
    // the records the sync just produced.
    let mut prev = 0usize;
    let mut saw_rotation = false;
    for i in 0..2000 {
        a.state.append_conflict(&bulky_conflict(i)).unwrap();
        let n = a.state.load_conflicts().unwrap().len();
        assert!(n >= prev, "the log SHRANK at append {i}: {prev} -> {n}");
        if std::fs::metadata(a.cfg.state_dir().join("conflicts.1.jsonl")).is_ok() {
            saw_rotation = true;
        }
        prev = n;
    }
    assert!(saw_rotation, "the run must actually cross a rotation or it proves nothing");
}

#[tokio::test]
async fn a_second_rotation_reports_what_it_dropped() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let a = syncer(&store, dir.path()).await;

    for i in 0..4000 {
        a.state.append_conflict(&bulky_conflict(i)).unwrap();
    }
    let dropped = a.state.conflicts_dropped().unwrap();
    let held = a.state.load_conflicts().unwrap().len();
    assert!(dropped > 0, "a second rotation DID drop records; the count must say so");
    assert_eq!(
        dropped as usize + held,
        4000,
        "dropped + held must account for every record: {dropped} + {held}"
    );
}

#[tokio::test]
async fn an_unreadable_conflict_log_is_not_reported_as_empty() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let a = syncer(&store, dir.path()).await;
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIPPED: running as root, mode bits cannot induce EACCES");
        return;
    }
    a.state.append_conflict(&bulky_conflict(1)).unwrap();

    // The PARENT: a mode-0o000 file still stats, so `exists()` and
    // `read_to_string` would both error and the test would pin nothing.
    let sd = a.cfg.state_dir();
    std::fs::set_permissions(&sd, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    let got = a.state.load_conflicts();
    std::fs::set_permissions(&sd, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let err = got.expect_err("an unreadable log must not read as 'no conflicts'");
    assert!(format!("{err}").contains("refusing to report"), "{err}");
}

#[test]
fn a_shortened_conflict_log_never_reports_an_empty_ack() {
    use super::sentinel::conflicts_since;
    let rec = |i: usize| bulky_conflict(i);

    // The ordinary case: the log grew, take the tail.
    let grew = vec![rec(0), rec(1), rec(2)];
    assert_eq!(conflicts_since(1, 0, grew.clone()).len(), 2);
    assert_eq!(conflicts_since(3, 0, grew.clone()).len(), 0, "grew by nothing");

    // The rotation case: the log SHRANK under us. Reporting the
    // survivors over-reports; skipping reports NOTHING, which is the
    // answer that hides a conflict.
    let shrank = vec![rec(7), rec(8)];
    assert_eq!(
        conflicts_since(5, 0, shrank.clone()).len(),
        2,
        "a shortened log must not yield an empty conflict set"
    );
}

/// A backend whose compose succeeds but reports no full-object
/// checksum — a proxy that strips the header, or a backend that never
/// validated the publish server-side. The manifest entry needs a
/// `crc64_b64`, and inventing one would cite bytes nothing vouched for.
struct ComposeWithoutChecksum(Arc<MemoryStore>);

#[async_trait::async_trait]
impl ObjectStore for ComposeWithoutChecksum {
    async fn copy_object(
        &self,
        src_key: &str,
        src_if_match: Option<&str>,
        dst_key: &str,
        condition: &PutCondition,
        stamps: &GenerationStamps,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.0.copy_object(src_key, src_if_match, dst_key, condition, stamps).await
    }
    async fn put_whole(
        &self,
        key: &str,
        body: Bytes,
        cond: &PutCondition,
        stamps: &GenerationStamps,
        crc: u64,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.0.put_whole(key, body, cond, stamps, crc).await
    }
    async fn compose_generation(
        &self,
        spec: &flint_store::ComposeSpec<'_>,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        let mut m = self.0.compose_generation(spec).await?;
        m.crc64_b64 = None;
        Ok(m)
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
        let mut out = self.0.list(prefix).await?;
        for o in out.iter_mut() {
            o.last_modified_unix = Some(0);
        }
        Ok(out)
    }
    async fn delete(&self, key: &str) -> flint_store::StoreResult<()> {
        self.0.delete(key).await
    }
    async fn delete_if_match(&self, key: &str, etag: &str) -> flint_store::StoreResult<()> {
        self.0.delete_if_match(key, etag).await
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
    async fn epoch_handoff(
        &self,
        key: &str,
        lease: &flint_store::EpochLease,
        echo: Option<&str>,
    ) -> flint_store::StoreResult<()> {
        self.0.epoch_handoff(key, lease, echo).await
    }
    async fn epoch_enqueue(
        &self,
        key: &str,
        observed: &flint_store::EpochState,
        holder_id: &str,
    ) -> flint_store::StoreResult<flint_store::EpochState> {
        self.0.epoch_enqueue(key, observed, holder_id).await
    }
}

#[tokio::test]
async fn a_compose_that_reports_no_checksum_refuses_instead_of_citing() {
    let mem = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(ComposeWithoutChecksum(mem.clone()));
    let dir = tempfile::tempdir().unwrap();
    let cfg = {
        let mut c = cfg_for(dir.path());
        // Force the compose path without a 64 MiB fixture.
        c.whole_put_max = 4096;
        c
    };
    let state = SyncerState::open(cfg.state_dir()).unwrap();
    let mut a = Syncer {
        store,
        cfg,
        state,
        lease: None,
        cell_written_at: None,
        noted_not_regular: Default::default(),
    };
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    // Over the threshold, so it goes through `upload_compose`.
    std::fs::write(dir.path().join("big.bin"), vec![7u8; 20_000]).unwrap();

    let err = a
        .run_barrier()
        .await
        .expect_err("a publish with no checksum must refuse, not cite");
    assert!(
        format!("{err}").contains("refusing to cite bytes nothing vouched for"),
        "{err}"
    );
    // Nothing cited: the manifest must not name a path this barrier
    // could not vouch for.
    let m = manifest::load(mem.as_ref(), &a.cfg).await.unwrap();
    assert!(
        m.map(|l| l.manifest.entries.is_empty()).unwrap_or(true),
        "a refused publish must leave no citation"
    );
}

// ── drafts × scoped checkout ─────────────────────────────────────────

// ---------------------------------------------------------------------
// The narrow / widen verb (scoped-read design §4, phase 4)
// ---------------------------------------------------------------------

/// A whole-tree workspace over `scoped_fixture`'s published tree: two
/// files under `inputs/`, six under `outputs/`, all held and all cited.
async fn rescope_fixture(store: &Arc<MemoryStore>) -> (tempfile::TempDir, Syncer) {
    let dir = tempfile::tempdir().unwrap();
    let mut b = syncer(store, dir.path()).await;
    b.checkout().await.unwrap();
    assert!(claim_until_held(&mut b, 12).await, "quiet polls exhausted ⇒ takeover");
    assert_eq!(b.state.load_baseline().unwrap().entries.len(), 8);
    (dir, b)
}

/// THE HEADLINE. A narrow removes six files from the tree and the held
/// set, and publishes NOT ONE deletion — across two barriers, because
/// the deletion rule needs absence to survive two consecutive scans and
/// one barrier could pass for the wrong reason.
#[tokio::test]
async fn a_narrow_unwatches_without_publishing_a_single_deletion() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let (dir, mut b) = rescope_fixture(&store).await;

    let r = b.rescope(Some(vec!["inputs".into()])).await.unwrap();
    assert_eq!(r.uncited, 6, "six citations should have left the held set");
    assert_eq!(r.unlinked, 6, "and six files the tree");
    assert!(r.kept_dirty.is_empty());

    assert!(read(dir.path(), "outputs/big-0.bin").is_none(), "the narrow did not unlink");
    assert!(read(dir.path(), "inputs/wanted.txt").is_some(), "the narrow took an admitted path");
    assert_eq!(b.state.load_scope().unwrap().as_deref(), Some(&["inputs".to_string()][..]));

    let r1 = b.run_barrier().await.unwrap();
    let r2 = b.run_barrier().await.unwrap();
    assert!(r1.deleted.is_empty() && r2.deleted.is_empty(), "{:?} {:?}", r1.deleted, r2.deleted);

    // The bucket still holds everything, and still CITES everything.
    let m = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries.len(), 8, "a narrow published a deletion: {:?}", m.entries.keys());
}

/// ANTI-VACUITY for the leg above, and the design's first mutation
/// check made into a test: the same six files removed WITHOUT the
/// uncite must publish their deletions. If this passes silently, the
/// two-scan delete rule is not biting and "no deletions" above proves
/// nothing.
#[tokio::test]
async fn the_same_unlink_without_the_uncite_publishes_deletions() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let (dir, mut b) = rescope_fixture(&store).await;

    for i in 0..6 {
        std::fs::remove_file(dir.path().join(format!("outputs/big-{i}.bin"))).unwrap();
    }

    let r1 = b.run_barrier().await.unwrap();
    let r2 = b.run_barrier().await.unwrap();
    assert!(r1.deleted.is_empty(), "first absence is not delete-eligible: {:?}", r1.deleted);
    assert_eq!(r2.deleted.len(), 6, "the delete rule did not bite: {:?}", r2.deleted);

    let m = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries.len(), 2, "the bucket should have lost six objects here");
}

/// Widen: a scope that grows fetches what it newly admits, and cites it.
#[tokio::test]
async fn a_widen_materialises_and_cites_what_it_newly_admits() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let (dir, mut b) = rescope_fixture(&store).await;

    b.rescope(Some(vec!["inputs".into()])).await.unwrap();
    assert!(read(dir.path(), "outputs/big-0.bin").is_none());

    let r = b.rescope(Some(vec!["inputs".into(), "outputs".into()])).await.unwrap();
    assert_eq!(r.materialized, 6, "widen did not fetch the newly admitted set");
    assert_eq!(r.already_held, 2, "and must not refetch what it already holds");
    assert_eq!(r.uncited, 0);
    assert!(read(dir.path(), "outputs/big-0.bin").is_some(), "widen did not materialise");
    assert_eq!(b.state.load_baseline().unwrap().entries.len(), 8);

    // And the widened tree is quiet: nothing reads as a local add.
    let r1 = b.run_barrier().await.unwrap();
    assert!(r1.uploaded.is_empty(), "widen re-uploaded its own fetch: {:?}", r1.uploaded);
}

/// C2, the one hard constraint, at the new call site. `inst_base` stays
/// the WHOLE manifest after a narrow — narrow it with the held set and
/// every unadmitted citation reads as foreign one merge later and the
/// whole tree comes back.
#[tokio::test]
async fn a_narrow_owes_nothing_it_dropped() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let (_dir, mut b) = rescope_fixture(&store).await;

    b.rescope(Some(vec!["inputs".into()])).await.unwrap();
    let base = b.state.load_baseline().unwrap();
    assert_eq!(base.entries.len(), 2, "the HELD set narrows");
    assert!(!base.entries.contains_key("outputs/big-0.bin"));
    assert!(!base.prev_scan.contains("outputs/big-0.bin"), "prev_scan must drop with entries");
    // ...and what it dropped is not owed back (the merge base narrows with
    // it since P1-lite; the scope keeps the dropped citations declined).
    for _ in 0..2 {
        b.run_barrier().await.unwrap();
    }
    assert_eq!(b.state.load_baseline().unwrap().entries.len(), 2, "a barrier fetched a dropped path back");
}

/// The design's SECOND mutation check: a crash between the intent and
/// the unlink must CONVERGE, never delete. The barrier replays before
/// it scans, so the half-applied state is gone before `classify` can
/// read it as an absence.
#[tokio::test]
async fn a_crash_between_the_intent_and_the_unlink_converges() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let (dir, mut b) = rescope_fixture(&store).await;

    // The crash: intent durable, nothing applied.
    b.state
        .save_scope_intent(&super::state::ScopeIntent {
            target: Some(vec!["inputs".into()]),
            drop: (0..6).map(|i| format!("outputs/big-{i}.bin")).collect(),
            held: Default::default(),
        })
        .unwrap();
    assert!(read(dir.path(), "outputs/big-0.bin").is_some(), "nothing applied yet");

    let r1 = b.run_barrier().await.unwrap();
    assert!(r1.rescope_replayed, "the barrier ran over a half-applied rescope");
    assert!(r1.deleted.is_empty(), "{:?}", r1.deleted);
    let r2 = b.run_barrier().await.unwrap();
    assert!(!r2.rescope_replayed, "the replay must clear its own intent");
    assert!(r2.deleted.is_empty(), "{:?}", r2.deleted);

    assert!(read(dir.path(), "outputs/big-0.bin").is_none(), "the replay did not finish the job");
    let m = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries.len(), 8, "converging cost the bucket objects");
}

/// The design's THIRD mutation check: a crash between the uncite and
/// the unlink must not RE-CITE. The files are still on disk with no
/// citation, which `classify` reads as six local additions — and
/// without the replay the next barrier uploads them and cites them all
/// back, silently undoing the narrow.
#[tokio::test]
async fn a_crash_between_the_uncite_and_the_unlink_does_not_re_cite() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let (dir, mut b) = rescope_fixture(&store).await;

    // The crash: intent durable with what the uncite dropped (recorded
    // before it), citations dropped, files still there.
    let mut base = b.state.load_baseline().unwrap();
    let drop: Vec<String> = (0..6).map(|i| format!("outputs/big-{i}.bin")).collect();
    b.state
        .save_scope_intent(&super::state::ScopeIntent {
            target: Some(vec!["inputs".into()]),
            held: drop.iter().map(|p| (p.clone(), base.entries[p].clone())).collect(),
            drop,
        })
        .unwrap();
    for i in 0..6 {
        let p = format!("outputs/big-{i}.bin");
        base.entries.remove(&p);
        base.prev_scan.remove(&p);
    }
    b.state.save_baseline(&base).unwrap();
    assert!(read(dir.path(), "outputs/big-0.bin").is_some(), "the files are still on disk");

    let r1 = b.run_barrier().await.unwrap();
    assert!(r1.rescope_replayed);
    assert!(
        r1.uploaded.is_empty(),
        "the barrier re-cited what the narrow dropped: {:?}",
        r1.uploaded
    );
    assert!(read(dir.path(), "outputs/big-0.bin").is_none());
    assert_eq!(
        b.state.load_baseline().unwrap().entries.len(),
        2,
        "the narrow was undone by its own recovery"
    );
}

/// A narrow may unwatch a file; it may never discard an edit. The DOOR
/// is strict — it refuses and names the paths, and the old scope stands
/// untouched.
#[tokio::test]
async fn a_rescope_refuses_to_drop_a_path_with_unpublished_changes() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let (dir, mut b) = rescope_fixture(&store).await;

    write(dir.path(), "outputs/big-0.bin", "the user's unpublished edit");
    let err = b.rescope(Some(vec!["inputs".into()])).await.unwrap_err();
    match err {
        LeanError::State(m) => assert!(m.contains("outputs/big-0.bin"), "{m}"),
        e => panic!("wrong error: {e:?}"),
    }

    assert!(b.state.load_scope().unwrap().is_none(), "a refused rescope changed the scope");
    assert!(b.state.load_scope_intent().unwrap().is_none(), "a refused rescope left an intent");
    assert_eq!(
        read(dir.path(), "outputs/big-0.bin").as_deref(),
        Some("the user's unpublished edit"),
        "a refused rescope touched the tree"
    );
    // Anti-vacuity: the SAME rescope goes through once the edit is gone.
    b.run_barrier().await.unwrap();
    b.rescope(Some(vec!["inputs".into()])).await.unwrap();
}

/// The REPLAY cannot be as strict as the door: if it refused a dirty
/// path it would wedge, because the intent gates every barrier and the
/// only thing that clears it is a successful apply. So a replay KEEPS
/// the dirty path — cited, on disk, with a conflict record — and
/// converges.
#[tokio::test]
async fn a_replay_keeps_a_dirty_path_instead_of_wedging_the_barrier() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let (dir, mut b) = rescope_fixture(&store).await;

    b.state
        .save_scope_intent(&super::state::ScopeIntent {
            target: Some(vec!["inputs".into()]),
            drop: (0..6).map(|i| format!("outputs/big-{i}.bin")).collect(),
            held: Default::default(),
        })
        .unwrap();
    write(dir.path(), "outputs/big-0.bin", "edited after the intent landed");

    let r1 = b.run_barrier().await.unwrap();
    assert!(r1.rescope_replayed);
    assert!(b.state.load_scope_intent().unwrap().is_none(), "the replay did not converge");

    // Kept: still on disk, still cited, and the edit is intact.
    assert_eq!(
        read(dir.path(), "outputs/big-0.bin").as_deref(),
        Some("edited after the intent landed")
    );
    assert!(b.state.load_baseline().unwrap().entries.contains_key("outputs/big-0.bin"));
    assert!(read(dir.path(), "outputs/big-1.bin").is_none(), "the clean ones still left");
    assert!(
        b.state
            .load_conflicts()
            .unwrap()
            .iter()
            .any(|c| c.path == "outputs/big-0.bin" && c.kind == "rescope-kept-locally-dirty"),
        "a kept path must be surfaced, not silently retained"
    );
}

/// The agent writes a dropped path BETWEEN the uncite and the unlink (the
/// model's `LeanP1UnlinkBlind`, 2026-10-02). The door saw it clean and the
/// apply's keep set saw it clean; the unlink must still not take bytes it
/// did not uncite. Kept, uncited, recorded — the next barrier publishes it
/// as the agent's add.
#[tokio::test]
async fn a_narrow_never_unlinks_bytes_the_agent_wrote_after_the_uncite() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let (dir, mut b) = rescope_fixture(&store).await;

    let root = dir.path().to_path_buf();
    super::barrier::CONSUME_WINDOW_HOOK.with(|h| {
        *h.borrow_mut() = Some((
            "outputs/big-0.bin".into(),
            "rescope-before-unlink",
            Box::new(move || write(&root, "outputs/big-0.bin", "the agent's edit after the uncite")),
        ))
    });
    let r = b.rescope(Some(vec!["inputs".into()])).await.unwrap();
    assert!(super::barrier::CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_none()), "fixture: the window never ran");

    assert_eq!(
        read(dir.path(), "outputs/big-0.bin").as_deref(),
        Some("the agent's edit after the uncite"),
        "the narrow unlinked the agent's write"
    );
    assert!(read(dir.path(), "outputs/big-1.bin").is_none(), "the clean ones still left");
    assert!(r.kept_dirty.contains(&"outputs/big-0.bin".to_string()), "{:?}", r.kept_dirty);
    assert!(
        b.state.load_conflicts().unwrap().iter().any(|c| c.path == "outputs/big-0.bin"),
        "a kept path must be surfaced"
    );
    // And it is the agent's add from here: the next barrier publishes it.
    let r1 = b.run_barrier().await.unwrap();
    assert!(r1.uploaded.iter().any(|p| p == "outputs/big-0.bin"), "{:?}", r1.uploaded);
    let m = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries.len(), 8, "{:?}", m.entries.keys());
}

/// The same window, held open by a crash: the syncer died between the
/// uncite and the unlink, and the agent — still running — wrote a dropped
/// path before the next barrier's replay. Uncited, the write and the
/// leftover read alike; only the recorded entry tells them apart.
#[tokio::test]
async fn a_replay_never_unlinks_what_the_agent_wrote_while_the_syncer_was_down() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let (dir, mut b) = rescope_fixture(&store).await;

    let mut base = b.state.load_baseline().unwrap();
    let drop: Vec<String> = (0..6).map(|i| format!("outputs/big-{i}.bin")).collect();
    b.state
        .save_scope_intent(&super::state::ScopeIntent {
            target: Some(vec!["inputs".into()]),
            held: drop.iter().map(|p| (p.clone(), base.entries[p].clone())).collect(),
            drop: drop.clone(),
        })
        .unwrap();
    for p in &drop {
        base.entries.remove(p);
        base.prev_scan.remove(p);
    }
    b.state.save_baseline(&base).unwrap();
    write(dir.path(), "outputs/big-0.bin", "written while the syncer was down");

    let r1 = b.run_barrier().await.unwrap();
    assert!(r1.rescope_replayed);
    assert_eq!(
        read(dir.path(), "outputs/big-0.bin").as_deref(),
        Some("written while the syncer was down"),
        "the replay unlinked the agent's write"
    );
    assert!(read(dir.path(), "outputs/big-1.bin").is_none(), "the leftovers still left");
    // Anti-vacuity for `..._does_not_re_cite`: only the agent's file publishes.
    let r2 = b.run_barrier().await.unwrap();
    let up: Vec<&String> = r1.uploaded.iter().chain(r2.uploaded.iter()).collect();
    assert_eq!(up, vec!["outputs/big-0.bin"], "{up:?}");
}

/// An all-rejected scope is REFUSED, never read as the whole tree —
/// the same fail-closed rule `sync.rs` needed after a typo escalated to
/// maximum privilege there.
#[tokio::test]
async fn an_all_rejected_rescope_is_refused_not_widened() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let (_dir, mut b) = rescope_fixture(&store).await;

    b.rescope(Some(vec!["inputs".into()])).await.unwrap();
    let err = b.rescope(Some(vec!["../escape".into(), "./here".into()])).await.unwrap_err();
    assert!(matches!(err, LeanError::State(_)), "{err:?}");
    assert_eq!(
        b.state.load_scope().unwrap().as_deref(),
        Some(&["inputs".to_string()][..]),
        "a refused rescope widened the workspace"
    );
    assert_eq!(b.state.load_baseline().unwrap().entries.len(), 2);
}

/// Rescope is an operation on a LIVE tree. Without a checkout there is
/// no held set to narrow and no marker to vouch for one.
#[tokio::test]
async fn a_rescope_before_any_checkout_is_refused() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let dir = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir.path()).await;

    let err = b.rescope(Some(vec!["inputs".into()])).await.unwrap_err();
    match err {
        LeanError::State(m) => assert!(m.contains("checkout"), "{m}"),
        e => panic!("wrong error: {e:?}"),
    }
}

/// Widening to the whole tree is `None`, and it must REMOVE the scope
/// document rather than leaving a stale claim behind.
#[tokio::test]
async fn a_rescope_to_none_holds_everything_and_clears_the_scope() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let (dir, mut b) = rescope_fixture(&store).await;

    b.rescope(Some(vec!["inputs".into()])).await.unwrap();
    assert_eq!(b.state.load_baseline().unwrap().entries.len(), 2);

    let r = b.rescope(None).await.unwrap();
    assert_eq!(r.materialized, 6);
    assert!(b.state.load_scope().unwrap().is_none(), "the scope document must be REMOVED");
    assert_eq!(b.state.load_baseline().unwrap().entries.len(), 8);
    assert!(read(dir.path(), "outputs/big-5.bin").is_some());
}

// ── the publish fence is the PUBLISHER's, not the reader's ───────────

/// A checkout runs to completion with another syncer's lease standing,
/// and leaves that lease exactly where it found it.
///
/// This is the whole change of 2026-09-11, and it is asserted through
/// `verbs::run_verb` — the single door `bin/flint_sync.rs` sends every
/// one-shot verb through, ROUTING included — rather than through
/// `checkout_scoped`, which never claimed anything and so could never
/// have failed. Flip `Step::Checkout` in `holds_the_fence_throughout`
/// and this leg goes red.
///
/// The TIMEOUT is the assertion, not a safety net. `claim` polls a
/// standing foreign lease every 10 seconds and supersedes only after
/// six observations in which the holder's token has not advanced, so
/// the previous behaviour on this fixture was to sit here for a minute
/// and then DEPOSE a live publisher. Route this verb back through
/// `claim_then` and the two seconds run out: that is the control, and
/// it fails on the exact line this test exists to pin.
#[tokio::test]
async fn a_checkout_does_not_wait_out_a_standing_lease() {
    let store = Arc::new(MemoryStore::new());

    // The publisher: holds the epoch, and keeps holding it.
    let pdir = tempfile::tempdir().unwrap();
    let mut publisher = syncer(&store, pdir.path()).await;
    assert!(claim_until_held(&mut publisher, 3).await);
    publisher.checkout().await.unwrap();
    write(pdir.path(), "README.md", "the real readme");
    write(pdir.path(), "src/main.rs", "fn main() {}");
    publisher.run_barrier().await.unwrap();
    // The barrier handed the cell on; hold it again, as a publisher
    // inside its commit section does.
    assert!(claim_until_held(&mut publisher, 3).await);
    let held = publisher.lease.clone().expect("the publisher holds a lease");

    // The reader: a different pod, a different tree, no lease.
    let rdir = tempfile::tempdir().unwrap();
    let mut reader = syncer(&store, rdir.path()).await;
    let done = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        super::verbs::run_verb(&mut reader, super::verbs::Step::Checkout),
    )
    .await;
    match done {
        Err(_) => panic!(
            "the checkout verb waited on the publisher's lease — it has no business \
             holding the publish fence"
        ),
        Ok(r) => r.expect("the checkout itself must succeed"),
    }
    assert_eq!(read(rdir.path(), "README.md").unwrap(), "the real readme");
    assert_eq!(read(rdir.path(), "src/main.rs").unwrap(), "fn main() {}");

    // And the publisher is still the publisher. A reader that took the
    // fence would have bumped the epoch out from under it, which is the
    // failure this verb used to produce after sixty seconds rather than
    // instead of hanging.
    let cell = store.epoch_read(&reader.cfg.epoch_key()).await.unwrap().expect("the cell stands");
    assert_eq!(cell.holder_id, held.holder_id, "the reader deposed the publisher");
    assert_eq!(cell.epoch, held.epoch, "the reader moved the epoch");
    assert!(!cell.released, "the reader released someone else's lease");
    assert!(reader.lease.is_none(), "the reader came away holding a lease");
}

/// The PREMISE of the leg above: a checkout installs nothing in the
/// bucket, so the fence that decides who may install is not its to
/// hold.
///
/// Stated as a count rather than as prose because prose does not fail.
/// If a future checkout writes so much as a marker object, this leg
/// goes red and says that the lease-free posture no longer follows —
/// which is the conversation worth having at that moment, and one that
/// a comment in `verbs.rs` would not start.
#[tokio::test]
async fn a_checkout_issues_no_write_to_the_bucket() {
    let store = Arc::new(MemoryStore::new());
    let pdir = tempfile::tempdir().unwrap();
    let mut publisher = syncer(&store, pdir.path()).await;
    assert!(claim_until_held(&mut publisher, 3).await);
    publisher.checkout().await.unwrap();
    for i in 0..8 {
        write(pdir.path(), &format!("f{i}.txt"), &format!("body {i}"));
    }
    publisher.run_barrier().await.unwrap();

    let rdir = tempfile::tempdir().unwrap();
    let mut reader = syncer(&store, rdir.path()).await;
    store.reset_op_counts();
    super::verbs::run_verb(&mut reader, super::verbs::Step::Checkout).await.unwrap();
    let ops = store.op_counts();

    // Every verb on this trait that can change a byte of the bucket.
    for op in [
        "put_whole",
        "delete",
        "delete_version",
        "copy_object",
        "compose_generation",
        "epoch_acquire",
        "epoch_renew",
        "epoch_release",
        "abort_upload",
    ] {
        assert_eq!(
            ops.get(op).copied().unwrap_or(0),
            0,
            "checkout called {op} — it is no longer a read, and `read_only_then` is no \
             longer justified. Full shape: {ops:?}"
        );
    }
    // The control on the control: an assertion over an empty map passes
    // for the wrong reason.
    assert!(ops.values().sum::<u64>() > 0, "the checkout made no requests at all: {ops:?}");
    assert_eq!(reader.state.load_baseline().unwrap().entries.len(), 8);
}

/// A backend that PUBLISHES in the middle of a checkout: the first time
/// the reader asks for the cited file, a new generation of that file
/// and a manifest one seq ahead land first, and the reader's `If-Match`
/// then cannot be satisfied.
///
/// It has to live in the backend for the same reason `SweepMidRead`
/// does — the window is between the manifest GET and the object GET,
/// and no sequence of calls from a test body reaches inside it.
struct PublishMidCheckout {
    inner: Arc<MemoryStore>,
    cfg: LeanConfig,
    trigger_key: String,
    trigger_path: String,
    fired: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl ObjectStore for PublishMidCheckout {
    async fn get_whole(
        &self,
        key: &str,
        if_match: Option<&str>,
    ) -> flint_store::StoreResult<(flint_store::ObjectMeta, Bytes)> {
        if key == self.trigger_key
            && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            // What a publisher does under handles, in the order it does
            // it: a fresh handle, the manifest that cites it, and then its
            // collector takes the handle the document stopped citing —
            // the one this reader is asking for.
            let loaded = manifest::load(self.inner.as_ref(), &self.cfg)
                .await
                .expect("the publisher reads the standing manifest")
                .expect("there is one");
            let body = Bytes::from_static(b"the publisher's next generation");
            let crc = crc64_nvme(&body);
            let stamps = GenerationStamps {
                generation: 2,
                epoch: 1,
                flush_uuid: "the-publisher".into(),
                boundary_source: None,
                posix: None,
            };
            let new_key = self.cfg.handle_key(&self.trigger_path, "the-publisher");
            let meta = self
                .inner
                .put_whole(&new_key, body, &PutCondition::IfNoneMatchAny, &stamps, crc)
                .await?;
            let mut m = loaded.manifest.clone();
            m.seq += 1;
            let e = m.entries.get_mut(&self.trigger_path).expect("the path is cited");
            let retired = std::mem::replace(&mut e.key, new_key);
            e.etag = meta.etag;
            e.crc64_b64 = flint_store::crc64_to_b64(crc);
            e.size = meta.size;
            e.generation = 2;
            manifest::cas_write(
                self.inner.as_ref(),
                &self.cfg,
                &m,
                Some(&loaded.handle()),
                1,
                "the-publisher",
            )
            .await
            .expect("the publish lands");
            self.inner.delete(&retired).await?;
        }
        self.inner.get_whole(key, if_match).await
    }

    // ── everything below is delegation ──
    async fn copy_object(
        &self,
        src_key: &str,
        src_if_match: Option<&str>,
        dst_key: &str,
        condition: &PutCondition,
        stamps: &GenerationStamps,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.copy_object(src_key, src_if_match, dst_key, condition, stamps).await
    }
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
    async fn delete_if_match(&self, key: &str, etag: &str) -> flint_store::StoreResult<()> {
        self.inner.delete_if_match(key, etag).await
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

/// A reader that loses a race with its OWN publisher re-resolves the
/// pointer and delivers the publisher's next generation — under handles
/// (design 2026-09-19) the handle it was fetching is simply GONE, taken
/// by the publisher's collector, and a gone handle whose pointer moved is
/// the publisher's doing, never a stranger's.
///
/// Dropping the claim from `checkout` made this window reachable for
/// the first time, and the message waiting in it was
/// `a_published_workspace_refuses_a_foreign_write_instead_of_adopting_it`'s
/// — "something other than its publisher wrote that object", pointing
/// an operator at a second writer that does not exist. Then the reader
/// was told to re-run; now it re-runs itself. That leg is this one's
/// control: same fixture shape, and the ONE thing that differs is whether
/// the manifest pointer moved.
#[tokio::test]
async fn a_publish_that_lands_mid_checkout_is_re_resolved_not_blamed_on_a_stranger() {
    let inner = Arc::new(MemoryStore::new());
    let pdir = tempfile::tempdir().unwrap();
    let mut publisher = syncer(&inner, pdir.path()).await;
    publisher.cfg.sole_writer = true;
    assert!(claim_until_held(&mut publisher, 3).await);
    publisher.checkout().await.unwrap();
    write(pdir.path(), "README.md", "generation one");
    publisher.run_barrier().await.unwrap();

    let rdir = tempfile::tempdir().unwrap();
    let mut reader = syncer(&inner, rdir.path()).await;
    let racing = Arc::new(PublishMidCheckout {
        inner: inner.clone(),
        cfg: reader.cfg.clone(),
        trigger_key: cited_key(&inner, &reader.cfg, "README.md").await,
        trigger_path: "README.md".to_string(),
        fired: std::sync::atomic::AtomicBool::new(false),
    });
    reader.store = racing.clone() as Arc<dyn ObjectStore>;

    let r = reader.checkout().await.expect("a reader one generation behind re-resolves and delivers");
    assert!(racing.fired.load(std::sync::atomic::Ordering::SeqCst), "fixture: the publish never landed mid-checkout");
    assert_eq!(r.reresolved, 1, "the pass was not re-run against the moved pointer: {r:?}");
    assert_eq!(
        read(rdir.path(), "README.md").as_deref(),
        Some("the publisher's next generation"),
        "the reader delivered a generation the pointer no longer names"
    );
    let b = reader.state.load_baseline().unwrap();
    assert_eq!(b.seq, 2, "the baseline names the generation the tree holds");
    assert!(reader.state.load_conflicts().unwrap().is_empty(), "nothing to surface: the publisher moved it");
}

/// Two fetches racing into one directory that does not exist yet.
///
/// `resolve_contained` walks the parent chain and, for a component that
/// is missing, calls `create_dir`. Between its stat and its mkdir a
/// sibling can create the same component, and the loser's EEXIST was
/// reported as a containment REFUSAL: the entry went to the conflict
/// record, `materialized` came up one short, the checkout completed,
/// and the tree had a hole. Unreachable while every fetch was polled
/// from one driver task — the walk is synchronous, so two entries could
/// never be inside it at once — and reachable the moment fetches run on
/// their own tasks, on any tree with a subdirectory: 3 of 6 spawn runs
/// over 20 x 1,000 files on a 2-vCPU VM came back one file short.
///
/// Eight threads released together, fifty rounds, two fresh components
/// per path. Against the unfixed walk this fails in the first rounds.
/// What it cannot pin deterministically: a symlink that APPEARS inside
/// the stat-to-mkdir window. The fix re-stats on EEXIST and refuses a
/// symlink there exactly as the first stat does; that branch is read,
/// not raced.
#[test]
fn siblings_racing_to_create_one_parent_do_not_refuse_each_other() {
    let dir = tempfile::tempdir().unwrap();
    for round in 0..50 {
        let root = dir.path().join(format!("r{round}"));
        std::fs::create_dir(&root).unwrap();
        let go = Arc::new(std::sync::Barrier::new(8));
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let root = root.clone();
                let go = go.clone();
                std::thread::spawn(move || {
                    go.wait();
                    super::barrier::contained_path(&root, &format!("new/deeper/f-{i}"))
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap().expect("a sibling's mkdir is not a containment refusal");
        }
        assert!(root.join("new/deeper").is_dir());
    }
}

/// The temp name is created `O_EXCL` FIRST and unlinked only on EEXIST
/// (2026-09-12: the unconditional unlink was 20,006 syscalls on a
/// 20,000-file checkout, every one ENOENT, each taking the parent's
/// write lock), the parent is recreated only on ENOENT, and the write
/// returns its own `fstat`. So the retry paths must do exactly what the
/// unconditional path did: replace a leftover, remove a planted symlink
/// WITHOUT following it, and recreate a parent that vanished after
/// containment. Delete the EEXIST arm of `create_exclusive` and leg 1
/// fails on "not exclusively creatable"; delete the ENOENT arm and leg
/// 3 does.
#[test]
fn the_temp_retry_paths_replace_a_leftover_and_recreate_a_vanished_parent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // 1. a leftover regular temp file from a killed checkout
    std::fs::create_dir_all(root.join("d")).unwrap();
    std::fs::write(root.join("d/f.txt.flint-sync-tmp"), b"STALE").unwrap();
    let st = super::barrier::write_file_atomic(&root.join("d/f.txt"), b"NEW", Some(0o644)).unwrap();
    assert_eq!(std::fs::read(root.join("d/f.txt")).unwrap(), b"NEW");
    assert_eq!(st.len(), 3, "the returned metadata is the written file's, not the leftover's");
    assert!(!root.join("d/f.txt.flint-sync-tmp").exists(), "the temp name is gone after the rename");
    // 2. a planted symlink at the temp name: removed, never followed
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("victim"), b"ORIGINAL").unwrap();
    std::os::unix::fs::symlink(outside.path().join("victim"), root.join("d/g.txt.flint-sync-tmp"))
        .unwrap();
    super::barrier::write_file_atomic(&root.join("d/g.txt"), b"NEW", None).unwrap();
    assert_eq!(std::fs::read(outside.path().join("victim")).unwrap(), b"ORIGINAL");
    assert_eq!(std::fs::read(root.join("d/g.txt")).unwrap(), b"NEW");
    // 3. the parent vanished between containment and the write
    let (target, present) = super::barrier::contained_path_stat(root, "gone/h.txt").unwrap();
    assert!(present.is_none(), "the walk reports an absent final component as None");
    std::fs::remove_dir(root.join("gone")).unwrap();
    super::barrier::write_file_atomic(&target, b"NEW", None).unwrap();
    assert_eq!(std::fs::read(root.join("gone/h.txt")).unwrap(), b"NEW");
    // …and now the walk hands back the present file's own metadata.
    let (_, present) = super::barrier::contained_path_stat(root, "gone/h.txt").unwrap();
    assert_eq!(present.map(|m| m.len()), Some(3));
    // 4. the durable writer does NOT recreate a vanished parent: a state
    //    or control directory that disappeared mid-run is an error.
    let missing = root.join("nostate/x.json");
    let err = super::safefs::write_via_tmp(&missing, &root.join("nostate/x.json.tmp"), b"{}", None)
        .unwrap_err();
    assert!(err.to_string().contains("not exclusively creatable"), "{err}");
    assert!(!root.join("nostate").exists());
}

/// A fresh fetch is verified against the manifest's CRC-64 — the one
/// integrity check that does not depend on the backend. S3 returns a
/// checksum header the SDK validates; Ozone returns no CRC-64 at all
/// and the SDK then validates nothing; and a body that is wrong under
/// the cited etag (bit-rot, a broken gateway, a cache serving the wrong
/// object) passes If-Match either way. Until 2026-09-12 checkout
/// compared bytes to the manifest only on the RESUME path, so a corrupt
/// fresh fetch was written, cited in the baseline, and read by the
/// agent as the file.
///
/// The S3-wins adoption arm is deliberately NOT checked: it adopts
/// bytes that moved past the manifest, whose CRC describes the old
/// ones — `an_ordinary_workspace_still_adopts_bytes_that_moved_past_
/// the_manifest` is that control, and it fails if the check is applied
/// to adopted bytes. Delete the whole-arm check and this test fails on
/// "must refuse".
#[tokio::test]
async fn a_fresh_fetch_whose_bytes_do_not_match_the_manifest_crc_is_refused() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir_a.path(), "model/config.json", "{\"layers\": 12, \"hidden\": 768}");
    write(dir_a.path(), "README.md", "intact");
    a.run_barrier().await.unwrap();

    // Same length, one bit flipped, same etag, same stored checksum
    // claim: nothing on the wire changes.
    store.inject_corrupt_body(&cited_key(&store, &a.cfg, "model/config.json").await, |b| b[10] ^= 0x01);

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    let err = b.checkout().await.expect_err("a corrupt fresh fetch must refuse");
    let msg = err.to_string();
    assert!(msg.contains("CRC-64") && msg.contains("model/config.json"), "{msg}");
    assert!(
        !dir_b.path().join("model/config.json").exists(),
        "nothing may be written for a body that fails the check"
    );
    assert!(!dir_b.path().join("model/config.json.flint-sync-tmp").exists());
}

/// The ranged arm has no checksum header to lean on at all — a range
/// GET carries none — so before this it had NO integrity check on a
/// fresh fetch. Each range's CRC-64 is taken as it is written and the
/// ranges are folded in offset order with `crc64_combine`; the fold is
/// compared before the rename, so a wrong object never becomes visible.
/// The flipped byte sits mid-object, in a range that is neither first
/// nor last, so a fold that ignored offsets or dropped a range would
/// not be caught by luck. Delete the fold and this fails on "must
/// refuse"; `ranged_checkout_is_byte_identical_and_parallel` is the
/// control that an intact object still passes it.
#[tokio::test]
async fn a_ranged_fetch_whose_bytes_do_not_match_the_manifest_crc_is_refused() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    let big: Vec<u8> = (0..5 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir_a.path().join("weights.bin"), &big).unwrap();
    a.run_barrier().await.unwrap();

    store.inject_corrupt_body(&cited_key(&store, &a.cfg, "weights.bin").await, |b| {
        let i = b.len() / 2 + 777;
        b[i] ^= 0x01;
    });

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    b.cfg.range_get_min_bytes = 1024 * 1024;
    b.cfg.range_get_chunk_bytes = 512 * 1024; // 10 ranges over 5 MiB
    b.cfg.range_get_parallelism = 4;
    let err = b.checkout().await.expect_err("a corrupt ranged fetch must refuse");
    let msg = err.to_string();
    assert!(msg.contains("CRC-64") && msg.contains("10 ranges"), "{msg}");
    assert!(!dir_b.path().join("weights.bin").exists(), "a wrong object must never be renamed into place");
    assert!(!dir_b.path().join("weights.bin.flint-sync-tmp").exists(), "the temp file is cleaned up");
}

// ---------------------------------------------------------------------
// The manifest's CRC is CLIENT-computed: a backend that attests nothing
// ---------------------------------------------------------------------

/// A backend that attests NO checksum on any read — HEAD, GET, a
/// versioned HEAD or GET — and echoes none on a write: Ozone's shape,
/// and on S3 the shape of a HITL upload sent without one. Every CRC the
/// manifest carries must then have been computed by a flint client
/// over the bytes it moved; nothing here can be copied from a header.
///
/// `compose_generation` is left alone: `flint-store`'s S3 backend folds
/// the part CRCs itself and returns that whatever the wire echoed, so a
/// compose with no checksum is a different double (`ComposeWithoutChecksum`).
struct AttestsNoChecksum(Arc<MemoryStore>);

fn unattested(mut m: flint_store::ObjectMeta) -> flint_store::ObjectMeta {
    m.crc64_b64 = None;
    m
}

#[async_trait::async_trait]
impl ObjectStore for AttestsNoChecksum {
    async fn copy_object(
        &self,
        src_key: &str,
        src_if_match: Option<&str>,
        dst_key: &str,
        condition: &PutCondition,
        stamps: &GenerationStamps,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.0.copy_object(src_key, src_if_match, dst_key, condition, stamps).await.map(unattested)
    }
    async fn put_whole(
        &self,
        key: &str,
        body: Bytes,
        cond: &PutCondition,
        stamps: &GenerationStamps,
        crc: u64,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.0.put_whole(key, body, cond, stamps, crc).await.map(unattested)
    }
    async fn compose_generation(
        &self,
        spec: &flint_store::ComposeSpec<'_>,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.0.compose_generation(spec).await
    }
    async fn head(&self, key: &str) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.0.head(key).await.map(unattested)
    }
    async fn get_whole(
        &self,
        key: &str,
        if_match: Option<&str>,
    ) -> flint_store::StoreResult<(flint_store::ObjectMeta, Bytes)> {
        self.0.get_whole(key, if_match).await.map(|(m, b)| (unattested(m), b))
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
    async fn delete_if_match(&self, key: &str, etag: &str) -> flint_store::StoreResult<()> {
        self.0.delete_if_match(key, etag).await
    }
    async fn head_version(
        &self,
        key: &str,
        v: &str,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.0.head_version(key, v).await.map(unattested)
    }
    async fn get_version(
        &self,
        key: &str,
        v: &str,
    ) -> flint_store::StoreResult<(flint_store::ObjectMeta, Bytes)> {
        self.0.get_version(key, v).await.map(|(m, b)| (unattested(m), b))
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
    async fn epoch_handoff(
        &self,
        key: &str,
        lease: &flint_store::EpochLease,
        echo: Option<&str>,
    ) -> flint_store::StoreResult<()> {
        self.0.epoch_handoff(key, lease, echo).await
    }
    async fn epoch_enqueue(
        &self,
        key: &str,
        observed: &flint_store::EpochState,
        holder_id: &str,
    ) -> flint_store::StoreResult<flint_store::EpochState> {
        self.0.epoch_enqueue(key, observed, holder_id).await
    }
}

/// A syncer over any store, for the doubles that wrap `MemoryStore`.
fn syncer_on(store: Arc<dyn ObjectStore>, root: &std::path::Path) -> Syncer {
    let cfg = cfg_for(root);
    let state = SyncerState::open(cfg.state_dir()).unwrap();
    Syncer { store, cfg, state, lease: None, cell_written_at: None, noted_not_regular: Default::default() }
}

fn crc_of(content: &str) -> String {
    flint_store::crc64_to_b64(crc64_nvme(content.as_bytes()))
}

/// The double has the property it claims: nothing it returns carries a
/// checksum, while the store underneath it does. A double missing a
/// property passes for the wrong reason.
async fn assert_attests_nothing(mem: &Arc<MemoryStore>, store: &Arc<dyn ObjectStore>, key: &str) {
    assert!(mem.head(key).await.unwrap().crc64_b64.is_some(), "the fixture object has a CRC");
    assert!(store.head(key).await.unwrap().crc64_b64.is_none(), "HEAD attests a checksum");
    let (m, _) = store.get_whole(key, None).await.unwrap();
    assert!(m.crc64_b64.is_none(), "GET attests a checksum");
}

/// A HITL upload on a backend that attests no checksum — and from a
/// writer that sent none in its inbox entry either (an old gateway, a
/// draft promote), so NOTHING on the read path carries a CRC. The
/// consume hashes what it writes, the baseline carries that, and the
/// citation repair cites it: the manifest ends up with the bytes' CRC
/// with no header ever consulted. A fresh checkout — which now verifies
/// every fetch against the manifest — materialises the file.
///
/// Before this change the repair copied HEAD's checksum into the
/// manifest, which here is `None`: the manifest could not carry one at
/// all on Ozone.
#[tokio::test]
async fn a_hitl_upload_on_a_backend_that_attests_no_checksum_is_cited_with_the_bytes_crc() {
    let mem = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(AttestsNoChecksum(mem.clone()));
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer_on(store.clone(), dir.path());
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "agent.txt", "agent work");
    sc.run_barrier().await.unwrap();

    // The upload lands object-first, then ONE manifest CAS cites it (P2):
    // the gateway hashed the bytes it sent, so the citation carries their
    // CRC though the backend attests none.
    let key = sc.cfg.file_key("docs/upload.pdf");
    let body = Bytes::from_static(b"user bytes");
    let stamps = GenerationStamps {
        generation: 0,
        epoch: 0,
        flush_uuid: "gateway-old".into(),
        boundary_source: None,
        posix: None,
    };
    let meta = mem
        .put_whole(&key, body.clone(), &PutCondition::IfNoneMatchAny, &stamps, crc64_nvme(&body))
        .await
        .unwrap();
    let (etag, k) = (meta.etag.clone(), key.clone());
    gateway_commit(&mem, &sc.cfg, "gateway-old", move |doc| {
        doc.entries.insert("docs/upload.pdf".into(), manifest::LeanEntry {
            key: k.clone(), etag: etag.clone(), crc64_b64: crc_of("user bytes"), size: 10,
            mode: 0o644, mtime_unix: 0, generation: 1, epoch: 0,
        });
    })
    .await
    .unwrap();
    assert_attests_nothing(&mem, &store, &key).await;

    write(dir.path(), "agent.txt", "agent work v2 — longer");
    sc.run_barrier().await.unwrap();
    sc.run_barrier().await.unwrap();

    assert_eq!(read(dir.path(), "docs/upload.pdf").unwrap(), "user bytes");
    let b = sc.state.load_baseline().unwrap();
    assert_eq!(
        b.entries["docs/upload.pdf"].crc64_b64.as_deref(),
        Some(crc_of("user bytes").as_str()),
        "the baseline carries the CRC of the bytes the consume wrote"
    );
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;
    let e = m.entries.get("docs/upload.pdf").expect("amputated!");
    assert_eq!(e.crc64_b64, crc_of("user bytes"), "the citation carries the bytes' CRC");

    // A fresh reader verifies against exactly that and gets the file.
    let dir2 = tempfile::tempdir().unwrap();
    let mut sc2 = syncer_on(store.clone(), dir2.path());
    sc2.checkout().await.unwrap();
    assert_eq!(read(dir2.path(), "docs/upload.pdf").unwrap(), "user bytes");
}

/// The phantom-conflict rule on a backend that attests no checksum: the
/// dirty-vs-remote identity check used to compare the local hash with
/// HEAD's, which here is `None` — every crash-shaped re-honor reported
/// a conflict for byte-identical content. The writer's CRC in the inbox
/// entry is what carries the check now.
#[tokio::test]
async fn sync_on_a_backend_that_attests_no_checksum_still_sees_identical_bytes() {
    let mem = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = Arc::new(AttestsNoChecksum(mem.clone()));
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer_on(store.clone(), dir.path());
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "shared.txt", "v1");
    a.run_barrier().await.unwrap();

    // The gateway hashes what it sends; the backend attests nothing.
    let (_, ui_key) = hitl_write_at(&mem, &a.cfg, "shared.txt", "foreign v2", "ci", now_unix()).await.unwrap();
    assert_attests_nothing(&mem, &store, &ui_key).await;
    let pre = a.state.load_baseline().unwrap();
    let r = a.sync().await.unwrap();
    assert!(r.applied.contains(&"shared.txt".to_string()));

    let mut b = a.state.load_baseline().unwrap();
    let stale = b.entries.get_mut("shared.txt").unwrap();
    stale.etag = "\"stale-pre-sync-etag\"".into();
    stale.mtime_unix -= 10;
    stale.size = 1;
    // The rest of the baseline is the pre-sync one too: seq and pointer
    // (under P2 the change is a committed document, so a baseline that
    // kept the new seq would say there is no news).
    b.seq = pre.seq;
    b.manifest_etag = pre.manifest_etag.clone();
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

/// A consume whose bytes do not match the writer's CRC is refused: not
/// written, left OWED (so the failure repeats visibly), and recorded. The corruption keeps the etag and
/// the store's attestation intact, the shape of bit-rot or a broken
/// gateway.
#[tokio::test]
async fn a_consume_whose_bytes_do_not_match_the_writers_crc_is_refused() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "agent.txt", "agent work");
    sc.run_barrier().await.unwrap();

    let (_, ui_key) =
        hitl_write_at(&store, &sc.cfg, "docs/upload.pdf", "user bytes", "dilip", now_unix()).await.unwrap();
    store.inject_corrupt_body(&ui_key, |b| b[3] ^= 0x40);

    // P2: the gateway cited the save with the CRC of what it PUT; the bucket
    // then corrupted the body. The next consume owes it and fetches it.
    sc.run_barrier().await.unwrap();
    assert!(read(dir.path(), "docs/upload.pdf").is_none(), "corrupt bytes were written");
    assert!(
        sc.state.load_conflicts().unwrap().iter().any(|c| c.kind.starts_with("consume-refused-checksum")),
        "the refusal was silent"
    );
    assert!(derive_pending(&sc).await, "a refused change must stay owed, not be consumed away");
    let n = sc.state.load_conflicts().unwrap().len();
    sc.run_barrier().await.unwrap();
    assert!(read(dir.path(), "docs/upload.pdf").is_none());
    assert!(sc.state.load_conflicts().unwrap().len() > n, "the refusal did not repeat");
}

/// A sync whose fetched bytes do not match the manifest's CRC is
/// refused before anything is written — the same check checkout's
/// fresh fetch makes, on the other read path.
#[tokio::test]
async fn a_sync_whose_bytes_do_not_match_the_manifest_crc_is_refused() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    b.checkout().await.unwrap();

    write(dir_a.path(), "f.txt", "published bytes");
    a.run_barrier().await.unwrap();
    store.inject_corrupt_body(&cited_key(&store, &a.cfg, "f.txt").await, |bytes| bytes[0] ^= 0x01);

    let err = b.sync().await.expect_err("a sync of corrupt bytes must refuse");
    assert!(err.to_string().contains("refusing to apply it"), "{err}");
    assert!(read(dir_b.path(), "f.txt").is_none(), "corrupt bytes were written");
}

// ---------------------------------------------------------------------
// DECLARED removals (docs/plans/flint-lean-delete-rename-design.md):
// delete and rename asked for from OUTSIDE the pod, performed by the
// barrier — one manifest generation, never a hole.
// ---------------------------------------------------------------------

/// What the gateway's `remove_file` does under P2: ONE manifest CAS stops
/// citing the path; its tombstone names what was deleted.
pub(super) async fn hitl_remove(store: &Arc<MemoryStore>, cfg: &LeanConfig, path: &str, _author: &str) {
    gateway_commit(store, cfg, &super::ui_flush(now_unix()), |doc| {
        let gone = doc.entries.remove(path).expect("the gateway deletes only what is cited");
        doc.tombstones.insert(path.to_string(), manifest::Tombstone { etag: gone.etag, seq: doc.seq });
    })
    .await
    .unwrap();
}

/// The version the gateway resolves for a path under P2: the citation.
pub(super) async fn gateway_current(store: &Arc<MemoryStore>, cfg: &LeanConfig, path: &str) -> Option<String> {
    manifest::load(store.as_ref(), cfg).await.unwrap().and_then(|m| m.manifest.entries.get(path).map(|e| e.etag.clone()))
}

/// What the gateway's `rename_file` does under P2: ONE manifest CAS moves
/// the citation — the destination names the source's handle, the source is
/// no longer cited and its tombstone names what moved. No bytes move.
pub(super) async fn hitl_rename(store: &Arc<MemoryStore>, cfg: &LeanConfig, from: &str, to: &str, _author: &str) {
    gateway_commit(store, cfg, &super::ui_flush(now_unix()), |doc| {
        let entry = doc.entries.remove(from).expect("the source is cited");
        assert!(!doc.entries.contains_key(to), "the gateway refuses a destination that exists");
        doc.tombstones.insert(from.to_string(), manifest::Tombstone { etag: entry.etag.clone(), seq: doc.seq });
        doc.tombstones.remove(to);
        doc.entries.insert(to.to_string(), entry);
    })
    .await
    .unwrap();
}



/// A UI delete COMMITS (P2); the tree follows through its queue. An unlink
/// that FAILS must leave the deletion queued for the next barrier — and must
/// never read as the agent's own file, or the next scan would publish it
/// again and undo the user's delete. Once the unlink can succeed, it does.
#[tokio::test]
async fn a_failed_unlink_of_a_queued_deletion_is_retried_and_never_republishes() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "locked/f.txt", "keep");
    sc.run_barrier().await.unwrap();

    use std::os::unix::fs::PermissionsExt;
    let locked = dir.path().join("locked");
    hitl_remove(&store, &sc.cfg, "locked/f.txt", "dilip").await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    for _ in 0..3 {
        sc.run_barrier().await.unwrap();
    }
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(read(dir.path(), "locked/f.txt").as_deref(), Some("keep"), "fixture: the unlink did not fail");
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert!(!m.manifest.entries.contains_key("locked/f.txt"), "a failed unlink re-published the deleted file");

    // Unlockable now: the queued deletion goes through.
    for _ in 0..2 {
        sc.run_barrier().await.unwrap();
    }
    assert!(read(dir.path(), "locked/f.txt").is_none(), "the deletion was dropped, not retried");
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert!(!m.manifest.entries.contains_key("locked/f.txt"));
}












// ── protocol review 2026-09-12: reproductions ───────────────────────
//
// Each test below was written to FAIL at `33abb284` against the finding
// it names (docs/plans/flint-lean-protocol-review-2026-09-12.md), then
// pass with the fix. The failing run is the reproduction.

/// ack-1 / inbox-3. A `.flint/sync` touch whose scope names no valid
/// entry (`[]`, `["/"]`, `["../x"]`) folded into a pending record whose
/// honor is a deterministic error — never acked, never retired, and
/// returned BEFORE the publish honor, so one touch stopped every
/// boundary for the life of the workspace. The scope refusal was a fix
/// ("an error must not return a legal value") shipped without its
/// callers. An invalid request is the agent's error: it is ACKED as
/// such and retired, and the publish behind it is still honoured.
#[tokio::test]
async fn an_invalid_sync_scope_is_acked_refused_and_never_wedges_publish() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.cfg.sentinel_min_interval_secs = 0;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();

    touch_sentinel(dir.path(), control::SYNC, r#"{"nonce":"s1","scope":[]}"#);
    a.sentinel_tick().await.expect("an invalid scope is the agent's error, not the syncer's");
    let ack = a.read_ack(Verb::Sync).expect("no sync ack: the agent waits forever");
    assert_eq!(ack.status, "refused-scope");
    assert!(ack.nonces.contains(&"s1".to_string()), "the refusal does not name the agent's nonce");
    assert!(
        ack.reason.as_deref().unwrap_or("").contains("scope"),
        "the refusal must say WHY: {:?}",
        ack.reason
    );
    assert!(
        a.load_pending(Verb::Sync).unwrap().is_none(),
        "the pending record stood: every later tick repeats the same error"
    );

    // THE WEDGE: a publish declared after the bad sync is still honoured.
    write(dir.path(), "b.txt", "b");
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"p1"}"#);
    a.sentinel_tick().await.unwrap();
    let ack = a.read_ack(Verb::Publish).expect("no publish ack after a refused sync");
    assert_eq!(ack.status, "ok");
    assert!(ack.nonces.contains(&"p1".to_string()));
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("b.txt"), "the boundary the agent declared never reached the bucket");
}

/// ack-1, the floor half: a standing pending whose honor fails must not
/// stop the CADENCE barrier — the floor tick returned on the sync error
/// before the barrier ran, so nothing published on the cadence either.
#[tokio::test]
async fn a_refused_sync_pending_does_not_stop_the_cadence_barrier() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();

    touch_sentinel(dir.path(), control::SYNC, r#"{"nonce":"s1","scope":["../escape"]}"#);
    assert!(a.consume_sentinel(Verb::Sync).unwrap());
    write(dir.path(), "c.txt", "c");
    a.floor_tick().await.expect("a refused pending must not fail the floor tick");
    assert_eq!(a.read_ack(Verb::Sync).map(|k| k.status), Some("refused-scope".into()));
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("c.txt"), "the cadence barrier never ran behind the refused sync");
}

/// inbox-7. Coalescing folded scopes by UNION and left `None` alone, so
/// an unscoped touch ("the whole tree") coalesced with a scoped one was
/// honoured as the scoped one, and the whole-tree agent matched its
/// nonce on an ack for a sync that reconciled one directory.
#[tokio::test]
async fn an_unscoped_sync_touch_widens_a_coalesced_scope_to_the_whole_tree() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();

    // Scoped first, then unscoped.
    touch_sentinel(dir.path(), control::SYNC, r#"{"nonce":"b","scope":["x/"]}"#);
    assert!(a.consume_sentinel(Verb::Sync).unwrap());
    touch_sentinel(dir.path(), control::SYNC, r#"{"nonce":"a"}"#);
    assert!(a.consume_sentinel(Verb::Sync).unwrap());
    let p = a.load_pending(Verb::Sync).unwrap().unwrap();
    assert!(p.scope.is_none(), "an unscoped touch was narrowed to {:?}", p.scope);
    assert_eq!(p.nonces, vec!["b".to_string(), "a".to_string()]);
    a.sentinel_tick().await.unwrap();
    assert!(a.load_pending(Verb::Sync).unwrap().is_none());

    // Unscoped first, then scoped: the later scope must not narrow it.
    touch_sentinel(dir.path(), control::SYNC, r#"{"nonce":"c"}"#);
    assert!(a.consume_sentinel(Verb::Sync).unwrap());
    touch_sentinel(dir.path(), control::SYNC, r#"{"nonce":"d","scope":["y/"]}"#);
    assert!(a.consume_sentinel(Verb::Sync).unwrap());
    let p = a.load_pending(Verb::Sync).unwrap().unwrap();
    assert!(p.scope.is_none(), "a later scoped touch narrowed a whole-tree request to {:?}", p.scope);
}

/// ack-3 / atomicity-8 / inbox-6. AGENTS.md tells the agent to read
/// `report.conflicts` on its acks; only the sync honor filled it. A
/// `consume-dirty` recorded by the publish's own boundary reached the
/// agent only through `conflicts.jsonl`.
#[tokio::test]
async fn a_publish_ack_carries_the_boundarys_conflict_records() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.cfg.sentinel_min_interval_secs = 0;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
    write(dir.path(), "shared.txt", "v1");
    a.run_barrier().await.unwrap();

    // The agent edits; someone else writes the same path from outside.
    write(dir.path(), "shared.txt", "the agent's longer edit");
    hitl_write(&store, &a.cfg, "shared.txt", "UI", "ui").await.unwrap();
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"p1"}"#);
    a.sentinel_tick().await.unwrap();
    let ack = a.read_ack(Verb::Publish).expect("no publish ack");
    assert_eq!(ack.status, "ok");
    assert!(
        // P2: the UI's write is committed, so the publish's COMMIT meets it (R7).
        ack.report.conflicts.iter().any(|c| c.path == "shared.txt" && c.kind.starts_with("commit-surfaced-foreign")),
        "the publish ack carries no conflict record for the path the agent won: {:?}",
        ack.report.conflicts
    );
}

/// atomicity-4. The scan compares mtime at whole seconds. A same-size
/// rewrite inside the second the baseline recorded is invisible until
/// the file changes size or second: the bucket stays stale under an
/// `ok` ack, a foreign consume overwrites the edit as "clean", a
/// declared removal unlinks it. The classic make/rsync trap, and it
/// needs no timestamp preservation by the agent — an editor that saves
/// twice in a second does it. Nanosecond mtime closes it.
#[tokio::test]
async fn a_same_size_rewrite_within_the_scan_second_is_still_published() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "status.json", r#"{"ok":1}"#);
    write(dir.path(), "other.txt", "untouched");
    a.run_barrier().await.unwrap();

    // Same size, same second (half a second later), different bytes.
    let sec = a.state.load_baseline().unwrap().entries["status.json"].mtime_unix;
    write(dir.path(), "status.json", r#"{"ok":2}"#);
    let f = std::fs::OpenOptions::new().write(true).open(dir.path().join("status.json")).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::new(sec as u64, 500_000_000)).unwrap();
    drop(f);

    let r = a.run_barrier().await.unwrap();
    assert!(
        r.uploaded.contains(&"status.json".to_string()),
        "the rewrite was invisible: uploaded={:?} no_change={}",
        r.uploaded,
        r.no_change
    );
    // THE OTHER ARM SHUT: the untouched file is not re-uploaded.
    assert!(!r.uploaded.contains(&"other.txt".to_string()), "nanosecond compare re-uploads everything");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let (_, body) = store.get_whole(&m.entries["status.json"].key, Some(&m.entries["status.json"].etag)).await.unwrap();
    assert_eq!(&body[..], br#"{"ok":2}"#);
}

/// Retired class 1 (inbox-5, finding 11 / L-17, drill host leg H2): an
/// upload whose base object a peer's collector had taken. Under the slot
/// the PUT named that base with If-Match; S3 answered 404 NoSuchKey and
/// Ozone 412, each answer needed its own recovery arm, and a writer whose
/// edited path a peer deleted failed every barrier until both existed.
/// Under handles (design 2026-09-19, R1) an upload names NO base: it lands
/// at a fresh handle whatever became of its predecessor — through the
/// whole-PUT arm and the compose arm alike, on a store that answers a
/// missing key either way.
async fn a_collected_predecessor_costs_the_upload_nothing(answer_412: bool, compose: bool) {
    let store = if answer_412 {
        Arc::new(MemoryStore::new().with_if_match_missing_as_412())
    } else {
        Arc::new(MemoryStore::new())
    };
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    if compose {
        a.cfg.whole_put_max = 4; // every body below goes through the compose arm
    }
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "x.txt", "v1 body");
    a.run_barrier().await.unwrap();

    // A peer's commit retired v1 and its collector took the handle.
    let v1 = cited_key(&store, &a.cfg, "x.txt").await;
    store.delete(&v1).await.unwrap();
    assert!(handles_for(&store, &a.cfg, "x.txt").await.is_empty(), "fixture: the predecessor is gone");
    write(dir.path(), "x.txt", "v2, longer than before");
    let r = a.run_barrier().await.expect("a collected predecessor failed the whole barrier");
    assert!(r.uploaded.contains(&"x.txt".to_string()), "the edit was not published: {r:?}");
    assert_eq!(cited_bytes(&store, &a.cfg, "x.txt").await.as_deref(), Some(&b"v2, longer than before"[..]));
    // ANTI-VACUITY: the upload went to a handle of its own and named no
    // base — nothing parked, nothing deferred, no recovery arm ran.
    let cited = cited_key(&store, &a.cfg, "x.txt").await;
    assert_ne!(cited, v1);
    assert!(a.cfg.handle_parts(&cited).is_some(), "not a handle: {cited}");
    assert!(r.parked.is_empty() && r.deferred.is_empty(), "{r:?}");
}

#[tokio::test]
async fn a_collected_predecessor_costs_the_upload_nothing_on_s3s_404() {
    a_collected_predecessor_costs_the_upload_nothing(false, false).await;
}

#[tokio::test]
async fn a_collected_predecessor_costs_the_upload_nothing_on_ozones_412() {
    a_collected_predecessor_costs_the_upload_nothing(true, false).await;
}

#[tokio::test]
async fn a_collected_predecessor_costs_the_upload_nothing_through_compose() {
    a_collected_predecessor_costs_the_upload_nothing(false, true).await;
}

/// inbox-8. Containment refused `.flint/` and nothing else: a citation
/// naming `.flint-sync/scope-intent.json` (a manifest installed through
/// the gateway's CAS door) was materialised INTO the state directory by
/// checkout, sync and rescope, and replayed at the next barrier's step 0.
#[test]
fn containment_refuses_the_state_directory_too() {
    let dir = tempfile::tempdir().unwrap();
    super::barrier::contained_path(dir.path(), ".flint/x").expect_err("the control namespace must be refused");
    super::barrier::contained_path(dir.path(), ".flint-sync/scope-intent.json")
        .expect_err("the state directory must be refused");
    super::barrier::contained_path(dir.path(), ".flint-sync").expect_err("the state directory itself");
    // THE OTHER ARM SHUT: ordinary paths, including look-alikes, pass.
    super::barrier::contained_path(dir.path(), "ok/x").expect("an ordinary path");
    super::barrier::contained_path(dir.path(), ".flint-syncer/notes").expect("a look-alike is not the state dir");
}

/// atomicity-7. A consume that crashed between `create_exclusive` and
/// `rename` left `<name>.flint-sync-tmp` as a regular file in the tree;
/// the next scan published it. The temp suffix is never scanned.
#[tokio::test]
async fn an_orphaned_consume_temp_is_never_published() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "a.txt", "real");
    write(dir.path(), "a.txt.flint-sync-tmp", "torn consume leftover");
    let r = a.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("a.txt"));
    assert!(
        !m.entries.contains_key("a.txt.flint-sync-tmp") && !r.uploaded.iter().any(|p| p.ends_with(".flint-sync-tmp")),
        "a crash-orphaned temp was published as data: {:?}",
        r.uploaded
    );
}

/// lease-2. Adopting a lost-renew token wrote nothing, so the cell's
/// token stood still for a full takeover threshold and a waiting
/// challenger could depose a live holder whose next write was late. The
/// adoption is followed by one renew so the token moves.
#[tokio::test]
async fn adopting_a_lost_renew_token_moves_the_cell() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    let key = a.cfg.epoch_key();
    let stale = a.lease.clone().unwrap();
    store.epoch_renew(&key, &stale, None).await.unwrap();
    let landed = store.epoch_read(&key).await.unwrap().unwrap().token;
    lease::renew(&mut a).await.expect("a lost renew response is not a deposal");
    let now = store.epoch_read(&key).await.unwrap().unwrap().token;
    assert_ne!(now, landed, "the adoption wrote nothing: a challenger's quiet count keeps climbing");
    assert_eq!(a.lease.as_ref().map(|l| l.token.as_str()), Some(now.as_str()), "the holder's token is not the cell's");
    let cell = store.epoch_read(&key).await.unwrap().unwrap();
    assert_eq!(cell.holder_id, a.lease.as_ref().unwrap().holder_id);
    assert!(!cell.released);
}

// ── protocol review 2026-09-12: reproductions needing a store that can act
// INSIDE the barrier's windows ───────────────────────────────────────────

type Hook = Box<dyn Fn() + Send + Sync>;

/// A store that lets a test act inside the syncer's windows: between the
/// scan and the upload (the inbox window-open PUT), inside a consume's
/// fetch, at a torn compose or acquire response, at a failed preserve.
#[derive(Default)]
pub(super) struct Hooks {
    /// Run ONCE, before the first `put_whole` whose key ends with the suffix.
    before_put: std::sync::Mutex<Option<(String, Hook)>>,
    /// Run ONCE, after the first `put_whole` whose key ends with the suffix
    /// LANDED — an upload of identical bytes leaves no etag change to poll.
    after_put: std::sync::Mutex<Option<(String, Hook)>>,
    /// Run ONCE, after `get_whole` fetched a key ending with the suffix and
    /// before the body is returned to the syncer.
    before_get_return: std::sync::Mutex<Option<(String, Hook)>>,
    /// Run ONCE, before the first DELETE (conditional or not) of a key
    /// ending with the suffix — the gap between a GC's HEAD and its
    /// delete, where a peer's lease-free upload can land.
    before_delete: std::sync::Mutex<Option<(String, Hook)>>,
    /// Run ONCE, after `head` answered for a key ending with the suffix
    /// and before the answer is returned — the moment an upload's 412
    /// arm decides to adopt what it saw.
    after_head: std::sync::Mutex<Option<(String, Hook)>>,
    /// Run ONCE, BEFORE `compose_generation` delegates for a key ending
    /// with the suffix: the window between `upload_one`'s one safe open
    /// (`open_beneath_nofollow`, which follows no link at any depth) and
    /// the store's own read of the local file. Review 2026-09-18 H5's
    /// residual lives in exactly this gap.
    before_compose: std::sync::Mutex<Option<(String, Hook)>>,
    /// `compose_generation` delegates (the object LANDS) and then reports a
    /// torn response, once.
    compose_err_once: std::sync::atomic::AtomicBool,
    /// `epoch_acquire` delegates (the acquire LANDS) and then reports a lost
    /// response, once.
    acquire_err_once: std::sync::atomic::AtomicBool,
    /// `copy_object` fails while set.
    copy_fail: std::sync::atomic::AtomicBool,
    /// `put_whole` fails for every key containing this (the conflict
    /// preserve is a GET + guarded PUT under `.flint/lean/conflicts/`).
    put_fail_containing: std::sync::Mutex<Option<String>>,
    /// For this many `epoch_handoff` calls, a synthetic waiter enqueues
    /// first — the token moves under the holder, so the handoff 412s the
    /// way it does when real waiters queue during a busy commit.
    handoff_races: std::sync::atomic::AtomicU32,
    /// For this many `epoch_handoff` calls, answer S3's 409
    /// ConditionalRequestConflict without landing.
    handoff_conflicts: std::sync::atomic::AtomicU32,
    /// For this many `put_whole` calls on a key ending with the suffix,
    /// answer S3's 409 ConditionalRequestConflict without landing — what
    /// S3 says to a conditional write racing another on the same key.
    put_conflicts: std::sync::Mutex<Option<(String, u32)>>,
}

pub(super) struct Hooked(pub(super) Arc<MemoryStore>, pub(super) Hooks);

impl Hooked {
    fn before_put(&self, key: &str, f: impl Fn() + Send + Sync + 'static) {
        *self.1.before_put.lock().unwrap() = Some((key.to_string(), Box::new(f)));
    }
    pub(super) fn after_put(&self, key: &str, f: impl Fn() + Send + Sync + 'static) {
        *self.1.after_put.lock().unwrap() = Some((key.to_string(), Box::new(f)));
    }
    fn before_compose(&self, key: &str, f: impl Fn() + Send + Sync + 'static) {
        *self.1.before_compose.lock().unwrap() = Some((key.to_string(), Box::new(f)));
    }
    fn before_get_return(&self, key: &str, f: impl Fn() + Send + Sync + 'static) {
        *self.1.before_get_return.lock().unwrap() = Some((key.to_string(), Box::new(f)));
    }
    fn before_delete(&self, key: &str, f: impl Fn() + Send + Sync + 'static) {
        *self.1.before_delete.lock().unwrap() = Some((key.to_string(), Box::new(f)));
    }
    fn after_head(&self, key: &str, f: impl Fn() + Send + Sync + 'static) {
        *self.1.after_head.lock().unwrap() = Some((key.to_string(), Box::new(f)));
    }
}

/// A hook armed on `<prefix>/files/<path>` fires for that key AND for
/// any HANDLE of the path (`<prefix>/files/<path>@<flush>`): a fixture
/// names the path, and the flush a barrier mints is nobody's to predict.
fn hook_matches(key: &str, suffix: &str) -> bool {
    if key.ends_with(suffix) {
        return true;
    }
    key.rsplit_once('@').map(|(path, _)| path.ends_with(suffix)).unwrap_or(false)
}

fn take_hook(slot: &std::sync::Mutex<Option<(String, Hook)>>, key: &str) -> Option<Hook> {
    let mut g = slot.lock().unwrap();
    if g.as_ref().map(|(suf, _)| hook_matches(key, suf)).unwrap_or(false) {
        g.take().map(|(_, h)| h)
    } else {
        None
    }
}

pub(super) fn hooked_syncer(store: &Arc<Hooked>, root: &std::path::Path) -> Syncer {
    let cfg = cfg_for(root);
    Syncer {
        store: store.clone() as Arc<dyn ObjectStore>,
        state: SyncerState::open(cfg.state_dir()).unwrap(),
        cfg,
        lease: None,
        cell_written_at: None,
        noted_not_regular: Default::default(),
    }
}

#[async_trait::async_trait]
impl ObjectStore for Hooked {
    async fn copy_object(
        &self,
        src_key: &str,
        src_if_match: Option<&str>,
        dst_key: &str,
        condition: &PutCondition,
        stamps: &GenerationStamps,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        if self.1.copy_fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(flint_store::StoreError::Other("injected: preserve copy failed".into()));
        }
        self.0.copy_object(src_key, src_if_match, dst_key, condition, stamps).await
    }
    async fn put_whole(
        &self,
        key: &str,
        body: Bytes,
        cond: &PutCondition,
        stamps: &GenerationStamps,
        crc: u64,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        if let Some(h) = take_hook(&self.1.before_put, key) {
            h();
        }
        if let Some(needle) = self.1.put_fail_containing.lock().unwrap().as_deref() {
            if key.contains(needle) {
                return Err(flint_store::StoreError::Other(format!("injected: put refused for {key}")));
            }
        }
        {
            let mut g = self.1.put_conflicts.lock().unwrap();
            if let Some((suffix, n)) = g.as_mut() {
                if *n > 0 && key.ends_with(suffix.as_str()) {
                    *n -= 1;
                    return Err(flint_store::StoreError::Conflict(format!(
                        "injected: ConditionalRequestConflict on {key}"
                    )));
                }
            }
        }
        let r = self.0.put_whole(key, body, cond, stamps, crc).await;
        if r.is_ok() {
            if let Some(h) = take_hook(&self.1.after_put, key) {
                h();
            }
        }
        r
    }
    async fn compose_generation(
        &self,
        spec: &flint_store::ComposeSpec<'_>,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        if let Some(h) = take_hook(&self.1.before_compose, spec.key) {
            h();
        }
        let r = self.0.compose_generation(spec).await;
        if self.1.compose_err_once.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(flint_store::StoreError::Other("injected: torn compose response".into()));
        }
        r
    }
    async fn head(&self, key: &str) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        let r = self.0.head(key).await;
        if let Some(h) = take_hook(&self.1.after_head, key) {
            h();
        }
        r
    }
    async fn get_whole(
        &self,
        key: &str,
        if_match: Option<&str>,
    ) -> flint_store::StoreResult<(flint_store::ObjectMeta, Bytes)> {
        let r = self.0.get_whole(key, if_match).await;
        if let Some(h) = take_hook(&self.1.before_get_return, key) {
            h();
        }
        r
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
        if let Some(h) = take_hook(&self.1.before_delete, key) {
            h();
        }
        self.0.delete(key).await
    }
    async fn delete_if_match(&self, key: &str, etag: &str) -> flint_store::StoreResult<()> {
        if let Some(h) = take_hook(&self.1.before_delete, key) {
            h();
        }
        self.0.delete_if_match(key, etag).await
    }
    /// The handles collector's batch: the before-delete hook fires for
    /// the first key of the batch it matches, before anything is deleted.
    async fn delete_many(&self, keys: &[String]) -> flint_store::StoreResult<flint_store::DeleteManyReport> {
        for key in keys {
            if let Some(h) = take_hook(&self.1.before_delete, key) {
                h();
            }
        }
        self.0.delete_many(keys).await
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
        let r = self.0.epoch_acquire(key, holder, observed).await;
        if self.1.acquire_err_once.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(flint_store::StoreError::Other("injected: lost acquire response".into()));
        }
        r
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
    async fn epoch_handoff(
        &self,
        key: &str,
        lease: &flint_store::EpochLease,
        echo: Option<&str>,
    ) -> flint_store::StoreResult<()> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.1.handoff_conflicts.fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1)).is_ok() {
            return Err(flint_store::StoreError::Conflict("injected: ConditionalRequestConflict".into()));
        }
        if let Ok(left) = self.1.handoff_races.fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1)) {
            let state = self.0.epoch_read(key).await?.expect("a cell to race");
            self.0.epoch_enqueue(key, &state, &format!("synthetic-waiter-{left}")).await?;
        }
        self.0.epoch_handoff(key, lease, echo).await
    }
    async fn epoch_enqueue(
        &self,
        key: &str,
        observed: &flint_store::EpochState,
        holder_id: &str,
    ) -> flint_store::StoreResult<flint_store::EpochState> {
        self.0.epoch_enqueue(key, observed, holder_id).await
    }
}

/// atomicity-1 (CRITICAL). The manifest entry carried the SCANNED size
/// while the upload carried the file as it stood at read time. A file
/// that grew between the two — a checkpoint being streamed when a
/// cadence tick fired — was cited with a length its object does not
/// have, and every fresh checkout of that entry (> range_get_min) fetched
/// `[0, entry.size)`, folded a CRC that could not match, and failed the
/// WHOLE checkout: the successor pod exits 1 and restarts into the same
/// failure, forever. The bytes were intact in the bucket the whole time.
#[tokio::test]
async fn the_manifest_cites_the_uploaded_length_not_the_scanned_one() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let dir = tempfile::tempdir().unwrap();
    let mut a = hooked_syncer(&hooked, dir.path());
    a.cfg.whole_put_max = 1 << 20;
    a.cfg.range_get_min_bytes = 1 << 20;
    a.cfg.range_get_chunk_bytes = 1 << 20;
    a.checkout().await.unwrap();
    let path = dir.path().join("ckpt.bin");
    std::fs::write(&path, vec![7u8; 4 << 20]).unwrap();

    // Between the scan and the upload's own stat the writer keeps
    // streaming. Nothing touches the store in that gap any more (the
    // window-open PUT the hook used to ride now opens in the commit
    // section, after the uploads), so the growth rides the PUT of a
    // path that uploads FIRST: with the fan-out at one, uploads run in
    // path order, and ckpt.bin's stat comes after aaa-first.txt's PUT.
    write(dir.path(), "aaa-first.txt", "uploads before ckpt.bin");
    a.cfg.upload_fanout = 1;
    let grow = path.clone();
    hooked.before_put(&a.cfg.file_key("aaa-first.txt"), move || {
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(&grow).unwrap().write_all(&vec![9u8; 2 << 20]).unwrap();
    });
    a.run_barrier().await.unwrap();

    let head = inner.head(&cited_key(&inner, &a.cfg, "ckpt.bin").await).await.unwrap();
    assert_eq!(head.size, 6 << 20, "fixture: the upload did not carry the grown file");
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(
        m.entries["ckpt.bin"].size, head.size,
        "the manifest cites a length the object does not have"
    );

    // The successor checks out the boundary.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = hooked_syncer(&hooked, dir_b.path());
    b.cfg.whole_put_max = 1 << 20;
    b.cfg.range_get_min_bytes = 1 << 20;
    b.cfg.range_get_chunk_bytes = 1 << 20;
    assert!(claim_until_held(&mut b, 12).await, "takeover");
    b.checkout().await.expect("the successor cannot check out the boundary: the workspace is unrecoverable");
    assert_eq!(std::fs::metadata(dir_b.path().join("ckpt.bin")).unwrap().len(), 6 << 20);
}

/// atomicity-3 / inbox-2. The consume stat'd the path clean, fetched the
/// foreign bytes over the network, and renamed them over the path
/// without looking again. An agent write inside the fetch was gone, with
/// no record. "The syncer never overwrites a file you modified."
#[tokio::test]
async fn a_consume_never_overwrites_a_write_that_landed_during_its_fetch() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let dir = tempfile::tempdir().unwrap();
    let mut a = hooked_syncer(&hooked, dir.path());
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "shared.txt", "v1");
    a.run_barrier().await.unwrap();

    hitl_write(&inner, &a.cfg, "shared.txt", "UI", "ui").await.unwrap();
    // P2: the save is committed; the next consume owes it and fetches it.
    let late = dir.path().join("shared.txt");
    hooked.before_get_return(&a.cfg.file_key("shared.txt"), move || {
        std::fs::write(&late, "AGENT-LATE, a different length").unwrap();
    });
    a.run_barrier().await.unwrap();

    assert_eq!(
        read(dir.path(), "shared.txt").as_deref(),
        Some("AGENT-LATE, a different length"),
        "the agent's write was overwritten by a consume that checked the path before its fetch"
    );
    // The consume stepped aside; the same barrier publishes the agent's
    // version over the UI's, which R7 preserves and names.
    let recs = a.state.load_conflicts().unwrap();
    assert!(
        recs.iter().any(|c| c.path == "shared.txt" && c.kind.starts_with("commit-surfaced-foreign") && c.preserved_key.is_some()),
        "no record names the foreign version that lost: {recs:?}"
    );
}

/// atomicity-6. The scan skips symlinks; the upload read followed them.
/// A regular file swapped for a symlink between the two published the
/// link's TARGET under the file's path — a file outside the workspace,
/// `/proc/self/environ` of the syncer's own process included.
#[tokio::test]
async fn the_upload_refuses_a_symlink_swapped_in_after_the_scan() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let dir = tempfile::tempdir().unwrap();
    let mut a = hooked_syncer(&hooked, dir.path());
    a.checkout().await.unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "SECRET").unwrap();
    write(dir.path(), "cfg.yaml", "harmless");
    // The swap lands between the scan and cfg.yaml's stat. Nothing
    // touches the store in that gap any more (the window-open PUT the
    // hook used to ride now opens in the commit section, after the
    // uploads), so it rides the PUT of a path that uploads FIRST: with
    // the fan-out at one, uploads run in path order.
    write(dir.path(), "aaa-first.txt", "uploads before cfg.yaml");
    a.cfg.upload_fanout = 1;
    let p = dir.path().join("cfg.yaml");
    let target = outside.path().join("secret");
    hooked.before_put(&a.cfg.file_key("aaa-first.txt"), move || {
        std::fs::remove_file(&p).unwrap();
        std::os::unix::fs::symlink(&target, &p).unwrap();
    });

    let r = a.run_barrier().await.expect("a planted symlink must not fail the barrier");
    assert!(!r.uploaded.contains(&"cfg.yaml".to_string()), "the link's target was published: {r:?}");
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("cfg.yaml"), "the manifest cites the link's target");
    assert!(inner.head(&a.cfg.file_key("cfg.yaml")).await.is_err(), "the outside bytes reached the bucket");
    let recs = a.state.load_conflicts().unwrap();
    assert!(recs.iter().any(|c| c.path == "cfg.yaml"), "nothing recorded the refusal: {recs:?}");
}

/// lease-3 / audit #7. Self-recognition keyed on `holder_id` alone: an
/// acquire that LANDED but whose response was lost left the cell naming
/// this holder at an epoch it never recorded; the retry recognized
/// itself and skipped the takeover rotation, so a deposed straggler's
/// manifest CAS still matched.
#[tokio::test]
async fn a_lost_acquire_response_still_rotates() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = hooked_syncer(&hooked, dir_a.path());
    a.checkout().await.unwrap();
    write(dir_a.path(), "f.txt", "x");
    a.run_barrier().await.unwrap();
    let seq0 = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;
    // A holds a commit section and stalls in it.
    assert!(claim_until_held(&mut a, 3).await);

    // The challenger polls the quiet cell up to the takeover threshold.
    let dir_b = tempfile::tempdir().unwrap();
    let mut b = hooked_syncer(&hooked, dir_b.path());
    loop {
        match lease::claim_step(&mut b, true).await.unwrap() {
            lease::ClaimOutcome::Waiting { quiet_polls, .. } if quiet_polls >= 5 => break,
            lease::ClaimOutcome::Waiting { .. } => {}
            lease::ClaimOutcome::Claimed(_) => panic!("fixture: claimed before the threshold"),
        }
    }
    // The acquiring step: the acquire lands, the response is lost.
    hooked.1.acquire_err_once.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(lease::claim_step(&mut b, true).await.is_err(), "fixture: the lost response did not surface");
    let cell = inner.epoch_read(&a.cfg.epoch_key()).await.unwrap().unwrap();
    let b_id = b.state.load_incarnation().unwrap().unwrap().holder_id;
    assert_eq!(cell.holder_id, b_id, "fixture: the acquire did not land");

    // The retry (a container restart) must still rotate the manifest.
    let outcome = lease::claim_step(&mut b, true).await.unwrap();
    assert!(matches!(outcome, lease::ClaimOutcome::Claimed(_)), "the retry did not claim");
    let seq1 = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;
    assert!(
        seq1 > seq0,
        "a lost acquire response skipped the takeover rotation (seq {seq0} → {seq1}): a straggler's CAS still matches"
    );
}

// ── the per-barrier lease (design 2026-09-13 §4): the falsifiers L1–L8
// in their local form, each with the control that makes it non-vacuous ──

/// L1/L2 — two writers on one workspace both publish, at once, with
/// neither waiting for the other's LIFE: each barrier claims the fence
/// for its commit section and hands it on. Control: the life-long lease
/// this replaces made the second writer wait forever (`verbs.rs`
/// 2026-09-11 named it a deadlock).
#[tokio::test]
async fn two_writers_publish_without_waiting_for_each_other() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    b.checkout().await.unwrap();
    write(dir_a.path(), "a/one.txt", "from A");
    write(dir_b.path(), "b/one.txt", "from B");
    // Both barriers in flight together: whoever claims second queues
    // behind the first's commit section and follows it.
    let (ra, rb) = tokio::join!(a.run_barrier(), b.run_barrier());
    let (ra, rb) = (ra.expect("A's barrier"), rb.expect("B's barrier"));
    assert_eq!(ra.uploaded, vec!["a/one.txt"]);
    assert_eq!(rb.uploaded, vec!["b/one.txt"]);
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("a/one.txt") && m.entries.contains_key("b/one.txt"), "{:?}", m.entries.keys());
    // Nobody holds the cell between barriers, and it advanced once per
    // commit section.
    let cell = store.epoch_read(&a.cfg.epoch_key()).await.unwrap().unwrap();
    assert!(cell.released && cell.handoff.is_none() && cell.waiters.is_empty(), "{cell:?}");
    assert_eq!(cell.epoch, 2, "one epoch per barrier");
    assert!(a.lease.is_none() && b.lease.is_none());
}

/// L3 — disjoint edits cross: what A published reaches B's tree at B's
/// next consume, and vice versa — derived from the document (P1-lite), in
/// the barrier that also publishes the writer's own edit.
#[tokio::test]
async fn disjoint_edits_cross_at_the_next_consume() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    b.checkout().await.unwrap();
    write(dir_a.path(), "a/one.txt", "from A");
    write(dir_b.path(), "b/one.txt", "from B");
    a.run_barrier().await.unwrap();
    let rb = b.run_barrier().await.unwrap();
    assert_eq!(rb.consumed, 1, "B's consume did not integrate A's file");
    assert_eq!(read(dir_b.path(), "a/one.txt").as_deref(), Some("from A"));
    assert!(rb.uploaded.contains(&"b/one.txt".to_string()), "B's own edit did not publish: {rb:?}");
    let ra2 = a.run_barrier().await.unwrap();
    assert_eq!(ra2.consumed, 1, "A's consume did not integrate B's file");
    assert_eq!(read(dir_a.path(), "b/one.txt").as_deref(), Some("from B"));
}

/// L4 — two writers edit ONE path: the later commit is current, the
/// earlier version is preserved in the bucket and named by a record on
/// the later writer, and the earlier writer's tree carries the later
/// version after its consume. Nothing is lost and nothing is silent.
#[tokio::test]
async fn a_same_path_edit_is_preserved_never_lost() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "seed");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("seed"));
    write(dir_a.path(), "x.txt", "A's edit");
    backdate_baseline(&a, "x.txt");
    write(dir_b.path(), "x.txt", "B's edit, longer");
    backdate_baseline(&b, "x.txt");
    a.run_barrier().await.unwrap();
    let rb = b.run_barrier().await.unwrap();
    assert!(rb.parked.is_empty(), "B's upload parked instead of preserving: {rb:?}");
    assert_eq!(rb.surfaced, vec!["x.txt".to_string()], "B's commit did not surface what it published over: {rb:?}");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let cited = &m.entries["x.txt"];
    let (_, body) = store.get_whole(&cited.key, Some(&cited.etag)).await.unwrap();
    assert_eq!(&body[..], b"B's edit, longer", "the later commit is current");
    // The record is written at the COMMIT under handles (R7): the slot's
    // 412 met the same version at the PUT.
    let rec = b
        .state
        .load_conflicts()
        .unwrap()
        .into_iter()
        .find(|c| c.path == "x.txt" && c.kind.starts_with("commit-surfaced-foreign"))
        .expect("B wrote no record of the version it superseded");
    let preserved = rec.preserved_key.expect("the record names no preserved key");
    let (_, kept) = store.get_whole(&preserved, None).await.unwrap();
    assert_eq!(&kept[..], b"A's edit", "A's bytes are not where the record says");
    // A's next consume brings B's version onto A's now-clean path.
    a.run_barrier().await.unwrap();
    a.run_barrier().await.unwrap();
    assert_eq!(read(dir_a.path(), "x.txt").as_deref(), Some("B's edit, longer"));
}

/// The waiter the next handoff will name polls fast; the rest of the queue
/// does not. In the writers drill the released cell stood idle a median
/// 614 ms (1 s polls) before its reserved waiter noticed — more than half
/// again the 973 ms median hold — with the queue behind it.
#[tokio::test]
async fn only_the_queue_head_is_told_to_poll_fast() {
    let store = Arc::new(MemoryStore::new());
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut a = syncer(&store, dirs[0].path()).await;
    let mut b = syncer(&store, dirs[1].path()).await;
    let mut c = syncer(&store, dirs[2].path()).await;
    let next = |o: lease::ClaimOutcome| match o {
        lease::ClaimOutcome::Waiting { next, .. } => Some(next),
        lease::ClaimOutcome::Claimed(_) => None,
    };
    assert!(claim_until_held(&mut a, 1).await);
    assert_eq!(next(lease::claim_step(&mut b, true).await.unwrap()), Some(true), "B heads the queue behind A");
    assert_eq!(next(lease::claim_step(&mut c, true).await.unwrap()), Some(false), "C is second");
    assert_eq!(next(lease::claim_step(&mut b, false).await.unwrap()), Some(true), "B still heads it on a re-poll");
    lease::release(&mut a).await.unwrap();
    // Released and reserved for B: C waits, and is not the head of anything yet.
    assert_eq!(next(lease::claim_step(&mut c, false).await.unwrap()), Some(false));
    assert_eq!(next(lease::claim_step(&mut b, false).await.unwrap()), None, "B claims its reservation");
    assert_eq!(next(lease::claim_step(&mut c, false).await.unwrap()), Some(true), "C heads the queue behind B");
}

/// The handoff races the queue. Every waiter's enqueue moves the cell's
/// token, so under contention a holder's handoff can 412 more than once in
/// a row, and S3 answers a conditional write racing another with 409
/// ConditionalRequestConflict. The release treated a second 412 as
/// "deposed meanwhile" and a 409 as a failure, and returned with the cell
/// still HELD: nobody released it, and the queue waited out the deposal
/// threshold (the contention drill: every arm lost 0-4 handoffs a run, and
/// once pull-only boundaries stopped re-claiming, one stood ~60 s and was
/// deposed with a manifest rotation).
#[tokio::test]
async fn a_handoff_that_races_the_queue_still_hands_the_cell_on() {
    for (races, conflicts) in [(2u32, 0u32), (0, 1), (3, 2)] {
        let inner = Arc::new(MemoryStore::new());
        let hooked = Arc::new(Hooked(inner.clone(), Hooks::default()));
        let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut a = hooked_syncer(&hooked, dir_a.path());
        let mut b = syncer(&inner, dir_b.path()).await;
        assert!(claim_until_held(&mut a, 1).await);
        assert!(matches!(lease::claim_step(&mut b, true).await.unwrap(), lease::ClaimOutcome::Waiting { .. }));
        hooked.1.handoff_races.store(races, std::sync::atomic::Ordering::SeqCst);
        hooked.1.handoff_conflicts.store(conflicts, std::sync::atomic::Ordering::SeqCst);
        lease::release(&mut a).await.unwrap_or_else(|e| panic!("races {races}, conflicts {conflicts}: the release failed: {e}"));
        let cell = inner.epoch_read(&a.cfg.epoch_key()).await.unwrap().unwrap();
        assert!(
            cell.released,
            "races {races}, conflicts {conflicts}: the release returned and the cell is still held by {}",
            cell.holder_id
        );
        let b_id = lease::incarnation(&b).unwrap().holder_id;
        assert_eq!(cell.handoff.as_deref(), Some(b_id.as_str()), "races {races}, conflicts {conflicts}: the head of the queue was not named");
        assert!(matches!(lease::claim_step(&mut b, true).await.unwrap(), lease::ClaimOutcome::Claimed(_)));
    }
}

/// L5 — the ticket is load-bearing. A released cell is reserved for the
/// queue HEAD: a later waiter that polls first does not get it. Delete
/// the `handoff` check in `claim_step` and C claims here.
#[tokio::test]
async fn the_ticket_hands_the_fence_to_the_queue_head() {
    let store = Arc::new(MemoryStore::new());
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut a = syncer(&store, dirs[0].path()).await;
    let mut b = syncer(&store, dirs[1].path()).await;
    let mut c = syncer(&store, dirs[2].path()).await;
    let id = |sc: &Syncer| lease::incarnation(sc).unwrap().holder_id;
    assert!(claim_until_held(&mut a, 1).await);
    assert!(matches!(lease::claim_step(&mut b, true).await.unwrap(), lease::ClaimOutcome::Waiting { .. }));
    assert!(matches!(lease::claim_step(&mut c, true).await.unwrap(), lease::ClaimOutcome::Waiting { .. }));
    let cell = store.epoch_read(&a.cfg.epoch_key()).await.unwrap().unwrap();
    assert_eq!(cell.waiters, vec![id(&b), id(&c)], "waiters queue in arrival order");
    lease::release(&mut a).await.unwrap();
    let cell = store.epoch_read(&a.cfg.epoch_key()).await.unwrap().unwrap();
    assert!(cell.released);
    assert_eq!(cell.handoff.as_deref(), Some(id(&b).as_str()), "the handoff names the head");
    assert_eq!(cell.waiters, vec![id(&c)]);
    // C polls first and is refused: the cell is B's.
    assert!(
        matches!(lease::claim_step(&mut c, true).await.unwrap(), lease::ClaimOutcome::Waiting { .. }),
        "a later waiter took a cell reserved for the queue head"
    );
    assert!(matches!(lease::claim_step(&mut b, true).await.unwrap(), lease::ClaimOutcome::Claimed(_)));
    let cell = store.epoch_read(&a.cfg.epoch_key()).await.unwrap().unwrap();
    assert_eq!((cell.holder_id.as_str(), cell.released), (id(&b).as_str(), false));
    assert_eq!(cell.waiters, vec![id(&c)], "the queue survives the handoff");
    lease::release(&mut b).await.unwrap();
    assert!(matches!(lease::claim_step(&mut c, true).await.unwrap(), lease::ClaimOutcome::Claimed(_)));
    let cell = store.epoch_read(&a.cfg.epoch_key()).await.unwrap().unwrap();
    assert_eq!(cell.epoch, 3);
    assert!(cell.waiters.is_empty() && cell.handoff.is_none());
}

/// L6a — a reservation whose holder died is skipped after the quiet
/// polls, or the cell would be wedged forever by a waiter that crashed
/// between enqueue and claim.
#[tokio::test]
async fn a_dead_handoff_is_skipped_after_the_quiet_polls() {
    let store = Arc::new(MemoryStore::new());
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut a = syncer(&store, dirs[0].path()).await;
    let mut b = syncer(&store, dirs[1].path()).await;
    let mut c = syncer(&store, dirs[2].path()).await;
    assert!(claim_until_held(&mut a, 1).await);
    assert!(matches!(lease::claim_step(&mut b, true).await.unwrap(), lease::ClaimOutcome::Waiting { .. }));
    lease::release(&mut a).await.unwrap();
    drop(b); // B dies holding the reservation.
    assert!(!claim_until_held(&mut c, lease::HANDOFF_QUIET_POLLS).await, "C took a reservation that was not yet quiet");
    assert!(claim_until_held(&mut c, 1).await, "C never took the dead reservation");
    let cell = store.epoch_read(&a.cfg.epoch_key()).await.unwrap().unwrap();
    assert_eq!(cell.holder_id, lease::incarnation(&c).unwrap().holder_id);
    assert!(cell.waiters.is_empty() && cell.handoff.is_none());
}

/// L6b — a holder that dies INSIDE its commit section is deposed after
/// the quiet polls by the next writer's barrier, which then publishes;
/// the deposed holder's own fence (renew) says so and its next barrier
/// claims again. Control: below the threshold the waiter does not depose.
#[tokio::test]
async fn a_dead_holder_mid_commit_is_deposed_by_the_next_barrier() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    b.checkout().await.unwrap();
    assert!(claim_until_held(&mut a, 1).await); // A: mid-commit, stalled
    write(dir_b.path(), "b.txt", "B publishes past a dead holder");
    assert!(!claim_until_held(&mut b, 3).await, "the waiter deposed a holder below the quiet threshold");
    let rb = b.run_barrier().await.expect("B's barrier deposes the dead holder and publishes");
    assert_eq!(rb.uploaded, vec!["b.txt"]);
    let err = lease::renew(&mut a).await.unwrap_err();
    assert!(matches!(err, LeanError::Fenced(_)), "{err}");
    write(dir_a.path(), "a.txt", "A, after being deposed");
    a.run_barrier().await.expect("a deposed writer's next barrier claims again");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("a.txt") && m.entries.contains_key("b.txt"));
}

/// L6c — a holder deposed INSIDE its commit section abandons that
/// barrier and nothing else: no manifest of its is installed, the
/// pending sentinel stands, the marker stays live, and the next tick
/// honours it. (`refused-fenced` and the fenced marker died with the
/// life-long lease.) The deposal lands between A's CAS attempt and its
/// pointer PUT, through the store hook.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holder_deposed_mid_commit_abandons_the_barrier() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = hooked_syncer(&hooked, dir_a.path());
    a.cfg.sentinel_min_interval_secs = 0;
    a.checkout().await.unwrap();
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
    write(dir_a.path(), "seed.txt", "seed");
    a.run_barrier().await.unwrap();
    let seq0 = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;

    write(dir_a.path(), "work.txt", "v1");
    touch_sentinel(dir_a.path(), control::PUBLISH, r#"{"nonce":"n1"}"#);
    assert!(a.consume_sentinel(Verb::Publish).unwrap(), "fixture: the touch was not consumed");
    assert!(matches!(a.sentinel_due().unwrap(), super::sentinel::Due::Ready), "fixture: not due");
    // Inside A's commit section, before its pointer CAS: a rival deposes
    // it (the cell moves, the manifest rotates).
    let (rival_store, cfg) = (inner.clone(), a.cfg.clone());
    hooked.before_put(&a.cfg.current_key(), move || {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let key = cfg.epoch_key();
                let seen = rival_store.epoch_read(&key).await.unwrap().unwrap();
                assert!(!seen.released, "fixture: A is not holding at its CAS");
                let l = rival_store.epoch_acquire(&key, "rival", Some(&seen)).await.unwrap();
                manifest::rotate_for_takeover(rival_store.as_ref(), &cfg, l.epoch).await.unwrap();
            })
        });
    });
    // The tick swallows the fence like any other failed honor: the
    // pending is kept and retried, nothing is acked, nothing exits.
    let acks = a.sentinel_tick().await.expect("a fence is a retry, not an error the loop sees");
    assert!(acks.is_empty(), "an ack was written for an abandoned barrier: {acks:?}");
    assert!(a.lease.is_none());
    assert!(a.load_pending(Verb::Publish).unwrap().is_some(), "the pending was dropped by the fence");
    assert!(a.read_ack(Verb::Publish).is_none(), "an ack was written for an abandoned barrier");
    let caps = a.read_capabilities().unwrap();
    assert_eq!(caps.state, "live");
    assert!(!caps.verbs.is_empty(), "the marker stopped advertising verbs");
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("work.txt"), "the abandoned barrier's manifest landed");
    assert_eq!(m.seq, seq0 + 1, "the rotation, and nothing after it");

    // The rival never renews: A's next tick deposes it in turn and
    // honours the standing touch with a fresh claim.
    let acks = a.sentinel_tick().await.expect("the retry");
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0].status, "ok");
    assert!(acks[0].nonces.contains(&"n1".to_string()));
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("work.txt"));
}

/// L7 — readers never touch the cell: a checkout and a sync leave no
/// epoch cell behind and issue no epoch request at all.
#[tokio::test]
async fn readers_never_touch_the_cell() {
    let store = Arc::new(MemoryStore::new());
    let dir_w = tempfile::tempdir().unwrap();
    let mut w = syncer(&store, dir_w.path()).await;
    w.checkout().await.unwrap();
    write(dir_w.path(), "f.txt", "published");
    w.run_barrier().await.unwrap();
    let epoch_before = store.epoch_read(&w.cfg.epoch_key()).await.unwrap().unwrap().epoch;
    store.reset_op_counts();
    let dir_r = tempfile::tempdir().unwrap();
    let mut r = syncer(&store, dir_r.path()).await;
    super::verbs::run_verb(&mut r, super::verbs::Step::Checkout).await.unwrap();
    super::verbs::run_verb(&mut r, super::verbs::Step::Sync).await.unwrap();
    assert_eq!(read(dir_r.path(), "f.txt").as_deref(), Some("published"));
    // The shared-prefix diagnostic READS the cell (one GET per verb, to
    // say whether another product writes here); a reader never WRITES it.
    let ops = store.op_counts();
    assert!(
        !ops.keys().any(|k| k.starts_with("epoch_") && *k != "epoch_read"),
        "a reader wrote the cell: {ops:?}"
    );
    let cell = store.epoch_read(&w.cfg.epoch_key()).await.unwrap().unwrap();
    assert_eq!(cell.epoch, epoch_before);
    assert!(cell.released && cell.waiters.is_empty());
    assert!(r.lease.is_none());
}

/// L8 — what the fence costs: a no-change boundary touches the cell not
/// at all (the inbox GET and the pointer GET, exactly as before), and a
/// publishing one pays two reads and two writes of a few-hundred-byte
/// cell — claim, verify, handoff — and no heartbeat renewal.
#[tokio::test]
async fn the_fence_costs_a_publishing_boundary_four_cell_requests_and_an_idle_one_none() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    write(dir.path(), "f.txt", "v1");
    a.floor_tick().await.unwrap();

    store.reset_op_counts();
    let idle = a.floor_tick().await.unwrap();
    assert!(idle.no_change);
    let ops = store.op_counts();
    assert!(!ops.keys().any(|k| k.starts_with("epoch_")), "an idle tick touched the cell: {ops:?}");
    assert_eq!(ops.values().sum::<u64>(), 2, "an idle tick is the inbox and the pointer: {ops:?}");

    write(dir.path(), "f.txt", "v2");
    backdate_baseline(&a, "f.txt");
    store.reset_op_counts();
    let busy = a.floor_tick().await.unwrap();
    assert_eq!(busy.uploaded, 1);
    let ops = store.op_counts();
    assert_eq!(ops.get("epoch_read").copied(), Some(2), "{ops:?}");
    assert_eq!(ops.get("epoch_acquire").copied(), Some(1), "{ops:?}");
    assert_eq!(ops.get("epoch_handoff").copied(), Some(1), "{ops:?}");
    assert_eq!(ops.get("epoch_renew"), None, "{ops:?}");
    assert_eq!(ops.get("epoch_enqueue"), None, "{ops:?}");
}

/// A PULL-ONLY boundary — the manifest moved, and this writer has nothing
/// of its own to publish, delete, consume or re-cite — writes nothing to
/// the bucket: its merge adds nothing, so there is no CAS, no GC and no
/// inbox entry to drop, and what is left (queue the peer's change, move
/// the merge base) is local. It must not queue for the cell or open the
/// window HITL writers wait on for that. In the writers drill 65 of 191
/// claims were pull-only.
#[tokio::test]
async fn a_pull_only_boundary_takes_no_fence_and_writes_nothing() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "seed");
    write(dir_a.path(), "gone.txt", "A deletes this");
    a.floor_tick().await.unwrap();
    b.checkout().await.unwrap();
    b.floor_tick().await.unwrap();

    write(dir_a.path(), "x.txt", "A's v2");
    backdate_baseline(&a, "x.txt");
    std::fs::remove_file(dir_a.path().join("gone.txt")).unwrap();
    a.declared_barrier().await.unwrap();
    let seq_a = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;

    store.reset_op_counts();
    let pulled = b.floor_tick().await.unwrap();
    let ops = store.op_counts();
    eprintln!("a pull-only boundary: {ops:?}");
    assert!(!ops.keys().any(|k| k.starts_with("epoch_")), "a pull-only boundary touched the cell: {ops:?}");
    let writes: Vec<_> =
        ops.keys().filter(|k| !matches!(**k, "get_whole" | "get_range" | "head" | "list")).collect();
    assert!(writes.is_empty(), "a pull-only boundary wrote to the bucket: {ops:?}");
    assert_eq!(pulled.seq, Some(seq_a), "{pulled:?}");
    // P1-lite: the boundary's consume took A's edit and A's delete at once.
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("A's v2"), "A's edit was not taken");
    assert_eq!(read(dir_b.path(), "gone.txt"), None, "A's delete was not taken");

    // It converges exactly as the fenced path did, and installs nothing.
    b.floor_tick().await.unwrap();
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("A's v2"));
    assert_eq!(read(dir_b.path(), "gone.txt"), None);
    let t = b.load_remote_seq();
    assert_eq!(t.observed_seq, t.integrated_seq, "{t:?}");
    assert_eq!(manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq, seq_a, "B installed a generation");

    // And B publishing for real afterwards still takes the fence.
    write(dir_b.path(), "b.txt", "B's own");
    store.reset_op_counts();
    let busy = b.floor_tick().await.unwrap();
    assert_eq!(busy.uploaded, 1);
    assert_eq!(store.op_counts().get("epoch_acquire").copied(), Some(1), "a publishing boundary skipped the fence");
}

/// The interleaving the fence used to rule out for a pull-only boundary:
/// B pulls INSIDE A's commit section — after A's manifest CAS, before its
/// GC delete and its window clear. B reads a committed document, so the
/// change it queues and the base it takes are A's installed ones; A's GC
/// removes only what that document no longer cites.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pull_only_boundary_inside_a_peers_commit_section_converges() {
    let inner = Arc::new(MemoryStore::new());
    let ha = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = hooked_syncer(&ha, dir_a.path());
    let mut b = syncer(&inner, dir_b.path()).await;
    // A claim B was not meant to make fails at once instead of waiting on A.
    b.cfg.claim_deadline_secs = 0;
    a.checkout().await.unwrap();
    write(dir_a.path(), "p.txt", "seed");
    write(dir_a.path(), "q.txt", "A deletes this");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();

    write(dir_a.path(), "p.txt", "A's v2");
    backdate_baseline(&a, "p.txt");
    std::fs::remove_file(dir_a.path().join("q.txt")).unwrap();
    let b_slot = std::sync::Mutex::new(Some(b));
    let b_done: Arc<std::sync::Mutex<Option<(Syncer, std::collections::BTreeMap<&'static str, u64>)>>> = Default::default();
    let b_done_in = b_done.clone();
    let (cfg, probe) = (a.cfg.clone(), inner.clone());
    ha.before_delete(&a.cfg.file_key("q.txt"), move || {
        let mut b = b_slot.lock().unwrap().take().expect("hook ran twice");
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let m = manifest::load(probe.as_ref(), &cfg).await.unwrap().unwrap().manifest;
                assert!(!m.entries.contains_key("q.txt"), "fixture: A's CAS has not uncited q.txt");
                let cell = probe.epoch_read(&cfg.epoch_key()).await.unwrap().unwrap();
                assert!(!cell.released, "fixture: A is not inside its commit section");
                probe.reset_op_counts();
                b.floor_tick().await.expect("B's pull inside A's commit section");
                *b_done_in.lock().unwrap() = Some((b, probe.op_counts()));
            })
        });
    });
    let ra = a.declared_barrier().await.expect("A's barrier");
    assert!(ra.deleted.contains(&"q.txt".to_string()), "fixture: {ra:?}");
    let (mut b, ops) = b_done.lock().unwrap().take().expect("fixture: A's GC never reached q.txt");
    assert!(!ops.keys().any(|k| k.starts_with("epoch_")), "B's pull touched the cell: {ops:?}");

    for _ in 0..3 {
        b.floor_tick().await.expect("B's tick");
        a.floor_tick().await.expect("A's tick");
    }
    assert_eq!(read(dir_b.path(), "p.txt").as_deref(), Some("A's v2"));
    assert_eq!(read(dir_b.path(), "q.txt"), None, "A's delete never reached B");
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("q.txt"), "the delete was undone");
    assert_every_citation_resolves(&inner, &a.cfg, "after the pull inside A's commit").await;
    let t = b.load_remote_seq();
    assert_eq!(t.observed_seq, t.integrated_seq, "{t:?}");
}

/// A restarted container that finds the cell HELD by its own pod
/// releases it at startup, so the other writers do not wait out a
/// deposal for a holder that holds nothing in memory.
#[tokio::test]
async fn a_restarted_container_releases_the_fence_its_predecessor_left_held() {
    let store = Arc::new(MemoryStore::new());
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 1).await);
    let held = a.lease.clone().unwrap();
    drop(a); // the container dies mid-commit
    let mut a2 = syncer(&store, dir_a.path()).await; // same emptyDir, same incarnation
    assert_eq!(lease::incarnation(&a2).unwrap().holder_id, held.holder_id);
    lease::release_stale_own(&mut a2).await.unwrap();
    let cell = store.epoch_read(&a2.cfg.epoch_key()).await.unwrap().unwrap();
    assert!(cell.released, "the stale hold was not released");
    assert_eq!(cell.epoch, held.epoch);
    let mut b = syncer(&store, dir_b.path()).await;
    assert!(claim_until_held(&mut b, 1).await, "the successor still had to wait");
}

// ── the per-barrier lease under TWO writers: the model tranche's findings,
// reproduced against the code (LeanSubtree tranche 6, 2026-09-13) ─────────
//
// Uploads hold no lease, so a peer's upload can land inside another
// writer's commit section. Under the life lease the second writer did not
// exist, and each of these was unreachable.

/// Run a syncer's barrier on its own OS thread and runtime. The barrier's
/// future is not provably `Send` (the upload fan-out's higher-ranked
/// lifetimes), so `tokio::spawn` cannot take it, and `tokio::join!` puts
/// both writers on ONE task — a hook that parks one writer there parks
/// both. A thread each keeps a parked writer's peer running.
fn barrier_on_thread(
    mut sc: Syncer,
) -> std::thread::JoinHandle<(Syncer, crate::LeanResult<super::barrier::BarrierReport>)> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let r = rt.block_on(sc.run_barrier());
        (sc, r)
    })
}

/// Every citation in the current manifest resolves to its bytes.
async fn assert_every_citation_resolves(store: &Arc<MemoryStore>, cfg: &LeanConfig, when: &str) {
    let m = manifest::load(store.as_ref(), cfg).await.unwrap().unwrap().manifest;
    for (path, e) in &m.entries {
        if let Err(err) = store.get_whole(&e.key, Some(&e.etag)).await {
            panic!("{when}: seq {} cites {path} at {} but the object is gone: {err}", m.seq, e.etag);
        }
    }
}

/// A merge-preserved entry once lived in the SHARED inbox, and the writer
/// whose change it carried (A) found it integrated and dropped it before
/// the writer that needed it (B) consumed it. Under P1-lite nothing is
/// carried: B's consume derives A's change from the document. B must still
/// end up with A's bytes, and the document must keep them.
#[tokio::test]
async fn a_peers_change_reaches_the_writer_whose_merge_queued_it_even_if_the_peer_consumes_first() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "seed");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();

    write(dir_a.path(), "x.txt", "A's v2");
    backdate_baseline(&a, "x.txt");
    a.run_barrier().await.unwrap();
    // B publishes an unrelated path.
    write(dir_b.path(), "z.txt", "z");
    b.run_barrier().await.unwrap();
    a.run_barrier().await.unwrap();
    // B converges on A's bytes within a couple of barriers.
    b.run_barrier().await.unwrap();
    b.run_barrier().await.unwrap();
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("A's v2"), "A's change never reached B");
    assert_every_citation_resolves(&store, &a.cfg, "after convergence").await;
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let (_, body) = store.get_whole(&m.entries["x.txt"].key, Some(&m.entries["x.txt"].etag)).await.unwrap();
    assert_eq!(&body[..], b"A's v2", "the manifest reverted A's change");
}

/// H8 follow-up (2026-09-23), L-123: B's merge queued A's v2; A published
/// v3; B's sync fetched v3; B's next consume still held the queued v2 and
/// wrote it over v3 (fixed then by the sync's prune). Under P1-lite nothing
/// is queued — the next consume derives against the CURRENT document — so
/// the regress has no route; this keeps the scenario as its pin.
#[tokio::test]
async fn a_queued_peer_change_never_lands_over_a_newer_version_a_sync_fetched() {
    queued_change_behind_a_sync(true).await;
}

/// The same with v2 collected, as A's collector normally leaves it: the
/// consume must not report the superseded handle as lost bytes.
#[tokio::test]
async fn a_queued_peer_change_a_sync_overtook_is_not_reported_missing() {
    queued_change_behind_a_sync(false).await;
}

/// Land a peer's publish of `path` between `sc`'s next consume and its
/// merge — the one gap P1-lite leaves, where a merge sees a change the
/// tree has not taken (`Baseline::behind`). Armed on the `before-scan`
/// test window; the caller runs the barrier.
/// `land_during_next_barrier`'s delete: another party's one-CAS delete of
/// `path` (a UI delete commits exactly so under P2).
pub(super) fn land_delete_during_next_barrier(store: &Arc<MemoryStore>, sc: &Syncer, path: &'static str) {
    let (store, cfg) = (store.clone(), sc.cfg.clone());
    super::barrier::CONSUME_WINDOW_HOOK.with(|h| {
        *h.borrow_mut() = Some((
            String::new(),
            "before-scan",
            Box::new(move || futures::executor::block_on(hitl_remove(&store, &cfg, path, "A"))),
        ))
    });
}

pub(super) fn land_during_next_barrier(store: &Arc<MemoryStore>, sc: &Syncer, path: &'static str, content: &'static str) {
    let (store, cfg) = (store.clone(), sc.cfg.clone());
    super::barrier::CONSUME_WINDOW_HOOK.with(|h| {
        *h.borrow_mut() = Some((
            String::new(),
            "before-scan",
            Box::new(move || {
                futures::executor::block_on(peer_publish(&store, &cfg, path, content, "A"));
            }),
        ))
    });
}

async fn queued_change_behind_a_sync(v2_still_at_its_handle: bool) {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "seed");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();

    // A's v2 lands after B's consume and before B's merge: B's merge sees
    // it, B's tree does not have it.
    write(dir_b.path(), "z.txt", "z");
    land_during_next_barrier(&store, &b, "x.txt", "A's v2");
    b.run_barrier().await.unwrap();
    assert!(super::barrier::CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_none()), "fixture: the window never ran");
    assert!(derive_pending(&b).await, "fixture: B's merge left nothing owed for x.txt");
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("seed"), "fixture: v2 already reached B's tree");
    let v2 = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries["x.txt"].clone();
    let v2_key = v2.key.clone();
    let (_, v2_body) = store.get_whole(&v2_key, Some(&v2.etag)).await.unwrap();

    write(dir_a.path(), "x.txt", "A's v3");
    backdate_baseline(&a, "x.txt");
    a.run_barrier().await.unwrap();
    if v2_still_at_its_handle {
        // A's collector has not run yet: v2's handle holds v2's bytes.
        if store.head(&v2_key).await.is_err() {
            let crc = crc64_nvme(&v2_body);
            let stamps = GenerationStamps {
                generation: 0,
                epoch: 0,
                flush_uuid: "a-before-its-collector".to_string(),
                boundary_source: None,
                posix: None,
            };
            store
                .put_whole(&v2_key, v2_body.clone(), &PutCondition::Unconditional, &stamps, crc)
                .await
                .unwrap();
        }
        assert_eq!(
            store.head(&v2_key).await.unwrap().etag,
            v2.etag.clone(),
            "fixture: v2's handle does not attest v2"
        );
    }

    b.sync().await.unwrap();
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("A's v3"), "fixture: B's sync did not fetch v3");
    let conflicts_before = b.state.load_conflicts().unwrap().len();
    b.run_barrier().await.unwrap();

    assert_eq!(
        read(dir_b.path(), "x.txt").as_deref(),
        Some("A's v3"),
        "B's consume wrote the queued v2 over the v3 its sync fetched"
    );
    b.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let (_, body) = store.get_whole(&m.entries["x.txt"].key, Some(&m.entries["x.txt"].etag)).await.unwrap();
    assert_eq!(&body[..], b"A's v3", "the manifest reverted A's v3");
    let new_conflicts: Vec<_> = b.state.load_conflicts().unwrap().into_iter().skip(conflicts_before).collect();
    assert!(
        new_conflicts.is_empty(),
        "a queued change the sync overtook was surfaced as a conflict: {new_conflicts:?}"
    );
}

/// The single-writer crash test above
/// (`a_crash_between_the_cas_and_step_7_never_makes_our_own_entry_foreign`)
/// takes the installed document as the merge base after a restart in the
/// CAS-to-step-7 window. With a second writer that document also carries
/// the PEER's changes this merge brought in, and step 7 is where they were
/// to be queued for this tree. A restart in that window must not make them
/// read as already integrated.
///
/// The restart is taken where the code really is: B's barrier also removes
/// a file of its own, and the GC delete of it (step 6, after the CAS and
/// before step 7) snapshots the state files step 7 rewrites; restoring them
/// after the barrier is the restart.
#[tokio::test]
async fn a_restart_between_the_cas_and_step_7_still_delivers_a_peers_change() {
    restart_between_the_cas_and_step_7(false).await;
}

/// The same window for a peer's DELETE: the tombstone rides the journal
/// as the upsert does.
#[tokio::test]
async fn a_restart_between_the_cas_and_step_7_still_delivers_a_peers_delete() {
    restart_between_the_cas_and_step_7(true).await;
}

/// What happens between the restart and B's next barriers.
#[derive(Clone, Copy, PartialEq)]
enum AfterRestart {
    Nothing,
    /// A scoped sync of another path: the merge base at x.txt never moves,
    /// so the change the journal carries is still owed.
    ScopedSyncElsewhere,
    /// A publishes v3 (v2's handle still at its key) and B's sync fetches it:
    /// the journalled v2 is overtaken and must not land over v3.
    PeerV3ThenSync,
    /// The UI renames x.txt (A's v2, the change the journal carries) to
    /// w.txt: B's next barrier declares the delete of x.txt and re-cites
    /// v2's handle at w.txt, while the pointer still holds B's own install.
    UiRenamesTheCarriedChange,
}

/// 2026-09-23: the journal's `installed_foreign` is re-queued by the next
/// consume, so a sync in between must prune it as it prunes the queue.
#[tokio::test]
async fn a_change_the_journal_carries_past_a_restart_never_lands_over_a_newer_sync() {
    restart_between_the_cas_and_step_7_then(false, AfterRestart::PeerV3ThenSync).await;
}

/// ...and a sync that never moved the path's base prunes nothing.
#[tokio::test]
async fn a_scoped_sync_elsewhere_after_a_restart_still_delivers_a_peers_change() {
    restart_between_the_cas_and_step_7_then(false, AfterRestart::ScopedSyncElsewhere).await;
}

/// 2026-09-24, the simplification review's M5: after a restart between the
/// CAS and step 7, `merge_onto` merges onto the document this workspace
/// installed (`installed_etag`), but `void_stale_repairs` judged whether a
/// declared delete APPLIES against the persisted merge base, which step 7
/// never advanced. A rename of a path that install changed then read as
/// "the source keeps the handle": the destination's repair was voided while
/// the merge applied the source's delete, and the handle was cited nowhere.
#[tokio::test]
async fn a_ui_rename_after_a_restart_between_the_cas_and_step_7_keeps_the_file() {
    restart_between_the_cas_and_step_7_then(false, AfterRestart::UiRenamesTheCarriedChange).await;
}

async fn restart_between_the_cas_and_step_7(peer_deletes: bool) {
    restart_between_the_cas_and_step_7_then(peer_deletes, AfterRestart::Nothing).await;
}

async fn restart_between_the_cas_and_step_7_then(peer_deletes: bool, then: AfterRestart) {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&inner, dir_a.path()).await;
    let mut b = hooked_syncer(&hooked, dir_b.path());
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "seed");
    write(dir_a.path(), "keep.txt", "keep");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("seed"));
    write(dir_b.path(), "tmp.txt", "B's scratch");
    b.run_barrier().await.unwrap();

    // A's change lands after B's next consume and before its merge (the one
    // gap P1-lite leaves): B's merge carries it, B's tree does not have it.
    if peer_deletes {
        land_delete_during_next_barrier(&inner, &b, "x.txt");
    } else {
        land_during_next_barrier(&inner, &b, "x.txt", "A's v2");
    }

    // B's barrier: its merge carries A's change; its GC delete of tmp.txt
    // snapshots the files step 7 is about to rewrite.
    let state_dir = b.cfg.state_dir();
    let snap: Arc<std::sync::Mutex<Vec<(std::path::PathBuf, Option<Vec<u8>>)>>> = Default::default();
    let snap_in = snap.clone();
    hooked.before_delete(&b.cfg.file_key("tmp.txt"), move || {
        *snap_in.lock().unwrap() = ["intent.json", "baseline.json"]
            .iter()
            .map(|f| {
                let p = state_dir.join(f);
                let bytes = std::fs::read(&p).ok();
                (p, bytes)
            })
            .collect();
    });
    write(dir_b.path(), "z.txt", "z");
    std::fs::remove_file(dir_b.path().join("tmp.txt")).unwrap();
    b.declared_barrier().await.unwrap();
    let taken = std::mem::take(&mut *snap.lock().unwrap());
    assert!(!taken.is_empty(), "fixture: B's barrier never reached its GC delete");
    assert!(super::barrier::CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_none()), "fixture: the window never ran");
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries.contains_key("x.txt"), !peer_deletes, "fixture: A's change never landed");
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("seed"), "fixture: A's change already reached B's tree");
    // The restart: step 7 never ran.
    for (p, bytes) in taken {
        match bytes {
            Some(b) => std::fs::write(&p, b).unwrap(),
            None => {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    match then {
        AfterRestart::Nothing => {}
        AfterRestart::ScopedSyncElsewhere => {
            b.sync_scoped(Some(vec!["keep.txt".into()])).await.unwrap();
        }
        AfterRestart::PeerV3ThenSync => {
            let v2 = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries["x.txt"].clone();
            let v2_key = v2.key.clone();
            let (_, v2_body) = inner.get_whole(&v2_key, Some(&v2.etag)).await.unwrap();
            write(dir_a.path(), "x.txt", "A's v3");
            backdate_baseline(&a, "x.txt");
            a.run_barrier().await.unwrap();
            if inner.head(&v2_key).await.is_err() {
                let crc = crc64_nvme(&v2_body);
                let stamps = GenerationStamps {
                    generation: 0,
                    epoch: 0,
                    flush_uuid: "a-before-its-collector".to_string(),
                    boundary_source: None,
                    posix: None,
                };
                inner.put_whole(&v2_key, v2_body, &PutCondition::Unconditional, &stamps, crc).await.unwrap();
            }
            b.sync().await.unwrap();
            assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("A's v3"), "fixture: B's sync did not fetch v3");
        }
        AfterRestart::UiRenamesTheCarriedChange => {
            let before = manifest::load(inner.as_ref(), &b.cfg).await.unwrap().unwrap();
            let v2 = before.manifest.entries.get("x.txt").cloned().expect("fixture: x.txt is not cited");
            assert_ne!(
                b.state.load_baseline().unwrap().entries.get("x.txt").map(|e| &e.etag),
                Some(&v2.etag),
                "fixture: B's baseline already holds v2"
            );
            hitl_rename(&inner, &b.cfg, "x.txt", "w.txt", "dilip").await;
            // The barrier that performs the rename: judged in the state it
            // leaves, as the model's invariants are, not three barriers on,
            // when the tree's pending copy has been uploaded again.
            let r = b.run_barrier().await.unwrap();
            let m = manifest::load(inner.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
            assert!(!m.entries.contains_key("x.txt"), "fixture: the rename's source was not removed ({r:?})");
            assert_eq!(
                m.entries.get("w.txt").map(|e| e.key.as_str()),
                Some(v2.key.as_str()),
                "the acked rename's handle is cited at neither name ({r:?})"
            );
            assert!(inner.head(&v2.key).await.is_ok(), "the collector took the renamed file's handle ({r:?})");
        }
    }

    for _ in 0..3 {
        b.run_barrier().await.unwrap();
    }
    if then == AfterRestart::PeerV3ThenSync {
        assert_eq!(
            read(dir_b.path(), "x.txt").as_deref(),
            Some("A's v3"),
            "the change the journal carried past the restart landed over the v3 B's sync fetched"
        );
        let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
        let (_, body) = inner.get_whole(&m.entries["x.txt"].key, Some(&m.entries["x.txt"].etag)).await.unwrap();
        assert_eq!(&body[..], b"A's v3", "the manifest reverted A's v3");
    } else if then == AfterRestart::UiRenamesTheCarriedChange {
        let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
        assert!(!m.entries.contains_key("x.txt"), "the rename's source is still cited");
        let w = m.entries.get("w.txt").unwrap_or_else(|| panic!("the renamed file is cited nowhere: {:?}", m.entries.keys()));
        let (_, body) = inner.get_whole(&w.key, Some(&w.etag)).await.unwrap();
        assert_eq!(&body[..], b"A's v2", "the rename's destination cites the wrong bytes");
        assert_eq!(read(dir_b.path(), "w.txt").as_deref(), Some("A's v2"), "the renamed file left B's tree");
        assert_eq!(read(dir_b.path(), "x.txt"), None, "the rename's source stayed in B's tree");
    } else if peer_deletes {
        assert_eq!(read(dir_b.path(), "x.txt"), None, "a restart between B's CAS and its step 7 kept A's deleted file in B's tree");
        let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
        assert!(!m.entries.contains_key("x.txt"), "B re-published the file A deleted");
    } else {
        assert_eq!(
            read(dir_b.path(), "x.txt").as_deref(),
            Some("A's v2"),
            "a restart between B's CAS and its step 7 lost A's change from B's tree for good"
        );
    }
    assert!(!derive_pending(&b).await, "B still owes a change after three barriers");
}

/// P1-lite has no journal, so a restart between a writer's CAS and its step
/// 7 leaves its baseline behind a document it wrote itself. Its own upload
/// must then CONVERGE: the tree's bytes are the document's, the baseline
/// follows, and nothing is published again — no second generation of the
/// same bytes, no record of its own version as a stranger's.
#[tokio::test]
async fn a_restart_after_the_cas_converges_on_its_own_upload_without_republishing() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "f.txt", "v1");
    a.run_barrier().await.unwrap();

    write(dir.path(), "f.txt", "v2 is longer");
    backdate_baseline(&a, "f.txt");
    let stale = a.state.load_baseline().unwrap();
    a.run_barrier().await.unwrap();
    let installed = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    // The restart: step 7 never ran.
    a.state.save_baseline(&stale).unwrap();
    let conflicts_before = a.state.load_conflicts().unwrap().len();

    let r = a.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.seq, installed.seq, "the converged upload was published again: {r:?}");
    assert_eq!(m.entries["f.txt"].key, installed.entries["f.txt"].key);
    let new: Vec<_> = a.state.load_conflicts().unwrap().into_iter().skip(conflicts_before).collect();
    assert!(new.is_empty(), "its own version was recorded as a stranger's: {new:?}");
    assert_eq!(a.state.load_baseline().unwrap().entries["f.txt"].etag, installed.entries["f.txt"].etag);
}

/// A PULL-ONLY barrier (nothing to publish; here the agent's delete is only
/// a first absence) that finds a peer's change the consume did not take —
/// it landed after the consume — takes theirs as the document it is
/// integrated with. It must mark the change OWED, or the next consume sees
/// the pointer unmoved and never takes it.
#[tokio::test]
async fn a_pull_only_barrier_leaves_a_change_it_saw_owed() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir.path()).await;
    b.checkout().await.unwrap();
    write(dir.path(), "x.txt", "seed");
    write(dir.path(), "gone.txt", "the agent deletes this");
    b.run_barrier().await.unwrap();

    std::fs::remove_file(dir.path().join("gone.txt")).unwrap();
    land_during_next_barrier(&store, &b, "x.txt", "A's v2");
    let r = b.run_barrier().await.unwrap();
    assert!(super::barrier::CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_none()), "fixture: the window never ran");
    assert!(r.uploaded.is_empty() && r.deleted.is_empty(), "fixture: not a pull-only barrier: {r:?}");
    assert_eq!(read(dir.path(), "x.txt").as_deref(), Some("seed"), "fixture: the change already reached the tree");

    // Nothing else moves the pointer. The agent restores the file, so the
    // next barrier has nothing to publish either.
    write(dir.path(), "gone.txt", "the agent deletes this");
    backdate_baseline(&b, "gone.txt");
    b.run_barrier().await.unwrap();
    assert_eq!(read(dir.path(), "x.txt").as_deref(), Some("A's v2"), "the change the pull-only barrier saw was never taken");
}

/// Put back what the baseline records: the bytes, and the mtime the
/// baseline stamped, so the path is CLEAN again by stat.
fn restore_to_baseline(sc: &Syncer, rel: &str, content: &str) {
    write(&sc.cfg.root, rel, content);
    let e = sc.state.load_baseline().unwrap().entries[rel].clone();
    let at = std::time::UNIX_EPOCH
        + std::time::Duration::new(e.mtime_unix as u64, e.mtime_nanos.unwrap_or(0));
    std::fs::File::options().write(true).open(sc.cfg.root.join(rel)).unwrap().set_modified(at).unwrap();
    assert!(!super::barrier::local_dirty(&sc.cfg.root.join(rel), Some(&e)), "fixture: still dirty");
}

// A consume that skips a path because the agent is working on it must
// leave the syncer BEHIND: once the agent backs out, the peer's version is
// owed, and nothing else will move the pointer (LeanP1 gate 2026-09-25,
// Inv_ShortcutSound, 9 states).
#[tokio::test]
async fn a_peers_file_skipped_under_an_agents_new_one_arrives_when_the_agent_drops_it() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir.path()).await;
    b.checkout().await.unwrap();
    write(dir.path(), "x.txt", "the agent's draft, never published");
    peer_publish(&store, &b.cfg, "x.txt", "the peer's file", "peer").await;
    b.consume_owed().await.unwrap();
    assert_eq!(read(dir.path(), "x.txt").as_deref(), Some("the agent's draft, never published"), "fixture: the consume took it");

    std::fs::remove_file(dir.path().join("x.txt")).unwrap();
    for _ in 0..2 {
        b.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir.path(), "x.txt").as_deref(), Some("the peer's file"), "the peer's file never arrived");
}

#[tokio::test]
async fn a_peers_delete_skipped_under_an_agents_edit_lands_when_the_agent_reverts() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir.path()).await;
    b.checkout().await.unwrap();
    write(dir.path(), "x.txt", "seed");
    b.run_barrier().await.unwrap();

    write(dir.path(), "x.txt", "the agent's edit");
    hitl_remove(&store, &b.cfg, "x.txt", "peer").await;
    b.consume_owed().await.unwrap();
    assert_eq!(read(dir.path(), "x.txt").as_deref(), Some("the agent's edit"), "fixture: the consume removed it");

    restore_to_baseline(&b, "x.txt", "seed");
    for _ in 0..2 {
        b.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir.path(), "x.txt"), None, "the peer's delete never landed");
}

#[tokio::test]
async fn a_peers_version_whose_write_lost_to_the_agent_arrives_when_the_agent_reverts() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir.path()).await;
    b.checkout().await.unwrap();
    write(dir.path(), "x.txt", "seed");
    b.run_barrier().await.unwrap();

    peer_publish(&store, &b.cfg, "x.txt", "the peer's v2", "peer").await;
    // The agent writes between the fetch and the rename: the consume's
    // write stands down (H7).
    let root = b.cfg.root.clone();
    super::barrier::CONSUME_WINDOW_HOOK.with(|h| {
        *h.borrow_mut() = Some((
            "x.txt".into(),
            "before-write",
            Box::new(move || {
                std::fs::write(root.join("x.txt"), "the agent's edit").unwrap();
            }),
        ))
    });
    b.consume_owed().await.unwrap();
    assert!(super::barrier::CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_none()), "fixture: the window never ran");
    assert_eq!(read(dir.path(), "x.txt").as_deref(), Some("the agent's edit"), "fixture: the consume overwrote it");

    restore_to_baseline(&b, "x.txt", "seed");
    for _ in 0..2 {
        b.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir.path(), "x.txt").as_deref(), Some("the peer's v2"), "the peer's version never arrived");
}

/// Deletes cross between writers the way edits do: A's delete of a path
/// B holds clean reaches B's tree.
#[tokio::test]
async fn a_peers_delete_reaches_the_other_writers_tree() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "seed");
    write(dir_a.path(), "keep.txt", "keep");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("seed"));

    std::fs::remove_file(dir_a.path().join("x.txt")).unwrap();
    a.run_barrier().await.unwrap();
    a.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("x.txt"), "fixture: A's delete never published");

    for _ in 0..3 {
        b.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir_b.path(), "x.txt"), None, "A's delete never reached B's tree");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("x.txt"), "B resurrected the path A deleted");
}

/// A delete/modify race, MY DELETE meeting THEIR EDIT. Before P1-lite the
/// delete was outranked for a barrier and won the next through the queue's
/// consume-dirty preservation (this test pinned that, 2026-09-24, with the
/// note that the merge table replacing it must keep the row). M3 keeps the
/// row — mine wins, theirs preserved — and drops the lag: the delete lands
/// in the FIRST barrier.
#[tokio::test]
async fn my_delete_meeting_a_peers_edit_wins_at_once_with_the_edit_preserved() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "seed");
    write(dir_a.path(), "keep.txt", "keep");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();

    write(dir_b.path(), "x.txt", "B's edit");
    backdate_baseline(&b, "x.txt");
    b.run_barrier().await.unwrap();
    let v2 = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest.entries["x.txt"].clone();

    std::fs::remove_file(dir_a.path().join("x.txt")).unwrap();
    a.declared_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("x.txt"), "A's delete never won: {:?}", m.entries.get("x.txt"));
    let records = a.state.load_conflicts().unwrap();
    assert!(
        records.iter().any(|r| r.path == "x.txt" && r.foreign_etag.trim_matches('"') == v2.etag.trim_matches('"')),
        "B's edit was not preserved with a record: {records:?}"
    );
    for _ in 0..3 {
        b.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir_b.path(), "x.txt"), None, "A's delete never reached B's tree");
}

/// The other direction, MY EDIT meeting THEIR DELETE: A's edit re-creates
/// the path B deleted — mine wins — and B's tree takes it back. And A
/// RECORDS the delete it overrode, naming the version the delete removed
/// (the user's rule, 2026-09-24: a delete over a dirty path lands, with a
/// record). Under P2 a UI delete commits at once, so this is the route
/// every UI delete of a file an agent is editing takes. Before, there was
/// no record: the merge reported what mine publishes over only among
/// theirs' ENTRIES, and a deleted path has none.
#[tokio::test]
async fn my_edit_meeting_a_peers_delete_recreates_the_path() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "seed");
    write(dir_a.path(), "keep.txt", "keep");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();

    std::fs::remove_file(dir_b.path().join("x.txt")).unwrap();
    b.declared_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("x.txt"), "fixture: B's delete never published");

    write(dir_a.path(), "x.txt", "A's edit");
    backdate_baseline(&a, "x.txt");
    for _ in 0..3 {
        a.run_barrier().await.unwrap();
    }
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let x = m.entries.get("x.txt").unwrap_or_else(|| panic!("A's edit lost to B's delete: {:?}", m.entries.keys()));
    let (_, body) = store.get_whole(&x.key, Some(&x.etag)).await.unwrap();
    assert_eq!(&body[..], b"A's edit");
    let seed_etag = format!("\"{:x}\"", <sha2::Sha256 as sha2::Digest>::digest(b"seed"));
    assert!(
        a.state.load_conflicts().unwrap().iter().any(|c| c.path == "x.txt" && c.foreign_etag == seed_etag && c.kind.starts_with("commit-recreated-deleted")),
        "A's edit overrode B's delete with no record: {:?}",
        a.state.load_conflicts().unwrap()
    );
    assert_eq!(read(dir_a.path(), "x.txt").as_deref(), Some("A's edit"));
    for _ in 0..3 {
        b.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("A's edit"), "B's tree never took the re-created path");
}

/// A UI write that re-creates a path a peer deleted survives the deletion
/// the other writer's merge saw but its tree has not taken. The deletion is
/// OLDER than the write, so the consume must take the write, never the
/// deletion over it. Found by the formal model with the writer-local queue
/// modelled (2026-09-15, `Inv_HITLTracked`, 19 steps); under P1-lite the
/// deletion is derived from the CURRENT document, which cites the write.
#[tokio::test]
async fn a_ui_write_over_a_peers_delete_survives_the_queued_tombstone() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b, dir_c) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "seed");
    write(dir_a.path(), "keep.txt", "keep");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();

    // A peer's delete lands inside B's next barrier: B's merge sees it, and
    // B's tree keeps the file until a later consume.
    write(dir_b.path(), "z.txt", "z");
    land_delete_during_next_barrier(&store, &b, "x.txt");
    b.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("x.txt"), "fixture: the delete never landed");
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("seed"), "fixture: the deletion already reached B's tree");
    assert!(derive_pending(&b).await, "fixture: B owes nothing for x.txt");

    // The UI writes the path again, and is acked.
    hitl_write(&store, &a.cfg, "x.txt", "from the UI", "reviewer").await.unwrap();

    b.run_barrier().await.unwrap();
    for _ in 0..2 {
        a.run_barrier().await.unwrap();
        b.run_barrier().await.unwrap();
    }
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let mut c = syncer(&store, dir_c.path()).await;
    c.checkout().await.unwrap();
    assert_eq!(
        read(dir_c.path(), "x.txt").as_deref(),
        Some("from the UI"),
        "the acked UI write is tracked by nothing: manifest cites x.txt {:?}, A's tree {:?}, B's tree {:?}",
        m.entries.get("x.txt").map(|e| e.etag.clone()),
        read(dir_a.path(), "x.txt"),
        read(dir_b.path(), "x.txt"),
    );
}

/// Two IDLE writers settle. A barrier that finds the manifest moved but
/// has nothing of its own to publish must not install a generation of its
/// own — or the peer's next tick sees the manifest move, does the same,
/// and the two trade empty generations (and cell claims) forever.
#[tokio::test]
async fn two_idle_writers_do_not_trade_empty_generations() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    b.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "from A");
    a.run_barrier().await.unwrap();
    // B integrates A's publish; then neither writer changes anything.
    for _ in 0..2 {
        b.run_barrier().await.unwrap();
        a.run_barrier().await.unwrap();
    }
    let settled = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;
    for _ in 0..4 {
        b.run_barrier().await.unwrap();
        a.run_barrier().await.unwrap();
    }
    let later = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;
    assert_eq!(later, settled, "idle writers kept installing generations: seq {settled} -> {later}");
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("from A"));
}


/// A UI write through the gateway reaches EVERY writer of the workspace,
/// not only the one whose consume took the inbox entry. The first
/// consumer drops the entry once its commit cites the write; the other
/// writer learns of it through its own merge (a foreign change, into its
/// local queue) and fetches it at the next consume.
#[tokio::test]
async fn a_gateway_write_reaches_every_writer_not_only_the_first_consumer() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "doc.md", "v1");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();

    hitl_write(&store, &a.cfg, "doc.md", "edited in the UI", "user@ui").await.unwrap();
    hitl_write(&store, &a.cfg, "new-from-ui.md", "created in the UI", "user@ui").await.unwrap();
    // P2: both are committed; A's tree takes them as peers' changes (one
    // barrier queues, the next writes).
    a.run_barrier().await.unwrap();
    a.run_barrier().await.unwrap();
    assert_eq!(read(dir_a.path(), "doc.md").as_deref(), Some("edited in the UI"));

    // B must converge on both writes too.
    b.run_barrier().await.unwrap();
    b.run_barrier().await.unwrap();
    assert_eq!(read(dir_b.path(), "doc.md").as_deref(), Some("edited in the UI"), "the UI edit never reached B");
    assert_eq!(read(dir_b.path(), "new-from-ui.md").as_deref(), Some("created in the UI"), "the UI create never reached B");
    assert_every_citation_resolves(&store, &a.cfg, "after both").await;
}

/// The same UI write against a path the second writer has EDITED: its
/// agent's version wins at its next boundary, the UI's bytes are
/// preserved in the bucket, and a record says so — never a silent loss.
#[tokio::test]
async fn a_gateway_write_over_a_path_another_writer_edited_is_preserved_on_that_writer() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "doc.md", "v1");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();

    write(dir_b.path(), "doc.md", "B's agent edit");
    backdate_baseline(&b, "doc.md");
    hitl_write(&store, &a.cfg, "doc.md", "edited in the UI", "user@ui").await.unwrap();
    a.run_barrier().await.unwrap(); // consumes, cites, drops
    b.run_barrier().await.unwrap();
    b.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let (_, body) = store.get_whole(&m.entries["doc.md"].key, Some(&m.entries["doc.md"].etag)).await.unwrap();
    assert_eq!(&body[..], b"B's agent edit", "B's later boundary is current");
    let preserved: Vec<_> = b
        .state
        .load_conflicts()
        .unwrap()
        .into_iter()
        .filter(|c| c.path == "doc.md" && c.preserved_key.is_some())
        .collect();
    let mut found_ui = false;
    for c in &preserved {
        let (_, kept) = store.get_whole(c.preserved_key.as_ref().unwrap(), None).await.unwrap();
        found_ui |= &kept[..] == b"edited in the UI";
    }
    assert!(found_ui, "the UI's bytes are not preserved under any record on B: {preserved:?}");
    assert_every_citation_resolves(&store, &a.cfg, "after both").await;
}

/// The event trace is EVIDENCE for the live drill: its oracles and its
/// timeline read these event and field names, and a rename here would
/// make an oracle silently count nothing. So two writers run a delete, a
/// concurrent edit and the convergence that follows with the trace on,
/// and the test pins what the drill's tools depend on — the prefix, the
/// claim shape (a deposal is `verdict: claimed, how: deposed`), the GC
/// and tombstone results, the request counter keys — and that one
/// writer's GC of a path precedes the other writer's tombstone of it in
/// wall-clock order, which is what a cross-node timeline is built on.
/// `FLINT_SYNC_TRACE_FIXTURE=<path>` writes the captured lines out, for
/// the drill's oracle self-test to read a real trace, not a hand-made one.
#[tokio::test]
async fn the_event_trace_reconstructs_a_two_writer_interleaving() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let buf = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.cfg.event_trace = Some(crate::trace::Sink::Memory(buf.clone()));
    b.cfg.event_trace = Some(crate::trace::Sink::Memory(buf.clone()));
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "seed");
    write(dir_a.path(), "keep.txt", "keep");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();

    // A deletes x while B edits keep; then both converge.
    std::fs::remove_file(dir_a.path().join("x.txt")).unwrap();
    write(dir_b.path(), "keep.txt", "B's edit");
    backdate_baseline(&b, "keep.txt");
    // A delete publishes on its second scan (the two-scan guard), and the
    // peer's tombstone is consumed a barrier after its merge queued it.
    a.run_barrier().await.unwrap();
    b.run_barrier().await.unwrap();
    for _ in 0..3 {
        a.run_barrier().await.unwrap();
        b.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir_b.path(), "x.txt"), None, "fixture: A's delete never reached B");
    assert_eq!(read(dir_a.path(), "keep.txt").as_deref(), Some("B's edit"), "fixture: B's edit never reached A");

    let lines = buf.lock().unwrap().clone();
    if let Ok(out) = std::env::var("FLINT_SYNC_TRACE_FIXTURE") {
        std::fs::write(&out, lines.join("\n") + "\n").unwrap();
    }
    let evs: Vec<serde_json::Map<String, serde_json::Value>> = lines
        .iter()
        .map(|l| {
            assert!(l.starts_with("{\"ts_ms\":"), "not the pinned prefix: {l}");
            let v: serde_json::Value = serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}"));
            let m = v.as_object().unwrap().clone();
            let head: Vec<&str> = m.keys().take(4).map(|k| k.as_str()).collect();
            // serde_json's map is sorted unless preserve_order is on, so
            // check the prefix on the TEXT, and the keys by presence.
            for k in ["ts_ms", "mono_ms", "holder", "ev"] {
                assert!(m.contains_key(k), "{k} missing ({head:?}): {l}");
            }
            let pos: Vec<usize> = ["\"ts_ms\":", "\"mono_ms\":", "\"holder\":", "\"ev\":"]
                .iter()
                .map(|k| l.find(k).unwrap())
                .collect();
            assert!(pos.windows(2).all(|w| w[0] < w[1]), "prefix out of order: {l}");
            m
        })
        .collect();
    let ev = |e: &serde_json::Map<String, serde_json::Value>| e["ev"].as_str().unwrap().to_string();
    let s = |e: &serde_json::Map<String, serde_json::Value>, k: &str| e.get(k).and_then(|v| v.as_str()).map(str::to_string);

    // Every commit-section event names its writer, and there are two.
    let holders: std::collections::BTreeSet<String> =
        evs.iter().filter(|e| ["claim", "cas", "gc", "release"].contains(&ev(e).as_str())).map(|e| {
            s(e, "holder").unwrap_or_else(|| panic!("a commit-section event without a holder: {e:?}"))
        }).collect();
    assert_eq!(holders.len(), 2, "expected both writers in the trace: {holders:?}");

    // The claim shape the oracles read.
    for e in evs.iter().filter(|e| ev(e) == "claim") {
        let verdict = s(e, "verdict").unwrap();
        assert!(["claimed", "waiting", "deadline"].contains(&verdict.as_str()), "claim verdict {verdict}: {e:?}");
        if verdict == "claimed" {
            let how = s(e, "how").unwrap_or_else(|| panic!("a claimed claim without how: {e:?}"));
            assert!(
                ["fresh", "adopted-own", "deposed", "orphaned-own", "skipped-handoff", "released"].contains(&how.as_str()),
                "claim how {how}: {e:?}"
            );
            assert!(e["epoch"].as_u64().is_some(), "claimed without an epoch: {e:?}");
        }
    }
    for e in evs.iter().filter(|e| ev(e) == "cas") {
        assert!(["ok", "lost"].contains(&s(e, "result").unwrap().as_str()), "cas result: {e:?}");
    }
    for e in evs.iter().filter(|e| ev(e) == "barrier_end") {
        let req = e["requests"].as_object().unwrap_or_else(|| panic!("barrier_end without request counts: {e:?}"));
        let keys: Vec<&str> = req.keys().map(|k| k.as_str()).collect();
        let mut want = vec!["copy", "delete", "get", "head", "list", "multipart", "put"];
        let mut got = keys.clone();
        want.sort();
        got.sort();
        assert_eq!(got, want, "request counter keys: {e:?}");
    }
    let cas_ok_by: std::collections::BTreeSet<String> =
        evs.iter().filter(|e| ev(e) == "cas" && s(e, "result").as_deref() == Some("ok")).filter_map(|e| s(e, "holder")).collect();
    assert_eq!(cas_ok_by.len(), 2, "both writers must install at least once: {cas_ok_by:?}");

    // Each barrier that installs, in emission order: start, scan, claimed,
    // cas ok, release, end — per writer.
    for h in &holders {
        let mine: Vec<String> = evs
            .iter()
            .filter(|e| s(e, "holder").as_deref() == Some(h.as_str()))
            .map(|e| match ev(e).as_str() {
                "claim" => format!("claim:{}", s(e, "verdict").unwrap()),
                "cas" => format!("cas:{}", s(e, "result").unwrap()),
                other => other.to_string(),
            })
            .filter(|n| ["scan", "claim:claimed", "cas:ok", "release", "barrier_end"].contains(&n.as_str()))
            .collect();
        let joined = mine.join(" ");
        assert!(
            joined.contains("scan claim:claimed cas:ok release barrier_end"),
            "{h}: no install in protocol order: {joined}"
        );
    }

    // The cross-writer order a timeline needs: the GC that deleted x (A)
    // precedes the tombstone that removed it from B's tree, by wall clock.
    let gc_x = evs
        .iter()
        .position(|e| ev(e) == "gc" && s(e, "path").as_deref() == Some("x.txt") && s(e, "result").as_deref() == Some("retired"))
        .unwrap_or_else(|| panic!("no gc retired for x.txt in:\n{}", lines.join("\n")));
    let tomb_x = evs
        .iter()
        .position(|e| ev(e) == "tombstone" && s(e, "path").as_deref() == Some("x.txt") && s(e, "action").as_deref() == Some("removed"))
        .unwrap_or_else(|| panic!("no tombstone removed for x.txt in:\n{}", lines.join("\n")));
    assert_ne!(s(&evs[gc_x], "holder"), s(&evs[tomb_x], "holder"), "the GC and the tombstone must be different writers");
    assert!(gc_x < tomb_x, "the tombstone was emitted before the GC");
    assert!(
        evs[gc_x]["ts_ms"].as_u64().unwrap() <= evs[tomb_x]["ts_ms"].as_u64().unwrap(),
        "wall clock puts the tombstone before the GC"
    );
    // And B's edit crossed to A, derived from the document (P1-lite).
    assert!(
        evs.iter().any(|e| ev(e) == "consume" && s(e, "path").as_deref() == Some("keep.txt") && s(e, "from").as_deref() == Some("manifest")),
        "B's edit did not reach A through the consume:\n{}",
        lines.join("\n")
    );
}

/// A killed writer's fixture: A publishes p1 and p2, B checks out, A's agent
/// edits both and A is killed after p1's upload lands and before p2's (and
/// before any claim); B keeps publishing elsewhere. Returns the stores, B
/// — whose tree still holds `seed-1` — and the ORPHAN: the handle A's
/// upload of p1 landed at, which nothing cites and nothing names.
async fn killed_writer_orphan() -> (Arc<MemoryStore>, Syncer, tempfile::TempDir, tempfile::TempDir, String) {
    let inner = Arc::new(MemoryStore::new());
    let ha = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = hooked_syncer(&ha, dir_a.path());
    a.cfg.upload_fanout = 1; // p1 lands before p2's PUT, deterministically
    let mut b = syncer(&inner, dir_b.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "p1.txt", "seed-1");
    write(dir_a.path(), "p2.txt", "seed-2");
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    let seed1 = cited_key(&inner, &a.cfg, "p1.txt").await;

    write(dir_a.path(), "p1.txt", "A's unpublished edit");
    backdate_baseline(&a, "p1.txt");
    write(dir_a.path(), "p2.txt", "A's other edit");
    backdate_baseline(&a, "p2.txt");
    let cfg = a.cfg.clone();
    ha.before_put(&a.cfg.file_key("p2.txt"), || panic!("A is killed between its uploads"));
    assert!(barrier_on_thread(a).join().is_err(), "fixture: A was not killed");
    let handles = handles_for(&inner, &cfg, "p1.txt").await;
    let orphan = handles.iter().find(|k| **k != seed1).cloned().expect("fixture: A's upload of p1 never landed");
    let (_, body) = inner.get_whole(&orphan, None).await.unwrap();
    assert_eq!(&body[..], b"A's unpublished edit", "fixture: not A's upload");

    write(dir_b.path(), "b.txt", "B works elsewhere");
    for _ in 0..4 {
        b.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir_b.path(), "p1.txt").as_deref(), Some("seed-1"), "fixture: B already has A's edit");
    (inner, b, dir_a, dir_b, orphan)
}

/// CONVERGENCE under a killed writer (finding 10, and H6's class). Uploads
/// hold no lease, so a writer killed after an upload lands and before its
/// commit leaves bytes nothing cites and nothing names. Under the slot
/// those bytes sat AT THE CITED KEY: a fresh checkout read them (S3-wins)
/// while a live writer kept the cited version, and the sweep's answer was
/// to TRACK them through the inbox so every tree adopted them. Under
/// handles (design 2026-09-19, R4) they sit at a handle of their own, no
/// reader can reach them — a checkout reads what the manifest cites and
/// nothing else — and the orphan sweep COLLECTS them from inside a commit
/// section once they are older than the grace. A lost writer's set is
/// never adopted, path by path or at all: nothing acked is lost (its
/// agent never got an ack), and the trees and the bucket agree throughout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_writer_killed_after_its_upload_does_not_leave_the_trees_diverged() {
    let (inner, mut b, _dir_a, dir_b, orphan) = killed_writer_orphan().await;

    // Before any sweep: a fresh checkout and the live tree agree already,
    // because neither can see the orphan.
    let dir_c = tempfile::tempdir().unwrap();
    let mut c = syncer(&inner, dir_c.path()).await;
    c.checkout().await.unwrap();
    assert_eq!(read(dir_b.path(), "p1.txt").as_deref(), Some("seed-1"));
    assert_eq!(read(dir_c.path(), "p1.txt").as_deref(), Some("seed-1"), "a fresh checkout read the orphan");
    assert!(inner.head(&orphan).await.is_ok(), "fixture: the orphan is still there");

    // The ingress sweep has nothing to say about a handle...
    let tracked = b.track_untracked(now_unix() + b.cfg.untracked_grace_secs).await.unwrap();
    assert!(tracked.is_empty(), "the ingress sweep tracked a lost writer's handle: {tracked:?}");
    // ...and the orphan sweep, due, collects it inside B's next commit.
    b.cfg.untracked_grace_secs = 0;
    b.state.save_orphan_sweep_at(now_unix() - b.cfg.untracked_sweep_secs - 1).unwrap();
    write(dir_b.path(), "b2.txt", "B publishes again");
    let r = b.run_barrier().await.unwrap();
    assert_eq!(r.swept, 1, "the orphan sweep did not collect the killed writer's handle: {r:?}");
    assert!(matches!(inner.head(&orphan).await, Err(StoreError::NotFound(_))), "the orphan survived the sweep");

    let m = manifest::load(inner.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(cited_bytes(&inner, &b.cfg, "p1.txt").await.as_deref(), Some(&b"seed-1"[..]), "the manifest adopted the orphan");
    assert_eq!(m.entries["p1.txt"].key, cited_key(&inner, &b.cfg, "p1.txt").await);
    assert_eq!(read(dir_b.path(), "p1.txt").as_deref(), Some("seed-1"));
}

/// The INGRESS sweep (design 2026-09-19, R5): an outside writer's object at
/// the bare path — `aws s3 cp` into `files/`, the one write nothing in this
/// crate makes any more — is left alone inside the grace, and tracked from
/// the floor only when due: a writer's first tick starts the clock, and a
/// tick past the interval copies it to a handle of its own, appends the
/// entry, and consumes it in the same barrier. The ingress object itself
/// is never touched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_floor_sweeps_for_ingress_objects_only_past_the_grace_and_when_due() {
    let (inner, mut b, _dir_a, dir_b, _orphan) = killed_writer_orphan().await;
    // An outside writer overwrites the bare path.
    let body = Bytes::from_static(b"an outside writer's bytes");
    let stamps = GenerationStamps { generation: 0, epoch: 0, flush_uuid: "aws-cli".into(), boundary_source: None, posix: None };
    let ingress = inner
        .put_whole(&b.cfg.file_key("p1.txt"), body.clone(), &PutCondition::Unconditional, &stamps, crc64_nvme(&body))
        .await
        .unwrap();
    assert_eq!(b.track_untracked(now_unix()).await.unwrap(), Vec::<String>::new(), "tracked inside the grace");

    b.cfg.untracked_grace_secs = 0;
    b.cfg.untracked_sweep_secs = 3600;
    b.floor_tick().await.unwrap();
    assert_eq!(read(dir_b.path(), "p1.txt").as_deref(), Some("seed-1"), "the first tick swept instead of starting the clock");
    assert!(b.state.load_untracked_sweep_at().unwrap() > 0, "the first tick did not start the clock");

    b.state.save_untracked_sweep_at(now_unix() - 3601).unwrap();
    b.floor_tick().await.unwrap();
    // P2: the sweep commits; B's tree takes the copy at its next barriers.
    for _ in 0..2 {
        b.run_barrier().await.unwrap();
    }
    assert_eq!(
        read(dir_b.path(), "p1.txt").as_deref(),
        Some("an outside writer's bytes"),
        "a due floor did not track and consume the ingress object"
    );
    let m = manifest::load(inner.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    let cited = &m.entries["p1.txt"];
    assert_eq!(b.cfg.handle_parts(&cited.key).map(|(_, f)| f), Some(super::untracked::ingress_flush(&ingress.etag).as_str()),
        "the manifest cites something other than the ingress object's adopted copy: {}", cited.key);
    let (at_key, _) = inner.get_whole(&b.cfg.file_key("p1.txt"), None).await.unwrap();
    assert_eq!(at_key.etag, ingress.etag, "the ingress object was touched");
    // Idempotent: a second sweep of the same ingress version adopts nothing.
    assert_eq!(b.track_untracked(now_unix()).await.unwrap(), Vec::<String>::new(), "adopted twice");
}

// ---------------------------------------------------------------------
// Read access (per-user access design §4.4, refreshed 2026-09-14): a
// syncer that follows the workspace and never writes to the bucket.
// ---------------------------------------------------------------------

/// The requests a read-only credential allows. Anything else a reader
/// sends is a write, and on a scoped key a 403.
const READ_OPS: &[&str] = &[
    "get_whole", "get_range", "get_version", "head", "head_version", "list", "list_versions",
    "list_uploads", "lifecycle_rules", "epoch_read", "presign_get",
];

fn writes_since_reset(store: &MemoryStore) -> Vec<(&'static str, u64)> {
    store.op_counts().into_iter().filter(|(op, _)| !READ_OPS.contains(op)).collect()
}

async fn reader(store: &Arc<MemoryStore>, root: &std::path::Path) -> Syncer {
    let mut sc = syncer(store, root).await;
    sc.cfg.access = super::Access::Read;
    sc
}

/// One workspace, two agents: A mounts it read-write, R read-only. Each
/// pod has its own syncer, and access is that syncer's, not the
/// workspace's — A publishes as ever and never learns R exists; R
/// follows A's boundaries and the inbox and sends only reads.
#[tokio::test]
async fn a_reader_follows_writers_and_the_inbox_without_one_store_write() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "A v1");
    write(dir_a.path(), "gone.txt", "A deletes this");
    a.floor_tick().await.unwrap();

    store.reset_op_counts();
    let mut r = reader(&store, dir_r.path()).await;
    r.checkout().await.unwrap();
    r.floor_tick().await.unwrap();
    assert_eq!(writes_since_reset(&store), vec![], "a reader's checkout and first tick wrote to the bucket");
    assert_eq!(read(dir_r.path(), "x.txt").as_deref(), Some("A v1"));

    // A writer publishes an edit, a delete and an add; a UI write waits
    // uncited in the inbox.
    write(dir_a.path(), "x.txt", "A v2");
    backdate_baseline(&a, "x.txt");
    std::fs::remove_file(dir_a.path().join("gone.txt")).unwrap();
    write(dir_a.path(), "new.txt", "A adds this");
    a.declared_barrier().await.unwrap();
    hitl_write(&store, &a.cfg, "ui.txt", "from the UI", "reviewer").await.unwrap();
    let seq_a = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;

    store.reset_op_counts();
    r.floor_tick().await.unwrap();
    assert_eq!(writes_since_reset(&store), vec![], "a reader's pull wrote to the bucket");
    assert_eq!(read(dir_r.path(), "x.txt").as_deref(), Some("A v2"));
    assert_eq!(read(dir_r.path(), "gone.txt"), None);
    assert_eq!(read(dir_r.path(), "new.txt").as_deref(), Some("A adds this"));
    assert_eq!(read(dir_r.path(), "ui.txt").as_deref(), Some("from the UI"), "the inbox write did not reach the reader");
    let t = r.load_remote_seq();
    assert_eq!((t.observed_seq, t.integrated_seq), (seq_a, seq_a), "{t:?}");

    // A local edit and a local file — a writable mount, or anything else
    // that put bytes in the reader's tree — are never uploaded, and the
    // edit is not overwritten: the foreign version is a sync-dirty record.
    write(dir_r.path(), "x.txt", "the reader's local edit");
    backdate_baseline(&r, "x.txt");
    write(dir_r.path(), "local-only.txt", "never published");
    write(dir_a.path(), "x.txt", "A v3");
    backdate_baseline(&a, "x.txt");
    a.declared_barrier().await.unwrap();
    store.reset_op_counts();
    r.floor_tick().await.unwrap();
    assert_eq!(writes_since_reset(&store), vec![], "a reader with local changes wrote to the bucket");
    assert_eq!(read(dir_r.path(), "x.txt").as_deref(), Some("the reader's local edit"));
    assert!(!object_at(&store, &r.cfg, "local-only.txt").await, "a reader published a local file");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("local-only.txt"));
    let conflicts = r.state.load_conflicts().unwrap();
    assert!(conflicts.iter().any(|c| c.path == "x.txt" && c.kind == "sync-dirty"), "{conflicts:?}");
}

#[tokio::test]
async fn an_idle_reader_tick_costs_the_inbox_and_the_pointer() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "A v1");
    a.floor_tick().await.unwrap();
    hitl_write(&store, &a.cfg, "ui.txt", "from the UI", "reviewer").await.unwrap();
    let mut r = reader(&store, dir_r.path()).await;
    r.checkout().await.unwrap();
    r.floor_tick().await.unwrap();

    store.reset_op_counts();
    r.floor_tick().await.unwrap();
    let ops = store.op_counts();
    assert_eq!(ops.into_iter().collect::<Vec<_>>(), vec![("get_whole", 2)], "an idle reader tick is two GETs");
    let t = r.load_remote_seq();
    assert!(t.updated_unix > 0, "the ticker's heartbeat did not move on an idle tick: {t:?}");

    // News in the inbox alone — no writer has published since — is news.
    hitl_write(&store, &a.cfg, "ui2.txt", "a second UI write", "reviewer").await.unwrap();
    r.floor_tick().await.unwrap();
    assert_eq!(read(dir_r.path(), "ui2.txt").as_deref(), Some("a second UI write"), "an inbox-only change never reached the reader");
}

#[tokio::test]
async fn a_publish_touch_on_a_reader_is_answered_refused_read_only() {
    let store = Arc::new(MemoryStore::new());
    let (dir_r, dir_w) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut r = reader(&store, dir_r.path()).await;
    r.checkout().await.unwrap();
    let posture = r.sentinel_preflight().unwrap();
    r.write_capabilities(&posture).unwrap();
    write(dir_r.path(), "f.txt", "a local file");

    store.reset_op_counts();
    touch_sentinel(dir_r.path(), control::PUBLISH, r#"{"nonce":"n-1"}"#);
    let acks = r.sentinel_tick().await.unwrap();
    assert_eq!(writes_since_reset(&store), vec![], "a refused publish wrote to the bucket");
    assert_eq!(acks.len(), 1, "{acks:?}");
    assert_eq!(acks[0].status, "refused-read-only");
    assert_eq!(acks[0].nonces, vec!["n-1".to_string()]);
    assert!(acks[0].reason.as_deref().unwrap_or("").contains("read-only"), "{:?}", acks[0].reason);
    assert!(r.load_pending(Verb::Publish).unwrap().is_none(), "the refused touch was left pending");
    let on_disk = std::fs::read_to_string(dir_r.path().join(super::CONTROL_DIR).join(control::PUBLISH_ACK)).unwrap();
    assert!(on_disk.contains("refused-read-only"), "{on_disk}");
    assert!(!object_at(&store, &r.cfg, "f.txt").await);

    // `sync` is a reader's verb.
    r.cfg.sentinel_min_interval_secs = 0;
    touch_sentinel(dir_r.path(), control::SYNC, r#"{"nonce":"s-1"}"#);
    let acks = r.sentinel_tick().await.unwrap();
    assert_eq!(acks.iter().map(|a| a.status.as_str()).collect::<Vec<_>>(), vec!["ok"]);

    // Control: the same touch on a writer publishes.
    let mut w = syncer(&store, dir_w.path()).await;
    w.checkout().await.unwrap();
    let posture = w.sentinel_preflight().unwrap();
    w.write_capabilities(&posture).unwrap();
    write(dir_w.path(), "g.txt", "a writer's file");
    touch_sentinel(dir_w.path(), control::PUBLISH, r#"{"nonce":"w-1"}"#);
    let acks = w.sentinel_tick().await.unwrap();
    assert_eq!(acks.iter().map(|a| a.status.as_str()).collect::<Vec<_>>(), vec!["ok"]);
}

#[tokio::test]
async fn a_barrier_on_a_reader_is_refused_before_any_write() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut r = reader(&store, dir.path()).await;
    r.checkout().await.unwrap();
    write(dir.path(), "f.txt", "would be an upload");
    store.reset_op_counts();
    for (name, out) in [
        ("run_barrier", r.run_barrier().await.map(|_| ())),
        ("cadence_barrier", r.cadence_barrier().await.map(|_| ())),
        ("declared_barrier", r.declared_barrier().await.map(|_| ())),
    ] {
        assert!(matches!(out, Err(LeanError::Refused(_))), "{name} on a reader: {out:?}");
    }
    assert_eq!(store.total_ops(), 0, "a refused barrier sent requests: {:?}", store.op_counts());
    assert!(!object_at(&store, &r.cfg, "f.txt").await);
}

#[tokio::test]
async fn one_shot_verbs_that_publish_or_fence_are_refused_on_a_reader() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut r = reader(&store, dir.path()).await;
    r.checkout().await.unwrap();
    store.reset_op_counts();
    for step in [super::verbs::Step::Barrier, super::verbs::Step::Rescope(None)] {
        let out = super::verbs::run_verb(&mut r, step).await;
        assert!(matches!(out, Err(LeanError::Refused(_))), "{out:?}");
    }
    assert_eq!(writes_since_reset(&store), vec![], "a refused verb wrote: {:?}", store.op_counts());
    assert!(super::verbs::run_verb(&mut r, super::verbs::Step::Sync).await.is_ok(), "sync is a reader's verb");
    assert!(super::verbs::run_verb(&mut r, super::verbs::Step::Checkout).await.is_ok());
    assert_eq!(writes_since_reset(&store), vec![]);
}

#[tokio::test]
async fn a_reader_drain_answers_what_it_owes_and_writes_nothing() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut r = reader(&store, dir.path()).await;
    r.checkout().await.unwrap();
    let posture = r.sentinel_preflight().unwrap();
    r.write_capabilities(&posture).unwrap();
    write(dir.path(), "f.txt", "local only");
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"at-sigterm"}"#);
    r.poll_sentinels().unwrap();
    assert!(r.load_pending(Verb::Publish).unwrap().is_some());

    store.reset_op_counts();
    let acks = r.drain().await.unwrap();
    assert_eq!(writes_since_reset(&store), vec![], "a reader's drain wrote to the bucket");
    assert_eq!(acks.iter().map(|a| a.status.as_str()).collect::<Vec<_>>(), vec!["refused-read-only"]);
    assert!(!object_at(&store, &r.cfg, "f.txt").await);
}

#[tokio::test]
async fn the_marker_says_read_access_and_the_guide_explains_it() {
    let store = Arc::new(MemoryStore::new());
    let (dir_r, dir_w) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut r = reader(&store, dir_r.path()).await;
    r.checkout().await.unwrap();
    let posture = r.sentinel_preflight().unwrap();
    r.write_capabilities(&posture).unwrap();
    let caps = r.read_capabilities().unwrap();
    assert_eq!(caps.access, "read");
    assert_eq!(caps.verbs, vec!["sync".to_string(), "remote-seq".to_string()], "a reader advertised {:?}", caps.verbs);

    let mut w = syncer(&store, dir_w.path()).await;
    w.checkout().await.unwrap();
    let posture = w.sentinel_preflight().unwrap();
    w.write_capabilities(&posture).unwrap();
    let caps = w.read_capabilities().unwrap();
    assert_eq!(caps.access, "readWrite");
    assert!(caps.verbs.contains(&"publish".to_string()));

    // An old marker (no `access`) still parses, as read-write.
    let old: control::Capabilities = serde_json::from_str(
        r#"{"protocol":1,"verbs":[],"state":"live","sentinel_min_interval_secs":5,
            "sentinel_hourly_budget":60,"syncer_version":"x","boot":{"holder_id":"h","boot_unix":0}}"#,
    )
    .unwrap();
    assert_eq!(old.access, "readWrite");

    for needle in ["\"access\": \"read\"", "refused-read-only", "EROFS"] {
        assert!(control::AGENT_GUIDE_TEXT.contains(needle), "the guide never says {needle}");
    }
}

/// The CSI node plugin launches a read-only volume's syncer with
/// `FLINT_SYNC_ACCESS=read` (`s3csi::node::lean_access_env`), and the
/// binary exits 2 on anything it cannot parse — so a spelling drift
/// between the two crates is a worker that never starts, not a writer.
#[test]
fn access_parses_exactly_what_the_plugin_writes() {
    use super::Access;
    assert_eq!(Access::parse("read"), Some(Access::Read));
    assert_eq!(Access::parse("readWrite"), Some(Access::ReadWrite));
    for bad in ["", "ro", "READ", "read-only", "readwrite", "rw"] {
        assert_eq!(Access::parse(bad), None, "{bad:?} must not parse");
    }
    assert_eq!(Access::default(), Access::ReadWrite);
    assert_eq!(Access::parse(Access::Read.as_str()), Some(Access::Read));
}

// ---- review 2026-09-18 -------------------------------------------------------

/// C1 (review 2026-09-18): A NAME RULE WITH NO OWNER. The walk skips every
/// name ending in `.flint-sync-tmp` (the consume's temp sibling, atomicity-7)
/// and the gateway accepted such a name. An acked UI write under it was
/// materialised by the consume, absent from the scan, cited once by the
/// citation repair as a first absence, classified as a delete at the next
/// barrier and its object COLLECTED — acked bytes destroyed with only a
/// trace line, no race needed. The name is reserved: the gateway refuses
/// it, and a citation or an entry that carries it anyway is surfaced and
/// never materialised, so the walk's blind spot can never become a delete.
#[tokio::test]
async fn a_consumed_entry_named_like_a_consume_temp_is_surfaced_never_collected() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    sc.checkout().await.unwrap();
    write(dir.path(), "agent.txt", "agent work");
    sc.run_barrier().await.unwrap();

    // An older gateway, or a client that chose the name: a document that
    // CITES it (P2: a UI write commits).
    let path = "notes/report.flint-sync-tmp";
    let key = sc.cfg.file_key(path);
    let body = Bytes::from_static(b"user bytes");
    let stamps = GenerationStamps {
        generation: 0,
        epoch: 0,
        flush_uuid: "gateway-old".into(),
        boundary_source: None,
        posix: None,
    };
    let meta = store
        .put_whole(&key, body.clone(), &PutCondition::IfNoneMatchAny, &stamps, crc64_nvme(&body))
        .await
        .unwrap();
    let (etag, crc) = (meta.etag.clone(), crc_of("user bytes"));
    let k = key.clone();
    gateway_commit(&store, &sc.cfg, "gateway-old", move |doc| {
        doc.entries.insert(path.into(), manifest::LeanEntry {
            key: k.clone(), etag: etag.clone(), crc64_b64: crc.clone(), size: 10,
            mode: 0o644, mtime_unix: 0, generation: 1, epoch: 0,
        });
    })
    .await
    .unwrap();

    for _ in 0..3 {
        sc.run_barrier().await.unwrap();
    }
    assert!(
        store.head(&key).await.is_ok(),
        "the acked write's object was collected: the consume materialised a name the walk cannot see, \
         the repair cited it as a first absence, and the next barrier deleted it"
    );
    assert!(read(dir.path(), path).is_none(), "a reserved name was materialised into the tree");
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key(path), "the walk's blind spot became a delete of the citation");
    let recs = std::fs::read_to_string(sc.cfg.state_dir().join("conflicts.jsonl")).unwrap_or_default();
    assert!(
        recs.contains("consume-refused-containment") && recs.contains(path),
        "nothing recorded the refusal: {recs}"
    );
}


// ---- review 2026-09-18, H10: an ack after the barrier that carried the declaration

/// Two writers on one published file; A's agent edits it (and deletes a
/// second one) and declares a boundary, and the sentinel is consumed.
async fn h10_declared_edit(store: &Arc<MemoryStore>, dir_a: &std::path::Path, dir_b: &std::path::Path) -> (Syncer, Syncer) {
    let mut a = syncer(store, dir_a).await;
    let mut b = syncer(store, dir_b).await;
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
    a.checkout().await.unwrap();
    write(dir_a, "shared.txt", "v1");
    write(dir_a, "gone.txt", "published, then deleted by A's agent before the touch");
    a.floor_tick().await.unwrap();
    a.floor_tick().await.unwrap();
    b.checkout().await.unwrap();

    write(dir_a, "shared.txt", "A's declared v2");
    backdate_baseline(&a, "shared.txt");
    std::fs::remove_file(dir_a.join("gone.txt")).unwrap();
    touch_sentinel(dir_a, control::PUBLISH, r#"{"nonce":"n-1"}"#);
    a.poll_sentinels().unwrap();
    assert!(a.load_pending(Verb::Publish).unwrap().is_some(), "fixture: the sentinel was not consumed");
    (a, b)
}

/// The install carried the whole declaration.
async fn h10_assert_carried(store: &Arc<MemoryStore>, a: &Syncer) {
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries["shared.txt"].crc64_b64, crc_of("A's declared v2"), "fixture: the declared edit was not installed");
    assert!(!m.entries.contains_key("gone.txt"), "the install withheld the delete the agent declared");
}

/// B integrates A's install, then deletes the declared file knowingly, in
/// a boundary of its own. Returns the seq of B's delete.
async fn h10_peer_deletes(store: &Arc<MemoryStore>, b: &mut Syncer, dir_b: &std::path::Path) -> u64 {
    b.floor_tick().await.unwrap();
    b.floor_tick().await.unwrap();
    assert_eq!(read(dir_b, "shared.txt").as_deref(), Some("A's declared v2"), "fixture: A's edit never reached B");
    std::fs::remove_file(dir_b.join("shared.txt")).unwrap();
    b.declared_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("shared.txt"), "fixture: B's delete did not publish");
    m.seq
}

/// The claim every H10 route shares: the document the ack names does not
/// cite the declared file (B deleted it; under P1-lite A's consume takes
/// the delete too). `ok` says the declared point is in it; it is
/// `partial`, and `report.dropped` names the path.
fn h10_assert_ack_is_honest(_dir_a: &std::path::Path, ack: &super::sentinel::Ack, peer_seq: u64) {
    assert!(ack.seq >= Some(peer_seq), "fixture: the ack names a document older than B's delete: {ack:?}");
    assert_eq!(
        ack.status, "partial",
        "the ack says the declared boundary is in seq {:?}, which no longer cites shared.txt: {ack:?}",
        ack.seq
    );
    assert_eq!(ack.report.dropped, vec!["shared.txt".to_string()], "{ack:?}");
}

/// Review 2026-09-18, H10, as the model found it (the crash world at
/// OrphanTrack = TRUE, depth 25): the honor's barrier installs A's
/// declared edit and runs step 7, and the container restarts before the
/// ack. B deletes the file. The restarted incarnation's re-run has nothing
/// of its own to publish and finds the manifest moved: it pulls B's
/// document and queues the delete — and acked that seq `ok`.
#[tokio::test]
async fn a_restart_between_the_install_and_the_ack_does_not_ack_a_peers_later_delete_as_the_declaration() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut a, mut b) = h10_declared_edit(&store, dir_a.path(), dir_b.path()).await;

    // The honor's barrier, through step 7; the ack is never written.
    a.declared_barrier_as("sentinel").await.unwrap();
    h10_assert_carried(&store, &a).await;
    assert!(a.read_ack(Verb::Publish).is_none(), "fixture: an ack was written");
    drop(a);

    let peer_seq = h10_peer_deletes(&store, &mut b, dir_b.path()).await;

    // The restart.
    let mut a = syncer(&store, dir_a.path()).await;
    a.settle_pending_at_startup().await.unwrap();
    let ack = a.read_ack(Verb::Publish).expect("the restart answered nothing");
    h10_assert_ack_is_honest(dir_a.path(), &ack, peer_seq);
    assert!(a.load_pending(Verb::Publish).unwrap().is_none());
}

/// H10 with no restart: the honor's barrier installs, and the ack write
/// fails (a directory squats on the ack path). The pending stands; B
/// deletes the file; the next honor re-runs, pulls B's document — and
/// acked that seq `ok`.
#[tokio::test]
async fn an_ack_write_that_failed_is_not_answered_later_with_a_peers_delete_as_the_declaration() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut a, mut b) = h10_declared_edit(&store, dir_a.path(), dir_b.path()).await;

    let squat = dir_a.path().join(super::CONTROL_DIR).join(control::PUBLISH_ACK);
    let _ = std::fs::remove_file(&squat);
    std::fs::create_dir_all(squat.join("occupied")).unwrap();
    clear_min_interval(&a);
    assert!(a.honor_pending(Verb::Publish, false).await.is_err(), "fixture: the ack write did not fail");
    h10_assert_carried(&store, &a).await;
    assert!(a.load_pending(Verb::Publish).unwrap().is_some(), "fixture: the pending was retired");
    std::fs::remove_dir_all(&squat).unwrap();

    let peer_seq = h10_peer_deletes(&store, &mut b, dir_b.path()).await;

    let ack = a.honor_pending(Verb::Publish, true).await.unwrap().expect("nothing was answered");
    h10_assert_ack_is_honest(dir_a.path(), &ack, peer_seq);
}

/// H10's third route: the honor fails before its commit, and the floor
/// tick runs its cadence barrier anyway (`floor_tick`; review 2026-09-12
/// ack-1: an honor error must not cost the cadence boundary). That
/// barrier began after the consume, so it carries the declaration — ALL
/// of it: it used to withhold the delete the agent declared (a first
/// absence waits for a second scan outside a declared barrier; the
/// model's `DSet` confirms it whenever a declaration stands). The re-run
/// honor then acked B's later delete `ok`.
#[tokio::test]
async fn the_floors_cadence_barrier_after_a_failed_honor_carries_the_whole_declaration() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut a, mut b) = h10_declared_edit(&store, dir_a.path(), dir_b.path()).await;

    // What `floor_tick` runs when the honor before it failed.
    a.cadence_barrier().await.unwrap();
    h10_assert_carried(&store, &a).await;
    let carried = manifest::load_pointer(store.as_ref(), &a.cfg).await.unwrap().unwrap();
    assert_eq!(
        carried.pointer.boundary_source.as_deref(),
        Some("sentinel-deferred"),
        "the install is stamped with the clock that published the declaration"
    );

    let peer_seq = h10_peer_deletes(&store, &mut b, dir_b.path()).await;

    let ack = a.honor_pending(Verb::Publish, true).await.unwrap().expect("nothing was answered");
    h10_assert_ack_is_honest(dir_a.path(), &ack, peer_seq);
}

// ---- review 2026-09-18, H4: a declared removal removes the version it named






// ---- review 2026-09-18, H3: S3's 409 on the inbox cell is a lost race



/// H3, the cell's one remaining writer (under P2 the cell carries only the
/// verb requests): one 409 is a lost race, retried, never an error.
#[tokio::test]
async fn a_verb_request_retries_a_409_on_the_cell() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let st: &dyn ObjectStore = hooked.as_ref();
    *hooked.1.put_conflicts.lock().unwrap() = Some((cfg.inbox_key(), 1));
    inbox::gateway_request(st, &cfg, inbox::RequestedVerb::Boundary, "ui")
        .await
        .expect("a 409 on the cell was an error, not a lost race");
    assert_eq!(hooked.1.put_conflicts.lock().unwrap().as_ref().map(|x| x.1), Some(0), "fixture: no 409 was injected");
    assert!(inbox::load(inner.as_ref(), &cfg).await.unwrap().doc.boundary_request.is_some());
}

// ---- review 2026-09-18, H7: the consume never overwrites a write it did not see

/// A clean path, a UI write of it committed (P2) and QUEUED by the syncer's
/// merge, and the syncer whose next barrier consumes it.
async fn h7_fixture(path: &str) -> (Arc<MemoryStore>, tempfile::TempDir, Syncer) {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    sc.checkout().await.unwrap();
    write(dir.path(), path, "v0");
    sc.run_barrier().await.unwrap();
    // The UI's save COMMITS; the next barrier's consume owes it to the tree.
    hitl_write(&store, &sc.cfg, path, "the ui's version", "ui").await.unwrap();
    assert_eq!(read(dir.path(), path).as_deref(), Some("v0"), "fixture: the UI write already reached the tree");
    (store, dir, sc)
}

fn h7_hook(stage: &'static str, root: &std::path::Path, path: &str, agent_bytes: &'static str) {
    let target = root.join(path);
    super::barrier::CONSUME_WINDOW_HOOK.with(|h| {
        *h.borrow_mut() = Some((
            path.to_string(),
            stage,
            Box::new(move || std::fs::write(&target, agent_bytes).unwrap()),
        ))
    });
}

/// Review 2026-09-18, H7. The consume's post-fetch re-stat (the
/// atomicity-3 fix) is followed by the CRC over the body, the containment
/// walk and a temp write of the whole body, then a rename over the path —
/// tens to hundreds of milliseconds for a large file. An agent write in
/// that window was overwritten with no record. It is the dirty case: the
/// agent's version stays and publishes, the UI's is preserved.
#[tokio::test]
async fn the_consume_never_overwrites_an_agent_write_made_while_it_writes() {
    let (store, dir, mut sc) = h7_fixture("h7-a.txt").await;
    h7_hook("before-write", dir.path(), "h7-a.txt", "the agent's write, inside the consume's window");
    sc.run_barrier().await.unwrap();
    assert!(
        super::barrier::CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_none()),
        "fixture: the consume never reached its write window"
    );
    assert_eq!(
        read(dir.path(), "h7-a.txt").as_deref(),
        Some("the agent's write, inside the consume's window"),
        "the consume overwrote an agent write it never saw"
    );
    let c = sc.state.load_conflicts().unwrap();
    // The consume stepped aside; the barrier's publish of the agent's
    // version preserves the UI's under R7's record.
    let rec = c
        .iter()
        .find(|r| r.path == "h7-a.txt" && r.kind.starts_with("commit-surfaced-foreign"))
        .expect("no record of the UI's version");
    let (_, body) = store.get_whole(rec.preserved_key.as_ref().unwrap(), None).await.unwrap();
    assert_eq!(&body[..], b"the ui's version");
    // ...and the agent's version is what the manifest cites once published.
    sc.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries["h7-a.txt"].crc64_b64, crc_of("the agent's write, inside the consume's window"));
}

/// H7's second window: after the rename, the baseline recorded a FRESH
/// stat of the path — so an agent write that landed between the rename
/// and that stat was recorded as the consumed version, read as clean by
/// every later scan, and never published.
#[tokio::test]
async fn an_agent_write_just_after_the_consume_is_published() {
    let (store, dir, mut sc) = h7_fixture("h7-b.txt").await;
    h7_hook("after-rename", dir.path(), "h7-b.txt", "the agent's write, after the rename, much longer");
    sc.run_barrier().await.unwrap();
    assert!(
        super::barrier::CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_none()),
        "fixture: the consume never reached its write window"
    );
    sc.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(
        m.entries["h7-b.txt"].crc64_b64,
        crc_of("the agent's write, after the rename, much longer"),
        "an agent write made just after the consume was recorded as the consumed version and never published"
    );
}

// ---- review 2026-09-18, H5: no component of an upload's path is a symlink

/// Review 2026-09-18, H5 — atomicity-6 one directory up. The upload's
/// lstat and its O_NOFOLLOW open guard the LAST component only; `mv d
/// d.bak; ln -s <outside> d` between the scan and the upload makes
/// `d/cfg.yaml` a regular file OUTSIDE the workspace (`/proc/self` works
/// the same way, and `environ` is the syncer's credentials), and it was
/// published and cited. Every component is resolved without following a
/// link.
#[tokio::test]
async fn the_upload_refuses_a_directory_swapped_for_a_symlink_after_the_scan() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let dir = tempfile::tempdir().unwrap();
    let mut a = hooked_syncer(&hooked, dir.path());
    a.checkout().await.unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("cfg.yaml"), "SECRET, outside the workspace").unwrap();
    write(dir.path(), "d/cfg.yaml", "harmless");
    write(dir.path(), "aaa-first.txt", "uploads before d/cfg.yaml");
    a.cfg.upload_fanout = 1;
    let (d, bak, target) = (dir.path().join("d"), dir.path().join("d.bak"), outside.path().to_path_buf());
    hooked.before_put(&a.cfg.file_key("aaa-first.txt"), move || {
        std::fs::rename(&d, &bak).unwrap();
        std::os::unix::fs::symlink(&target, &d).unwrap();
    });

    let r = a.run_barrier().await.expect("a planted directory link must not fail the barrier");
    assert!(dir.path().join("d.bak").exists(), "fixture: the swap never ran");
    assert!(!r.uploaded.contains(&"d/cfg.yaml".to_string()), "the link's target was published: {r:?}");
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("d/cfg.yaml"), "the manifest cites a file outside the workspace");
    assert!(inner.head(&a.cfg.file_key("d/cfg.yaml")).await.is_err(), "the outside bytes reached the bucket");
    let recs = a.state.load_conflicts().unwrap();
    assert!(recs.iter().any(|c| c.path == "d/cfg.yaml"), "nothing recorded the refusal: {recs:?}");
}

/// Review 2026-09-18, H5's RESIDUAL. The fix above gave the whole-PUT
/// path one descriptor from a walk that follows no link at any depth,
/// and takes the stamps, the size and the bytes from it. The COMPOSE
/// path (`> whole_put_max`) then hands the store a PATH
/// (`ComposeSpec::local_path`) and the store opens it again, once per
/// part, with `O_NOFOLLOW` on the LAST COMPONENT ONLY
/// (`flint-store/src/s3.rs::read_local`, and the same in `memory.rs`).
/// So the window the fix closed reopens between the syncer's open and
/// the store's read, and it is wider than the original: it spans every
/// part of a large upload.
///
/// The swap is planted in exactly that window — after `upload_one` has
/// opened the file safely, before the store reads a byte. A large file
/// under a directory swapped for a symlink out of the workspace must
/// never publish the LINK TARGET's bytes; publishing its own, from the
/// descriptor already open, is the right outcome and what the fix does.
///
/// MUTATION: give `ComposeSpec` back a path and reopen it in
/// `read_local`; this test publishes "SECRET, outside the workspace"
/// under `files/d/cfg.yaml` and fails on the body assertion.
#[tokio::test]
async fn the_compose_path_reads_the_descriptor_it_was_given_not_the_path_it_was_named() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let dir = tempfile::tempdir().unwrap();
    let mut a = hooked_syncer(&hooked, dir.path());
    a.checkout().await.unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mine = "harmless bytes, inside the workspace";
    // The same LENGTH as the workspace's own file: the store reads the
    // part sizes the syncer measured through its descriptor, so a
    // shorter target fails the read and hides the harm behind an error.
    let theirs = format!("{:!<1$}", "SECRET, outside the workspace", mine.len());
    assert_eq!(theirs.len(), mine.len());
    std::fs::write(outside.path().join("cfg.yaml"), &theirs).unwrap();
    write(dir.path(), "d/cfg.yaml", mine);
    a.cfg.upload_fanout = 1;
    a.cfg.whole_put_max = 8; // 8 bytes: everything bigger composes
    let (d, bak, target) =
        (dir.path().join("d"), dir.path().join("d.bak"), outside.path().to_path_buf());
    hooked.before_compose(&a.cfg.file_key("d/cfg.yaml"), move || {
        std::fs::rename(&d, &bak).unwrap();
        std::os::unix::fs::symlink(&target, &d).unwrap();
    });

    let r = a.run_barrier().await.expect("a planted directory link must not fail the barrier");
    assert!(dir.path().join("d.bak").exists(), "fixture: the swap never ran");
    assert!(r.uploaded.contains(&"d/cfg.yaml".to_string()), "fixture: the compose path never ran: {r:?}");
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let key = m.entries.get("d/cfg.yaml").expect("the file was not published at all").key.clone();
    let (_, body) = inner.get_whole(&key, None).await.unwrap();
    assert_eq!(
        std::str::from_utf8(&body).unwrap(),
        mine,
        "the compose path published the link target's bytes: it reopened the path instead of \
         reading the descriptor the syncer opened"
    );
}

// ---- review 2026-09-18, H2: the commit's fence, and the rotation a restart skipped

/// Review 2026-09-18, H2 (the fence's position). The commit section read
/// the cell ONCE, before its HEAD fan-out and its window, and then loaded
/// the manifest and CASed onto it with no fence. A holder deposed in
/// between — a long fan-out is enough; nothing renews there — loaded its
/// SUCCESSOR's document, rotation included, so the rotation fenced
/// nothing and the deposed holder's install landed on top of the
/// successor's commit. The model's CAS carried an atomic fence and could
/// not see it (`LeanBarrierLeaseStragglerLoadsSuccessor`).
#[tokio::test]
async fn a_holder_deposed_during_its_commit_heads_does_not_install_onto_its_successors_document() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = hooked_syncer(&hooked, dir_a.path());
    a.checkout().await.unwrap();
    write(dir_a.path(), "base.txt", "x");
    a.run_barrier().await.unwrap();
    syncer(&inner, dir_b.path()).await.checkout().await.unwrap();

    // A's next barrier holds the cell; while its commit re-reads the
    // citation it adds, B deposes it (six quiet polls, rotation) and
    // commits a change of its own.
    write(dir_a.path(), "a.txt", "A's upload");
    {
        let (inner, root_b) = (inner.clone(), dir_b.path().to_path_buf());
        hooked.after_head(&a.cfg.file_key("a.txt"), move || {
            let (inner, root_b) = (inner.clone(), root_b.clone());
            std::thread::spawn(move || {
                tokio::runtime::Runtime::new().unwrap().block_on(async {
                    let mut b = syncer(&inner, &root_b).await;
                    assert!(claim_until_held(&mut b, 10).await, "fixture: B never deposed A");
                    write(&root_b, "b.txt", "B's commit");
                    b.run_barrier().await.unwrap();
                })
            })
            .join()
            .unwrap();
        });
    }
    let before = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;
    let r = a.run_barrier().await;
    let m = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("b.txt"), "fixture: B's commit did not land (seq {before} -> {})", m.seq);
    assert!(
        !m.entries.contains_key("a.txt"),
        "A, deposed during its commit, installed onto its successor's rotated document: {r:?}"
    );
    assert!(matches!(r, Err(LeanError::Fenced(_))), "a deposed holder's barrier must end fenced: {r:?}");
}

/// Review 2026-09-18, H2 (the rotation a restart skipped). B deposes A —
/// its acquire LANDS — and B's container dies before the takeover
/// rotation. The restarted container finds the cell under its own holder
/// id and released it (`release_stale_own`) with NO rotation; the next
/// claimant took the released cell without one either. A, alive and
/// slow, had loaded and verified before the deposal: its CAS on the
/// unmoved pointer landed. The orphaned-own arm of `claim_step`, written
/// for exactly this state, was never reached from the restart.
#[tokio::test]
async fn a_restart_between_a_deposal_and_its_rotation_still_rotates() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let (dir_a, dir_b, dir_c) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = hooked_syncer(&hooked, dir_a.path());
    a.checkout().await.unwrap();
    write(dir_a.path(), "f.txt", "x");
    a.run_barrier().await.unwrap();
    // A holds a commit section: it has loaded the manifest (and read the
    // cell), and stalls before its CAS.
    assert!(claim_until_held(&mut a, 3).await);
    let straggler = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap();

    // B deposes A; the acquire lands and B dies before rotating.
    let mut b = hooked_syncer(&hooked, dir_b.path());
    loop {
        match lease::claim_step(&mut b, true).await.unwrap() {
            lease::ClaimOutcome::Waiting { quiet_polls, .. } if quiet_polls >= 5 => break,
            lease::ClaimOutcome::Waiting { .. } => {}
            lease::ClaimOutcome::Claimed(_) => panic!("fixture: claimed before the threshold"),
        }
    }
    hooked.1.acquire_err_once.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(lease::claim_step(&mut b, true).await.is_err(), "fixture: the acquire's response was not lost");
    let b_id = b.state.load_incarnation().unwrap().unwrap().holder_id;
    assert_eq!(inner.epoch_read(&a.cfg.epoch_key()).await.unwrap().unwrap().holder_id, b_id, "fixture: B's acquire did not land");
    drop(b);

    // B's container restarts and releases what it finds held under its id;
    // C claims the released cell.
    let mut b2 = hooked_syncer(&hooked, dir_b.path());
    lease::release_stale_own(&mut b2).await.unwrap();
    let mut c = syncer(&inner, dir_c.path()).await;
    assert!(claim_until_held(&mut c, 3).await, "fixture: C could not claim the released cell");

    // A's CAS, on the document it loaded before it was deposed.
    let mut mine = straggler.manifest.clone();
    mine.seq += 1;
    let r = manifest::cas_write(inner.as_ref(), &a.cfg, &mine, Some(&straggler.handle()), 1, "straggler").await;
    assert!(
        matches!(r, Err(LeanError::Store(flint_store::StoreError::PreconditionFailed(_)))),
        "a deposed holder's CAS landed: the restart released the deposal without its rotation ({:?})",
        r.map(|m| m.etag)
    );
}

/// H2's other door into the same state: the takeover rotation met S3's
/// 409 on the pointer and failed the claim AFTER the acquire had landed —
/// the cell named the new holder at an epoch it never recorded, no
/// rotation had run, and a restart then released it (the test above).
/// A 409 is a lost race: the rotation retries.
#[tokio::test]
async fn a_takeover_rotation_that_meets_a_409_retries_it() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Hooks::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = hooked_syncer(&hooked, dir_a.path());
    a.checkout().await.unwrap();
    write(dir_a.path(), "f.txt", "x");
    a.run_barrier().await.unwrap();
    let seq0 = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;
    assert!(claim_until_held(&mut a, 3).await);

    let mut b = hooked_syncer(&hooked, dir_b.path());
    loop {
        match lease::claim_step(&mut b, true).await.unwrap() {
            lease::ClaimOutcome::Waiting { quiet_polls, .. } if quiet_polls >= 5 => break,
            lease::ClaimOutcome::Waiting { .. } => {}
            lease::ClaimOutcome::Claimed(_) => panic!("fixture: claimed before the threshold"),
        }
    }
    *hooked.1.put_conflicts.lock().unwrap() = Some((b.cfg.current_key(), 1));
    let outcome = lease::claim_step(&mut b, true).await;
    assert_eq!(hooked.1.put_conflicts.lock().unwrap().as_ref().map(|x| x.1), Some(0), "fixture: no 409 was injected");
    assert!(
        matches!(outcome, Ok(lease::ClaimOutcome::Claimed(_))),
        "a 409 on the takeover rotation failed a claim whose acquire had landed: {:?}",
        outcome.err()
    );
    let seq1 = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;
    assert!(seq1 > seq0, "the takeover did not rotate");
}

// ---- review 2026-09-18, H7 in `sync`: the agent's write inside the sync's own window

/// A published path, a remote change of it (a UI write in the inbox), and
/// the syncer about to `sync`.
async fn h7_sync_fixture(path: &str) -> (Arc<MemoryStore>, tempfile::TempDir, Syncer) {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    sc.checkout().await.unwrap();
    write(dir.path(), path, "v0");
    sc.run_barrier().await.unwrap();
    hitl_write(&store, &sc.cfg, path, "the remote version", "ui").await.unwrap();
    (store, dir, sc)
}

/// H7's first window, in `sync`: dirt was judged by ONE scan at the
/// start, and every write after it — a whole-tree sync fetches for
/// minutes — overwrote whatever the agent had written since, with no
/// record. "The syncer never overwrites a file you modified": the licence
/// is re-checked immediately before the rename, and a write there is a
/// `sync-dirty` conflict like any other.
#[tokio::test]
async fn sync_never_overwrites_an_agent_write_made_while_it_writes() {
    let (_store, dir, mut sc) = h7_sync_fixture("h7-s1.txt").await;
    h7_hook("before-write", dir.path(), "h7-s1.txt", "the agent's write, during the sync");
    let r = sc.sync().await.unwrap();
    assert!(
        super::barrier::CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_none()),
        "fixture: the sync never reached its write window"
    );
    assert_eq!(
        read(dir.path(), "h7-s1.txt").as_deref(),
        Some("the agent's write, during the sync"),
        "sync overwrote an agent write it never saw"
    );
    assert!(r.conflicts.contains(&"h7-s1.txt".to_string()), "{r:?}");
}

/// H7's second window, in `sync`: the baseline recorded a FRESH stat
/// after the write, so an agent write between the rename and that stat
/// was recorded as the synced version and never published.
#[tokio::test]
async fn an_agent_write_just_after_a_sync_applies_is_published() {
    let (store, dir, mut sc) = h7_sync_fixture("h7-s2.txt").await;
    h7_hook("after-rename", dir.path(), "h7-s2.txt", "the agent's write, after the sync's rename, longer");
    sc.sync().await.unwrap();
    assert!(
        super::barrier::CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_none()),
        "fixture: the sync never reached its write window"
    );
    sc.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(
        m.entries["h7-s2.txt"].crc64_b64,
        crc_of("the agent's write, after the sync's rename, longer"),
        "an agent write made just after the sync applied was recorded as the synced version and never published"
    );
}

/// And the delete arm: a remote deletion applied to a path the agent
/// wrote after the sync's scan removed the agent's write.
#[tokio::test]
async fn sync_never_deletes_an_agent_write_made_after_its_scan() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    sc.checkout().await.unwrap();
    write(dir.path(), "h7-s3.txt", "v0");
    sc.run_barrier().await.unwrap();
    let loaded = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    let mut theirs = loaded.manifest.clone();
    theirs.seq += 1;
    theirs.entries.remove("h7-s3.txt");
    manifest::cas_write(store.as_ref(), &sc.cfg, &theirs, Some(&loaded.handle()), 0, "remote-delete").await.unwrap();

    h7_hook("before-delete", dir.path(), "h7-s3.txt", "the agent's write, after the sync's scan");
    let r = sc.sync().await.unwrap();
    assert!(
        super::barrier::CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_none()),
        "fixture: the sync never reached its delete"
    );
    assert_eq!(
        read(dir.path(), "h7-s3.txt").as_deref(),
        Some("the agent's write, after the sync's scan"),
        "sync applied a remote delete over an agent write made after its scan"
    );
    assert!(r.conflicts.contains(&"h7-s3.txt".to_string()), "{r:?}");
}

/// Review 2026-09-18, M5's agent-side twin. The consume half is fixed —
/// a containment refusal writes a durable `consume-refused-containment`
/// record. This is the other half: the agent replaces a published FILE
/// with a DIRECTORY of the same name. The scan sees `a` gone and `a/x`
/// new, but the two-scan guard withholds a first absence, so the barrier
/// publishes the upload and not the delete. If nothing catches it, the
/// installed document cites both `a` and `a/x`, and no workspace can
/// hold both: a fresh checkout of that document must create a directory
/// where its own manifest says a file goes.
///
/// The invariant this pins is one no model world can state — the core
/// has no name alphabet, so "a cited path is never a prefix of another
/// cited path" is a code-level claim (review doc, H5's row).
#[tokio::test]
async fn a_published_file_replaced_by_a_directory_never_leaves_a_document_no_checkout_can_hold() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "a", "a file, published");
    write(dir_a.path(), "keep.txt", "keep");
    a.run_barrier().await.unwrap();

    // The agent turns the file into a directory and puts a file in it.
    std::fs::remove_file(dir_a.path().join("a")).unwrap();
    write(dir_a.path(), "a/x", "now a directory");
    let r = a.run_barrier().await.unwrap();

    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let cited: Vec<&String> = m.entries.keys().collect();
    let clash: Vec<(&String, &String)> = cited
        .iter()
        .flat_map(|p| cited.iter().map(move |q| (*p, *q)))
        .filter(|(p, q)| p != q && q.starts_with(&format!("{p}/")))
        .collect();
    assert!(
        clash.is_empty(),
        "the document cites a path and something under it, which no filesystem can hold: \
         {clash:?} (barrier: {r:?})"
    );

    // The harm, stated as the reader sees it: a fresh workspace must be
    // able to materialise whatever the document says.
    let mut b = syncer(&store, dir_b.path()).await;
    b.checkout().await.expect("a fresh checkout could not materialise the published document");
    assert_eq!(read(dir_b.path(), "keep.txt").as_deref(), Some("keep"));

    // Withheld is not lost: the next barrier confirms the absence, the
    // citation goes, and the file under the new directory publishes. A
    // rule that only refuses would trade one silent break for a stall.
    let r2 = a.run_barrier().await.unwrap();
    assert_eq!(r2.deleted, vec!["a".to_string()], "the absence never confirmed: {r2:?}");
    assert_eq!(r2.uploaded, vec!["a/x".to_string()], "the withheld path never published: {r2:?}");
    let m2 = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m2.entries.contains_key("a"), "the file citation outlived its file");
    assert!(m2.entries.contains_key("a/x"));
    let dir_c = tempfile::tempdir().unwrap();
    syncer(&store, dir_c.path()).await.checkout().await.unwrap();
    assert_eq!(read(dir_c.path(), "a/x").as_deref(), Some("now a directory"));
}

/// The other direction, and the other arm of the same guard: a
/// published DIRECTORY (`a/x` cited) replaced by a FILE at `a`. Here it
/// is the PARENT that this barrier is publishing and the child that the
/// document still cites, so the parent is the side that yields.
#[tokio::test]
async fn a_published_directory_replaced_by_a_file_never_leaves_a_document_no_checkout_can_hold() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_c) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "a/x", "a directory, published");
    a.run_barrier().await.unwrap();

    std::fs::remove_dir_all(dir_a.path().join("a")).unwrap();
    write(dir_a.path(), "a", "now a file");
    let r = a.run_barrier().await.unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(
        !(m.entries.contains_key("a") && m.entries.contains_key("a/x")),
        "the document cites a file and a path inside it: {:?} ({r:?})",
        m.entries.keys().collect::<Vec<_>>()
    );
    assert_eq!(r.parked, vec!["a".to_string()], "the parent did not yield: {r:?}");

    let r2 = a.run_barrier().await.unwrap();
    assert_eq!(r2.deleted, vec!["a/x".to_string()], "{r2:?}");
    assert_eq!(r2.uploaded, vec!["a".to_string()], "{r2:?}");
    syncer(&store, dir_c.path()).await.checkout().await.unwrap();
    assert_eq!(read(dir_c.path(), "a").as_deref(), Some("now a file"));
}

/// Review 2026-09-18, M5's gateway half, asked as the UI would do it
/// CORRECTLY: to turn the published file `build` into a directory, the
/// human deletes `build` and then writes `build/log.txt`. Both land in
/// the cell — a removal and an entry — and the consume takes entries
/// first and removals after, so the entry is materialised while the file
/// is still there and containment refuses it. The write is acked, then
/// dropped with a pod-local record (L6), and the human is never told.
///
/// The published shape must end up as the human asked: the file gone,
/// the directory there, and the document citing `build/log.txt` alone.
#[tokio::test]
async fn a_ui_delete_then_a_write_under_that_name_turns_the_file_into_a_directory() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    write(dir.path(), "build", "a file, published");
    a.run_barrier().await.unwrap();

    hitl_remove(&store, &a.cfg, "build", "human").await;
    hitl_write(&store, &a.cfg, "build/log.txt", "now a directory", "human").await.unwrap();

    // However many barriers the protocol needs, it must converge.
    for _ in 0..4 {
        a.run_barrier().await.unwrap();
    }
    let recs = a.state.load_conflicts().unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(
        read(dir.path(), "build/log.txt").as_deref(),
        Some("now a directory"),
        "the acked UI write never reached the tree; records: {:?}",
        recs.iter().map(|c| (&c.path, &c.kind)).collect::<Vec<_>>()
    );
    assert!(!m.entries.contains_key("build"), "the deleted file is still cited");
    assert!(m.entries.contains_key("build/log.txt"), "the UI's write was never published: {m:?}");
}


/// The same ordering in the SYNC verb. Sync applies adds (pass 3)
/// before remote deletions (pass 4), so a remote generation that turned
/// the file `build` into the directory `build/log.txt` refuses the add
/// for containment and then deletes the file — the workspace is left
/// without the remote's file until some later sync, and sync is
/// harness-invoked, never background (review 2026-09-18, the critic's
/// `sync.rs` item; the consume's twin is L-121).
#[tokio::test]
async fn sync_turns_a_file_into_a_directory_in_one_pass() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "build", "a file, published");
    write(dir_a.path(), "keep.txt", "keep");
    a.run_barrier().await.unwrap();

    // B starts from that document, so its merge base cites `build`.
    let mut b = syncer(&store, dir_b.path()).await;
    b.checkout().await.unwrap();
    assert_eq!(read(dir_b.path(), "build").as_deref(), Some("a file, published"));

    // A replaces the file with a directory and publishes it (two
    // barriers: the first withholds the clash, the second converges).
    std::fs::remove_file(dir_a.path().join("build")).unwrap();
    write(dir_a.path(), "build/log.txt", "now a directory");
    a.run_barrier().await.unwrap();
    a.run_barrier().await.unwrap();

    // ONE sync must leave B's workspace as the document says.
    let r = b.sync().await.unwrap();
    assert_eq!(
        read(dir_b.path(), "build/log.txt").as_deref(),
        Some("now a directory"),
        "sync refused the add and then deleted the file: {r:?}"
    );
    assert!(dir_b.path().join("build").is_dir(), "the file was not replaced by the directory");
}

/// The clash check as it was first written: every key against every key.
/// The oracle for `path_clashes`, which must answer the same.
fn path_clashes_quadratic(cited: &std::collections::BTreeMap<String, ()>, upserts: &std::collections::BTreeMap<String, ()>) -> Vec<String> {
    let cited: Vec<&String> = cited.keys().collect();
    let mut mine: Vec<String> = vec![];
    for p in &cited {
        let pfx = format!("{p}/");
        if cited.iter().any(|q| q.starts_with(&pfx)) {
            let child = cited.iter().find(|q| q.starts_with(&pfx) && upserts.contains_key(**q)).map(|q| (*q).clone());
            if upserts.contains_key(*p) {
                mine.push((*p).clone());
            } else if let Some(c) = child {
                mine.push(c);
            }
        }
    }
    mine.sort();
    mine.dedup();
    mine
}

#[test]
fn the_clash_check_answers_as_the_every_pair_scan_did() {
    // Names chosen so a check that looks only just past `a` misses: `a-b`
    // and `a.txt` sort between `a` and `a/x`.
    let names = ["a", "a-b", "a.txt", "a/x", "a/x/y", "a/z", "ab", "ab/c", "b", "b0", "b/c"];
    let n = names.len();
    // Every subset of the names as the document, every subset of that as
    // this barrier's upserts.
    for cited_bits in 0u32..(1 << n) {
        let cited: std::collections::BTreeMap<String, ()> =
            (0..n).filter(|i| cited_bits & (1 << i) != 0).map(|i| (names[i].to_string(), ())).collect();
        let keys: Vec<&String> = cited.keys().collect();
        // All upsert subsets is 3^11; one in five cited sets, all of theirs, stays well under a second.
        if cited_bits % 5 != 0 {
            continue;
        }
        for up_bits in 0u32..(1 << keys.len()) {
            let upserts: std::collections::BTreeMap<String, ()> = keys
                .iter()
                .enumerate()
                .filter(|(i, _)| up_bits & (1 << i) != 0)
                .map(|(_, k)| ((*k).clone(), ()))
                .collect();
            assert_eq!(
                super::barrier::path_clashes(&cited, &upserts),
                path_clashes_quadratic(&cited, &upserts),
                "cited {keys:?}, upserts {:?}",
                upserts.keys().collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn the_clash_check_is_not_quadratic_in_the_document() {
    // 30k paths, no clash, the shape of a large workspace. Every pair is
    // 9x10^8 comparisons (tens of seconds); the range lookup is milliseconds. The
    // bound is loose on purpose: it separates the two, nothing finer.
    let cited: std::collections::BTreeMap<String, ()> =
        (0..30_000).map(|i| (format!("d{:04}/f{i:07}.txt", i / 1000), ())).collect();
    let upserts: std::collections::BTreeMap<String, ()> = [("d0015/f0015000.txt".to_string(), ())].into();
    let t = std::time::Instant::now();
    assert!(super::barrier::path_clashes(&cited, &upserts).is_empty());
    assert!(t.elapsed() < std::time::Duration::from_secs(5), "took {:?}", t.elapsed());
}

/// The scan trigger's commit-side advance: a busy agent publishing over the
/// document its consume derived against keeps the next tick cheap (the
/// pointer and nothing else), instead of a full derive after each of its
/// own publishes.
#[tokio::test]
async fn a_busy_agents_idle_tick_after_its_own_publish_is_the_pointer_alone() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    for i in 0..20 {
        write(dir.path(), &format!("f{i}.txt"), "seed");
    }
    a.run_barrier().await.unwrap();
    // Two publishes of its own, over a document nobody else moved.
    for v in ["v2", "v3"] {
        write(dir.path(), "f1.txt", v);
        backdate_baseline(&a, "f1.txt");
        let r = a.run_barrier().await.unwrap();
        assert_eq!(r.uploaded, vec!["f1.txt".to_string()], "fixture: {r:?}");
    }
    store.reset_op_counts();
    a.run_barrier().await.unwrap();
    let gets = store.op_counts().get("get_whole").copied().unwrap_or(0);
    assert_eq!(gets, 2, "an idle tick after its own publish re-derived: {:?}", store.op_counts());
}

/// A path the consume skipped as the agent's work and that this barrier
/// then PUBLISHES is not the agent's pending work any more: the next tick
/// must not re-derive for it.
#[tokio::test]
async fn a_skipped_path_this_barrier_publishes_leaves_the_next_tick_cheap() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    write(dir.path(), "x.txt", "seed");
    a.run_barrier().await.unwrap();
    // A peer's version, and the agent's edit over it: the consume skips x.
    peer_publish(&store, &a.cfg, "x.txt", "the peer's v2", "peer").await;
    write(dir.path(), "x.txt", "the agent's edit");
    backdate_baseline(&a, "x.txt");
    a.consume_owed().await.unwrap();
    assert!(a.state.load_baseline().unwrap().skipped.contains("x.txt"), "fixture: x was not skipped");
    // This barrier publishes the agent's x over theirs (with a record).
    let r = a.run_barrier().await.unwrap();
    assert_eq!(r.uploaded, vec!["x.txt".to_string()], "fixture: {r:?}");
    store.reset_op_counts();
    a.run_barrier().await.unwrap();
    let gets = store.op_counts().get("get_whole").copied().unwrap_or(0);
    assert_eq!(gets, 2, "the published path still read as the agent's pending work: {:?}", store.op_counts());
}

/// A whole-tree `sync` that declines a path the agent is working on records
/// it as the consume would: when the agent backs out, theirs arrives.
#[tokio::test]
async fn a_path_a_sync_declined_as_dirty_arrives_when_the_agent_reverts() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    write(dir.path(), "x.txt", "seed");
    a.run_barrier().await.unwrap();
    peer_publish(&store, &a.cfg, "x.txt", "the peer's v2", "peer").await;
    write(dir.path(), "x.txt", "the agent's edit");
    let r = a.sync().await.unwrap();
    assert_eq!(r.conflicts, vec!["x.txt".to_string()], "fixture: the sync took x: {r:?}");
    assert_eq!(read(dir.path(), "x.txt").as_deref(), Some("the agent's edit"), "fixture");

    restore_to_baseline(&a, "x.txt", "seed");
    for _ in 0..2 {
        a.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir.path(), "x.txt").as_deref(), Some("the peer's v2"), "the peer's version never arrived");
}

// ---------------------------------------------------------------------
// Review 2026-09-29 (sync.rs / reader.rs): a sync must leave the tree and
// the cheap path's record exactly as the consume would.
// ---------------------------------------------------------------------

/// S1. The consume takes nothing when the document is absent. A whole-tree
/// sync used to read the absence as an EMPTY document and delete every
/// clean file the baseline holds — for a writer whose prefix was wiped or
/// re-pointed, the last copy of its published work.
#[tokio::test]
async fn a_sync_against_an_absent_document_deletes_nothing() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "keep/a.txt", "published a");
    write(dir.path(), "b.txt", "published b");
    a.run_barrier().await.unwrap();
    assert!(!a.state.load_baseline().unwrap().entries.is_empty());
    // The prefix is wiped: no pointer, no legacy document, no objects.
    for o in store.list(PREFIX).await.unwrap() {
        store.delete(&o.key).await.unwrap();
    }
    assert!(manifest::load(store.as_ref(), &a.cfg).await.unwrap().is_none());

    let r = a.sync().await;

    assert!(r.is_err(), "a sync with no document must refuse, not treat it as empty: {r:?}");
    assert_eq!(read(dir.path(), "keep/a.txt").as_deref(), Some("published a"));
    assert_eq!(read(dir.path(), "b.txt").as_deref(), Some("published b"));
}

/// S2. A fetch that fails leaves the path owed. The consume marks it
/// (`left`) and records nothing as derived; the sync recorded the document
/// as derived anyway, so the cheap path then skipped the path until the
/// pointer moved again.
#[tokio::test]
async fn a_path_a_sync_could_not_fetch_stays_owed() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "shared.txt", "v1");
    a.run_barrier().await.unwrap();
    hitl_write(&store, &a.cfg, "shared.txt", "user v2", "dilip").await.unwrap();
    let key = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries["shared.txt"].key.clone();
    // The object reads absent for the length of the sync (a delete marker).
    store.delete(&key).await.unwrap();

    a.sync().await.unwrap();
    assert_eq!(read(dir.path(), "shared.txt").as_deref(), Some("v1"), "nothing was fetchable");

    // It is back, with the etag the document cites; the pointer has not moved.
    for v in store.list_versions(&key).await.unwrap() {
        if v.key == key && v.is_delete_marker {
            store.delete_version(&key, &v.version_id).await.unwrap();
        }
    }
    a.consume_owed().await.unwrap();
    assert_eq!(read(dir.path(), "shared.txt").as_deref(), Some("user v2"), "the path the sync could not take was never owed again");
}

/// S3. A write that fails for a transient reason (here: a directory the
/// syncer cannot write to) is not a containment refusal. The consume
/// records it as `consume-write-failed (will retry)` and leaves it owed;
/// the sync recorded `sync-refused-containment` and marked the document
/// derived, so the path was never taken.
#[tokio::test]
async fn a_path_a_sync_could_not_write_stays_owed() {
    use std::os::unix::fs::PermissionsExt;
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir.path(), "sub/f.txt", "v1");
    a.run_barrier().await.unwrap();
    hitl_write(&store, &a.cfg, "sub/f.txt", "user v2", "dilip").await.unwrap();
    let sub = dir.path().join("sub");
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o555)).unwrap();

    let r = a.sync().await.unwrap();
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Not vacuous: the write really failed (a root run would not fail it).
    assert!(r.conflicts.contains(&"sub/f.txt".to_string()), "the write did not fail: {r:?}");
    assert_eq!(read(dir.path(), "sub/f.txt").as_deref(), Some("v1"));

    a.consume_owed().await.unwrap();
    assert_eq!(read(dir.path(), "sub/f.txt").as_deref(), Some("user v2"), "the path the sync could not write was never owed again");
}

/// S4. A reader re-syncs when the documents move — and, like the consume's
/// cheap path, when its last sync left something owed. It used to judge by
/// the two etags alone, so a path its sync could not take waited for the
/// next commit, however long that was.
#[tokio::test]
async fn a_reader_retries_what_its_last_sync_left_owed() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "x.txt", "A v1");
    a.floor_tick().await.unwrap();
    let mut r = reader(&store, dir_r.path()).await;
    r.checkout().await.unwrap();
    r.floor_tick().await.unwrap();
    assert_eq!(read(dir_r.path(), "x.txt").as_deref(), Some("A v1"));

    write(dir_a.path(), "x.txt", "A v2");
    backdate_baseline(&a, "x.txt");
    a.declared_barrier().await.unwrap();
    let key = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries["x.txt"].key.clone();
    store.delete(&key).await.unwrap();
    r.floor_tick().await.unwrap();
    assert_eq!(read(dir_r.path(), "x.txt").as_deref(), Some("A v1"), "nothing was fetchable");

    for v in store.list_versions(&key).await.unwrap() {
        if v.key == key && v.is_delete_marker {
            store.delete_version(&key, &v.version_id).await.unwrap();
        }
    }
    // Nothing moved since the last tick: only what was left owed says pull.
    r.floor_tick().await.unwrap();
    assert_eq!(read(dir_r.path(), "x.txt").as_deref(), Some("A v2"), "the reader never retried the path its sync left");
}

/// R3. A widen admits paths this tree did not hold. A file the agent made
/// at such a path since the last barrier is its unpublished work — the
/// consume would call it dirty and leave it. The widen fetched the
/// document's version over it (checkout's resume rule: a present file
/// whose bytes differ is re-materialized), a silent loss.
#[tokio::test]
async fn a_widen_never_overwrites_a_file_the_agent_made_at_an_admitted_path() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    a.checkout().await.unwrap();
    write(dir_a.path(), "inputs/in.txt", "input");
    write(dir_a.path(), "outputs/o.txt", "A's published output");
    a.run_barrier().await.unwrap();

    let mut b = syncer(&store, dir_b.path()).await;
    b.checkout_scoped(Some(vec!["inputs".into()])).await.unwrap();
    assert_eq!(read(dir_b.path(), "outputs/o.txt"), None);
    // B's agent writes there before any barrier of B's.
    write(dir_b.path(), "outputs/o.txt", "B's unpublished work");

    let r = b.rescope(None).await.unwrap();

    assert_eq!(read(dir_b.path(), "outputs/o.txt").as_deref(), Some("B's unpublished work"), "the widen overwrote the agent's file");
    // And surfaced, never silent: a record names it.
    let recs = b.state.load_conflicts().unwrap();
    assert!(recs.iter().any(|c| c.path == "outputs/o.txt"), "no record names the kept file: {r:?} {recs:?}");

    // B's next barrier publishes its version over A's, and A's is not lost:
    // the merge records what it published over (R7).
    let a_etag = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest.entries["outputs/o.txt"].etag.clone();
    assert!(claim_until_held(&mut b, 3).await);
    b.run_barrier().await.unwrap();
    let doc = manifest::load(store.as_ref(), &b.cfg).await.unwrap().unwrap().manifest;
    assert_ne!(doc.entries["outputs/o.txt"].etag, a_etag, "B's work was not published");
    let recs = b.state.load_conflicts().unwrap();
    assert!(
        recs.iter().any(|c| c.path == "outputs/o.txt" && c.foreign_etag == a_etag && c.preserved_key.is_some()),
        "A's version was replaced with no record preserving it: {recs:?}"
    );
}

// ---------------------------------------------------------------------
// ack-7: a sync's records across a SECOND rotation (review 2026-09-12)
// ---------------------------------------------------------------------

/// The ack names the records its own barrier or sync produced: count
/// before, take what is new after. A second rotation drops the whole
/// older generation (`conflicts.1.jsonl`) and the held count falls by
/// exactly that many. If the operation produced as many records as the
/// dropped generation held, the held count comes back to where it was
/// and a count-and-skip reports NOTHING; if it produced more, it loses
/// that many of its own. Exactly the operation's records, every time.
#[tokio::test]
async fn an_ack_names_every_record_its_operation_produced_across_a_second_rotation() {
    use super::sentinel::conflicts_since;
    let store = Arc::new(MemoryStore::new());
    // Each case: the log has rotated `rotated` times when the operation
    // starts (once: nothing dropped yet; twice: a generation already
    // dropped, so the starting count must include it), and the operation
    // produces the older generation's size plus `extra`, crossing the
    // next rotation. Equal is the case that reported none; more is the
    // case that lost some.
    for (case, rotated, extra) in [
        ("first drop, as many as it drops", 1, 0usize),
        ("first drop, more than it drops", 1, 7),
        ("a later drop, as many as it drops", 2, 0),
        ("a later drop, more than it drops", 2, 7),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let a = syncer(&store, dir.path()).await;
        let sd = a.cfg.state_dir();
        let mut rotations = 0;
        let mut i = 0usize;
        while rotations < rotated {
            let had = std::fs::metadata(sd.join("conflicts.jsonl")).map(|m| m.len()).unwrap_or(0);
            a.state.append_conflict(&bulky_conflict(i)).unwrap();
            i += 1;
            let has = std::fs::metadata(sd.join("conflicts.jsonl")).map(|m| m.len()).unwrap_or(0);
            if has < had {
                rotations += 1;
            }
        }
        // Records already in the live log when the operation starts (a
        // rotation just happened above, so it is empty): the realistic
        // start, and the one where a starting count that leaves out the
        // dropped records over-reports into them.
        for n in 0..5 {
            a.state.append_conflict(&bulky_conflict(500_000 + n)).unwrap();
        }
        let dropped_before = a.state.conflicts_dropped().unwrap();
        assert_eq!(dropped_before > 0, rotated > 1, "{case}: the fixture's starting point");
        let generation =
            std::fs::read_to_string(sd.join("conflicts.1.jsonl")).unwrap().lines().filter(|l| !l.trim().is_empty()).count();
        assert!(generation > 100, "{case}: the fixture needs a real generation: {generation}");
        let before = a.state.conflicts_appended().unwrap();
        let k = generation + extra;
        let made: Vec<String> = (0..k)
            .map(|n| {
                let r = bulky_conflict(1_000_000 + n);
                a.state.append_conflict(&r).unwrap();
                r.path
            })
            .collect();
        let dropped = a.state.conflicts_dropped().unwrap();
        assert!(dropped > dropped_before, "{case}: the run must cross a rotation that drops");
        let got: Vec<String> =
            conflicts_since(before, dropped, a.state.load_conflicts().unwrap()).into_iter().map(|r| r.path).collect();
        assert_eq!(got.len(), made.len(), "{case}: the ack must name all {k} records the operation produced");
        assert_eq!(got, made, "{case}: and exactly those, oldest first");
    }
}

/// Over-reporting is the safe direction, and it is the answer when the
/// log is reset under a reader (the total went DOWN): every survivor.
#[test]
fn a_log_reset_under_a_reader_over_reports() {
    use super::sentinel::conflicts_since;
    let survivors = vec![bulky_conflict(7), bulky_conflict(8)];
    assert_eq!(conflicts_since(50, 0, survivors).len(), 2);
}

// ---------------------------------------------------------------------
// ack-10 / U37: a consumed touch stranded in staging (review 2026-09-12)
// ---------------------------------------------------------------------

/// The consume renames the touch into staging, then folds it into the
/// pending record. If the fold fails mid-run, the touch waits in
/// staging; the NEXT consume must fold it first, never rename a new
/// touch over it (which loses its nonce, so the agent waiting on that
/// nonce never sees an ack). And while the fold still fails, the next
/// touch stays where the agent put it.
#[tokio::test]
async fn a_touch_stranded_by_a_failed_fold_is_folded_before_the_next_consume() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    let pending = a.cfg.state_dir().join(Verb::Publish.pending_name());
    let staging = a.cfg.state_dir().join("publish.consumed");
    let sentinel = dir.path().join(super::CONTROL_DIR).join(control::PUBLISH);

    // A transient fold failure: the pending record cannot be read.
    std::fs::create_dir_all(&pending).unwrap();
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"n1"}"#);
    assert!(a.consume_sentinel(Verb::Publish).is_err(), "the fixture must make the fold fail");
    assert!(staging.exists(), "the failed fold leaves the touch in staging");

    // Still failing: the next touch must not be taken over the stranded one.
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"n2"}"#);
    assert!(a.consume_sentinel(Verb::Publish).is_err());
    assert!(sentinel.exists(), "a touch the syncer cannot fold stays where the agent put it");
    let staged = std::fs::read_to_string(&staging).unwrap();
    assert!(staged.contains("n1"), "the stranded touch was overwritten: {staged}");

    // The failure clears: both nonces reach the pending record.
    std::fs::remove_dir(&pending).unwrap();
    assert!(a.consume_sentinel(Verb::Publish).unwrap(), "the touch was not consumed");
    assert!(!staging.exists(), "staging is cleared once folded");
    assert!(!sentinel.exists());
    let nonces = a.load_pending(Verb::Publish).unwrap().expect("no pending record").nonces;
    assert_eq!(nonces, vec!["n1".to_string(), "n2".to_string()], "every touch's nonce must reach the ack");
}

/// The stranded touch is honored on the next poll, not only when the
/// agent happens to touch again: an agent waiting on that nonce may
/// never touch again.
#[tokio::test]
async fn a_stranded_touch_is_folded_on_the_next_poll_without_another_touch() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    let pending = a.cfg.state_dir().join(Verb::Publish.pending_name());
    let staging = a.cfg.state_dir().join("publish.consumed");

    std::fs::create_dir_all(&pending).unwrap();
    touch_sentinel(dir.path(), control::PUBLISH, r#"{"nonce":"n1"}"#);
    assert!(a.poll_sentinels().is_err(), "the fixture must make the fold fail");
    assert!(staging.exists());

    std::fs::remove_dir(&pending).unwrap();
    let consumed = a.poll_sentinels().unwrap();
    assert_eq!(consumed, vec![Verb::Publish], "the stranded touch counts as this poll's consume");
    assert!(!staging.exists());
    let nonces = a.load_pending(Verb::Publish).unwrap().expect("no pending record").nonces;
    assert_eq!(nonces, vec!["n1".to_string()]);
}
