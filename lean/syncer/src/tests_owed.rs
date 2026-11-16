//! What a live tree is owed from the document is fetched the way a
//! checkout fetches (`Syncer::take_owed`): together, and a large object
//! in ranges straight to its temp file. The consume and the sync used to
//! make one whole-object GET at a time, each body held in memory.
//!
//! The fetch runs off the task that decided what is owed, so the two
//! rules that task used to keep inline are tested here on the new path:
//! an agent write made while an object is fetched is never renamed over
//! (H7), and a store that fails stops the pass instead of being asked
//! for everything else first.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use flint_store::memory::MemoryStore;

use super::manifest;
use super::tests::{cited_key, derive_pending, hooked_syncer, read, syncer, write, Hooked};
use super::Syncer;

/// How many small files the peer publishes at once.
const OWED: usize = 24;

fn name(i: usize) -> String {
    format!("in/f{i:02}.txt")
}

/// Two trees on one workspace, B checked out and holding nothing, and
/// then A publishes `OWED` small files: B owes every one.
async fn a_peers_bulk_arrival(store: &Arc<MemoryStore>, a: &mut Syncer, b: &mut Syncer) {
    a.checkout().await.unwrap();
    b.checkout().await.unwrap();
    for i in 0..OWED {
        write(&a.cfg.root.clone(), &name(i), &format!("the peer's file {i}"));
    }
    a.run_barrier().await.unwrap();
    let doc = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(doc.entries.len(), OWED, "fixture: the peer's barrier did not publish every file");
}

/// The consume fetches the files it owes together. Only a delay makes the
/// overlap observable: without one a request is over before the next is
/// sent, whoever sends them.
#[tokio::test]
async fn a_consume_fetches_what_it_is_owed_together() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a_peers_bulk_arrival(&store, &mut a, &mut b).await;

    store.inject_request_delay_ms(5);
    store.reset_peak_requests_in_flight();
    let (taken, _) = b.consume_owed().await.unwrap();
    store.inject_request_delay_ms(0);
    assert_eq!(taken, OWED);
    for i in 0..OWED {
        assert_eq!(read(dir_b.path(), &name(i)), Some(format!("the peer's file {i}")));
    }
    let peak = store.peak_requests_in_flight();
    assert!(peak as usize >= OWED / 2, "{OWED} owed files were fetched {peak} at a time");
    assert!(!derive_pending(&b).await, "everything was taken, and the consume still reads as behind");
}

/// The sync does too, and its report still names what it applied in path
/// order.
#[tokio::test]
async fn a_sync_fetches_what_it_is_owed_together() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a_peers_bulk_arrival(&store, &mut a, &mut b).await;

    store.inject_request_delay_ms(5);
    store.reset_peak_requests_in_flight();
    let r = b.sync().await.unwrap();
    store.inject_request_delay_ms(0);
    assert_eq!(r.applied, (0..OWED).map(name).collect::<Vec<_>>(), "{r:?}");
    assert!(r.conflicts.is_empty(), "{r:?}");
    for i in 0..OWED {
        assert_eq!(read(dir_b.path(), &name(i)), Some(format!("the peer's file {i}")));
    }
    let peak = store.peak_requests_in_flight();
    assert!(peak as usize >= OWED / 2, "{OWED} owed files were fetched {peak} at a time");
}

/// What is owed is decided, fetched and recorded a batch of paths at a
/// time (`take_batch`), so a bulk arrival is not held in memory. A batch's
/// edge loses nothing and reorders nothing: with five paths to a batch,
/// twenty-four files arrive, some of them the agent's own, and the sync's
/// report names them in path order.
#[tokio::test]
async fn a_batch_edge_loses_nothing_and_keeps_path_order() {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b, dir_c) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    let mut c = syncer(&store, dir_c.path()).await;
    c.checkout().await.unwrap();
    a_peers_bulk_arrival(&store, &mut a, &mut b).await;
    b.cfg.take_batch = 5;
    c.cfg.take_batch = 5;
    // The agent's own files where a peer's are owed, one on a batch's
    // last slot and one on the next batch's first: not fetched, not lost.
    for i in [4, 5, 17] {
        write(dir_b.path(), &name(i), "the agent's own");
        write(dir_c.path(), &name(i), "the agent's own");
    }

    let (taken, _) = b.consume_owed().await.unwrap();
    assert_eq!(taken, OWED - 3);
    let r = c.sync().await.unwrap();
    let theirs: Vec<String> = (0..OWED).filter(|i| ![4, 5, 17].contains(i)).map(name).collect();
    assert_eq!(r.applied, theirs, "{r:?}");
    assert_eq!(r.conflicts, [4, 5, 17].map(name).to_vec(), "{r:?}");
    for root in [dir_b.path(), dir_c.path()] {
        for i in 0..OWED {
            let want = if [4, 5, 17].contains(&i) { "the agent's own".to_string() } else { format!("the peer's file {i}") };
            assert_eq!(read(root, &name(i)), Some(want), "{}", name(i));
        }
    }
    let skipped = b.state.load_baseline().unwrap().skipped;
    assert_eq!(skipped.iter().cloned().collect::<Vec<_>>(), [4, 5, 17].map(name).to_vec());
}

/// A cited object that is gone when the consume asks for it (a newer
/// document replaced the entry and its collector took the handle) is not
/// an error and not a hole: nothing is written, and the path stays owed,
/// so the next consume derives again even if the pointer has not moved.
#[tokio::test]
async fn a_consume_that_finds_a_cited_object_gone_leaves_the_path_owed() {
    use flint_store::ObjectStore;
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a_peers_bulk_arrival(&store, &mut a, &mut b).await;
    store.delete(&cited_key(&store, &a.cfg, &name(7)).await).await.unwrap();

    let (taken, _) = b.consume_owed().await.unwrap();
    assert_eq!(taken, OWED - 1, "the rest of what was owed did not arrive");
    assert_eq!(read(dir_b.path(), &name(7)), None);
    assert!(derive_pending(&b).await, "a path whose object was gone is no longer owed");
    assert!(
        !b.state.load_baseline().unwrap().entries.contains_key(&name(7)),
        "the baseline cites a version the tree never held"
    );
}

/// 3 MiB of non-uniform bytes: a run of one value would hide a range
/// written at the wrong offset.
fn large(seed: usize) -> Vec<u8> {
    (0..3 * 1024 * 1024 + seed).map(|i| ((i + seed) % 251) as u8).collect()
}

/// A writer A on the plain store and a second tree B behind the hooked
/// one, B set to range anything of 1 MiB or more in 512 KiB ranges. B is
/// checked out before A publishes `weights.bin`, so B owes it.
async fn a_large_object_owed(
    inner: &Arc<MemoryStore>,
    hooked: &Arc<Hooked>,
    dir_a: &std::path::Path,
    dir_b: &std::path::Path,
    body: &[u8],
) -> (Syncer, Syncer) {
    let mut a = syncer(inner, dir_a).await;
    let mut b = hooked_syncer(hooked, dir_b);
    b.cfg.range_get_min_bytes = 1024 * 1024;
    b.cfg.range_get_chunk_bytes = 512 * 1024;
    b.cfg.range_get_parallelism = 4;
    a.checkout().await.unwrap();
    b.checkout().await.unwrap();
    std::fs::write(dir_a.join("weights.bin"), body).unwrap();
    a.run_barrier().await.unwrap();
    assert!(
        manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries.contains_key("weights.bin"),
        "fixture: the peer's barrier did not publish the object"
    );
    inner.reset_op_counts();
    inner.reset_peak_get_range_in_flight();
    (a, b)
}

fn ranges_fetched(inner: &MemoryStore) -> u64 {
    inner.op_counts().get("get_range").copied().unwrap_or(0)
}

fn temp_left(root: &std::path::Path) -> bool {
    root.join("weights.bin.flint-sync-tmp").exists()
}

/// A large object the tree is owed arrives in ranges, written at their
/// offsets, and is never fetched whole: a whole GET holds the object in
/// memory, and the consume made one for every size.
#[tokio::test]
async fn a_consume_takes_a_large_object_in_ranges_never_whole() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Default::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let body = large(0);
    let (_a, mut b) = a_large_object_owed(&inner, &hooked, dir_a.path(), dir_b.path(), &body).await;

    let whole = Arc::new(AtomicBool::new(false));
    let saw = whole.clone();
    hooked.before_get_return("weights.bin", move || saw.store(true, Ordering::SeqCst));
    inner.inject_get_range_delay_ms(5);
    let (taken, _) = b.consume_owed().await.unwrap();
    inner.inject_get_range_delay_ms(0);

    assert_eq!(taken, 1);
    assert_eq!(std::fs::read(dir_b.path().join("weights.bin")).unwrap(), body, "the ranges did not assemble the object");
    assert!(!whole.load(Ordering::SeqCst), "the object was fetched whole");
    let peak = inner.peak_get_range_in_flight();
    assert!(peak > 1, "ranges must overlap; peak in flight was {peak}");
    assert!(!temp_left(dir_b.path()));
    // What the baseline records is the file as written: the next barrier
    // has nothing to publish.
    let r = b.run_barrier().await.unwrap();
    assert!(r.uploaded.is_empty(), "the consumed object read as the agent's change: {:?}", r.uploaded);
}

/// The same through the sync.
#[tokio::test]
async fn a_sync_takes_a_large_object_in_ranges_never_whole() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Default::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let body = large(1);
    let (_a, mut b) = a_large_object_owed(&inner, &hooked, dir_a.path(), dir_b.path(), &body).await;

    let whole = Arc::new(AtomicBool::new(false));
    let saw = whole.clone();
    hooked.before_get_return("weights.bin", move || saw.store(true, Ordering::SeqCst));
    let r = b.sync().await.unwrap();

    assert_eq!(r.applied, vec!["weights.bin".to_string()], "{r:?}");
    assert_eq!(std::fs::read(dir_b.path().join("weights.bin")).unwrap(), body);
    assert!(!whole.load(Ordering::SeqCst), "the object was fetched whole");
    assert!(ranges_fetched(&inner) > 1, "no ranges were fetched");
}

fn agent_writes_at(stage: &'static str, root: &std::path::Path, bytes: &'static str) {
    let target = root.join("weights.bin");
    super::barrier::CONSUME_WINDOW_HOOK.with(|h| {
        *h.borrow_mut() =
            Some(("weights.bin".to_string(), stage, Box::new(move || std::fs::write(&target, bytes).unwrap())))
    });
}

fn window_ran() -> bool {
    super::barrier::CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_none())
}

/// H7 on the ranged arm. A large object is fetched for seconds, and the
/// agent may write its path at any moment of them: the licence to
/// overwrite is asked once more with every range in the temp, immediately
/// before the rename. The agent's version stays, and is what the tree
/// publishes.
#[tokio::test]
async fn a_consume_never_renames_a_ranged_object_over_an_agent_write() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Default::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, mut b) = a_large_object_owed(&inner, &hooked, dir_a.path(), dir_b.path(), &large(2)).await;

    agent_writes_at("before-write", dir_b.path(), "the agent's write, while the object was fetched");
    b.consume_owed().await.unwrap();
    assert!(window_ran(), "fixture: the consume never reached its write window");
    assert!(ranges_fetched(&inner) > 1, "fixture: the object did not take the ranged arm");
    assert_eq!(
        read(dir_b.path(), "weights.bin").as_deref(),
        Some("the agent's write, while the object was fetched"),
        "the consume renamed a ranged object over an agent write it never saw"
    );
    assert!(!temp_left(dir_b.path()), "the temp of the fetch that stood down was left in the tree");
    assert!(
        b.state.load_baseline().unwrap().skipped.contains("weights.bin"),
        "the path the agent took is not recorded as the agent's"
    );
    // The agent's version publishes.
    for _ in 0..2 {
        b.run_barrier().await.unwrap();
    }
    let doc = manifest::load(inner.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(doc.entries["weights.bin"].size, "the agent's write, while the object was fetched".len() as u64);
}

/// The sync's H7 on the ranged arm: the agent's version wins, with the
/// record a dirty path has always had.
#[tokio::test]
async fn a_sync_never_renames_a_ranged_object_over_an_agent_write() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Default::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (_a, mut b) = a_large_object_owed(&inner, &hooked, dir_a.path(), dir_b.path(), &large(3)).await;

    agent_writes_at("before-write", dir_b.path(), "the agent's write, during the sync");
    let r = b.sync().await.unwrap();
    assert!(window_ran(), "fixture: the sync never reached its write window");
    assert!(ranges_fetched(&inner) > 1, "fixture: the object did not take the ranged arm");
    assert_eq!(read(dir_b.path(), "weights.bin").as_deref(), Some("the agent's write, during the sync"));
    assert!(r.applied.is_empty(), "{r:?}");
    assert_eq!(r.conflicts, vec!["weights.bin".to_string()], "{r:?}");
    assert!(
        b.state.load_conflicts().unwrap().iter().any(|c| c.path == "weights.bin" && c.kind == "sync-dirty"),
        "the agent's version won without a record"
    );
    assert!(!temp_left(dir_b.path()));
}

/// A large object whose ranges do not fold to the CRC the document cites
/// is never renamed into place. A checkout fails on it; a consume records
/// it and leaves the path owed, as it does for an object fetched whole,
/// and the rest of what is owed still arrives.
#[tokio::test]
async fn a_ranged_object_that_is_not_the_cited_bytes_is_refused_and_stays_owed() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Default::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut a, mut b) = a_large_object_owed(&inner, &hooked, dir_a.path(), dir_b.path(), &large(4)).await;
    write(dir_a.path(), "beside.txt", "a sound file beside it");
    a.run_barrier().await.unwrap();
    inner.inject_corrupt_body(&cited_key(&inner, &a.cfg, "weights.bin").await, |bytes| bytes[700_000] ^= 0x01);
    inner.reset_op_counts();

    let (taken, _) = b.consume_owed().await.unwrap();
    assert!(ranges_fetched(&inner) > 1, "fixture: the object did not take the ranged arm");
    assert_eq!(taken, 1);
    assert_eq!(read(dir_b.path(), "beside.txt").as_deref(), Some("a sound file beside it"));
    assert!(!dir_b.path().join("weights.bin").exists(), "corrupt bytes were renamed into place");
    assert!(!temp_left(dir_b.path()), "the corrupt temp was left in the tree");
    let refusals = |b: &Syncer| {
        b.state
            .load_conflicts()
            .unwrap()
            .iter()
            .filter(|c| c.path == "weights.bin" && c.kind.starts_with("consume-refused-checksum"))
            .count()
    };
    assert_eq!(refusals(&b), 1, "the refusal was silent");
    assert!(derive_pending(&b).await, "a refused object must stay owed");
    b.consume_owed().await.unwrap();
    assert_eq!(refusals(&b), 2, "the refusal did not repeat");
}

/// A store that fails is not asked for everything else first. The consume
/// stopped at its first failed GET when it fetched one object at a time;
/// with the fetches in flight together, none is started after one has
/// failed, and the failure is the consume's answer. What it is owed
/// arrives once the store answers again.
#[tokio::test]
async fn a_consume_starts_no_fetch_after_one_has_failed() {
    let inner = Arc::new(MemoryStore::new());
    let hooked = Arc::new(Hooked(inner.clone(), Default::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&inner, dir_a.path()).await;
    let mut b = hooked_syncer(&hooked, dir_b.path());
    a_peers_bulk_arrival(&inner, &mut a, &mut b).await;

    hooked.fail_gets_containing(Some("/files/in/"));
    let err = b.consume_owed().await.expect_err("a consume whose store failed reported success");
    assert!(err.to_string().contains("injected: get refused"), "{err}");
    let asked = hooked.gets_failed() as usize;
    assert!(asked >= 1, "fixture: the store was never asked");
    assert!(
        asked <= b.cfg.fetch_drivers.max(1),
        "{asked} of {OWED} fetches were sent to a store that had already failed"
    );
    assert_eq!(read(dir_b.path(), &name(0)), None);

    hooked.fail_gets_containing(None);
    let (taken, _) = b.consume_owed().await.unwrap();
    assert_eq!(taken, OWED);
    assert_eq!(read(dir_b.path(), &name(OWED - 1)), Some(format!("the peer's file {}", OWED - 1)));
}
