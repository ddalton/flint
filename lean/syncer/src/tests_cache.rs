//! The latency plan's levers A and C (`docs/plans/flint-lean-latency-plan-2026-10-23.md`):
//! the manifest chunk cache and the parallel chunk fetch (`chunkcache`,
//! `manifest::load_from`), and the collection pass's cadence
//! (`LeanConfig::collect_every_secs`, barrier step 6).
//!
//! Requests are counted at the store, never inferred, and overlap is
//! OBSERVED (the store's peak of requests in flight under an injected
//! delay), never timed. Each claim has an arm that would pass vacuously
//! without its control: an uncached load, a fetch width of one, a pass
//! that is due.

use std::collections::HashSet;
use std::sync::Arc;

use flint_store::memory::MemoryStore;
use flint_store::{ObjectStore, RequestCounts};

use super::chunkcache::{ChunkCache, DiskChunkCache};
use super::tests::{cfg_for, manifest_of, syncer, write, entry_at};
use super::*;

/// A config whose manifest splits into many small chunks: 2,000 entries
/// make about thirty.
fn small_chunks(root: &std::path::Path) -> LeanConfig {
    let mut c = cfg_for(root);
    c.chunk_target = 64;
    c.chunk_min = 16;
    c.chunk_max = 256;
    // The state dir, as a syncer's `SyncerState::open` makes it: the cache
    // lives inside it and never recreates it (a state dir that vanished is
    // not something to make again quietly, `safefs`).
    std::fs::create_dir_all(c.state_dir()).unwrap();
    c
}

fn stamps(seq: u64) -> manifest::PublishStamps<'static> {
    manifest::PublishStamps { epoch: 1, flush_uuid: if seq == 1 { "u1" } else { "u2" }, boundary_source: None }
}

fn chunk_refs(p: &manifest::Pointer) -> Vec<chunk::ChunkRef> {
    match p.entries().unwrap() {
        manifest::Entries::Chunked(c) => c.to_vec(),
        _ => panic!("not chunked"),
    }
}

fn ops(store: &MemoryStore) -> RequestCounts {
    store.request_counts().unwrap()
}

fn delta(a: RequestCounts, b: RequestCounts) -> RequestCounts {
    RequestCounts {
        get: b.get - a.get,
        head: b.head - a.head,
        put: b.put - a.put,
        copy: b.copy - a.copy,
        delete: b.delete - a.delete,
        list: b.list - a.list,
        multipart: b.multipart - a.multipart,
    }
}

/// Publish `m` with `cfg` and return the chunk list it installed.
async fn publish(store: &Arc<dyn ObjectStore>, cfg: &LeanConfig, m: &manifest::LeanManifest) -> Vec<chunk::ChunkRef> {
    let before = manifest::load(store.as_ref(), cfg).await.unwrap();
    let h = before.as_ref().map(|l| l.handle());
    let prev = h.as_ref().map(|h| h.prev_chunks.clone()).unwrap_or_default();
    manifest::cas_write_chunked(store.as_ref(), cfg, m, h.as_ref(), &prev, stamps(m.seq)).await.unwrap();
    chunk_refs(&manifest::load_pointer(store.as_ref(), cfg).await.unwrap().unwrap().pointer)
}

// ── lever A: the chunk cache ────────────────────────────────────────

/// The headline: a document this reader already holds costs the pointer
/// and one LIST, at any size. The control is the same load uncached: it
/// fetches every chunk, which is what proves the fixture has many.
#[tokio::test]
async fn a_load_of_a_document_already_held_fetches_no_chunk() {
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = inner.clone();
    let dir = tempfile::tempdir().unwrap();
    let cfg = small_chunks(dir.path());
    let m = manifest_of(2000, 1);
    let refs = publish(&store, &cfg, &m).await;
    assert!(refs.len() >= 20, "the fixture made {} chunks; the claim needs many", refs.len());

    let mut uncached = cfg.clone();
    uncached.chunk_cache = None;
    let a = ops(&inner);
    let cold = manifest::load(store.as_ref(), &uncached).await.unwrap().unwrap();
    let cold_ops = delta(a, ops(&inner));
    assert_eq!(cold_ops.get, 1 + refs.len() as u64, "the control fetches the pointer and every chunk");
    assert_eq!(cold_ops.list, 0, "an uncached load lists nothing");

    // The writer kept what it wrote, so this load is all hits.
    let a = ops(&inner);
    let warm = manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap();
    let warm_ops = delta(a, ops(&inner));
    assert_eq!(warm_ops.get, 1, "a cached load fetched chunks: {warm_ops:?}");
    assert_eq!(warm_ops.list, 1, "a cached load must still prove the chunks exist, once");
    assert_eq!(warm.manifest.entries, m.entries, "the cached document is not the published one");
    assert_eq!(warm.manifest.entries, cold.manifest.entries);
    assert_eq!(warm.etag, cold.etag);
}

/// A reader that took the last document fetches only what a peer's
/// one-entry change re-addressed. Its first load is all misses — and
/// lists nothing, because nothing came from its cache.
#[tokio::test]
async fn a_peers_one_entry_change_costs_the_chunks_it_changed() {
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = inner.clone();
    let (wd, rd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let writer = small_chunks(wd.path());
    let reader = small_chunks(rd.path());
    let m1 = manifest_of(2000, 1);
    let refs1 = publish(&store, &writer, &m1).await;

    let a = ops(&inner);
    manifest::load(store.as_ref(), &reader).await.unwrap().unwrap();
    let first = delta(a, ops(&inner));
    assert_eq!(first.get, 1 + refs1.len() as u64, "an empty cache fetches every chunk");
    assert_eq!(first.list, 0, "nothing came from the cache, so there is nothing to prove");

    let mut m2 = m1.clone();
    m2.seq = 2;
    m2.entries.insert("src/f01000.txt".into(), entry_at("changed", 2));
    let refs2 = publish(&store, &writer, &m2).await;
    let changed = refs2.iter().filter(|r| !refs1.contains(r)).count() as u64;
    assert!(changed >= 1);

    let a = ops(&inner);
    let got = manifest::load(store.as_ref(), &reader).await.unwrap().unwrap();
    let second = delta(a, ops(&inner));
    assert_eq!(got.manifest.entries, m2.entries);
    assert_eq!(second.get, 1 + changed, "fetched more than the re-addressed chunks: {second:?}");
    assert_eq!(second.list, 1);
}

/// A cached body that does not hash to its address is damage, not data:
/// it is dropped, fetched again, and the cache ends up holding the right
/// bytes.
#[tokio::test]
async fn a_cached_body_that_does_not_hash_to_its_address_is_fetched_again() {
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = inner.clone();
    let dir = tempfile::tempdir().unwrap();
    let cfg = small_chunks(dir.path());
    let m = manifest_of(2000, 1);
    let refs = publish(&store, &cfg, &m).await;
    let cache_dir = cfg.state_dir().join(CHUNK_CACHE_DIR);
    let victim = cache_dir.join(&refs[3].addr);
    assert!(victim.is_file(), "the writer did not keep what it wrote");
    std::fs::write(&victim, b"{\"entries\":{}}").unwrap();

    let a = ops(&inner);
    let got = manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap();
    let d = delta(a, ops(&inner));
    assert_eq!(got.manifest.entries, m.entries, "a damaged cache entry changed the document");
    assert_eq!(d.get, 2, "the pointer and the one damaged chunk: {d:?}");
    let healed = std::fs::read(&victim).unwrap();
    assert_eq!(chunk::chunk_address(&healed), refs[3].addr, "the cache kept the damage");
}

/// The cache directory sits in the app-writable tree. A link at a chunk's
/// name is never followed — even to a file holding exactly the right
/// bytes — and a FIFO there is refused, not waited on. Both are misses.
#[tokio::test]
async fn a_link_or_a_fifo_at_a_cached_name_is_a_miss() {
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = inner.clone();
    let dir = tempfile::tempdir().unwrap();
    let cfg = small_chunks(dir.path());
    let m = manifest_of(2000, 1);
    let refs = publish(&store, &cfg, &m).await;
    let cache_dir = cfg.state_dir().join(CHUNK_CACHE_DIR);

    // A link to a copy of the right bytes, outside the cache.
    let outside = tempfile::tempdir().unwrap();
    let decoy = outside.path().join("decoy");
    std::fs::copy(cache_dir.join(&refs[1].addr), &decoy).unwrap();
    std::fs::remove_file(cache_dir.join(&refs[1].addr)).unwrap();
    std::os::unix::fs::symlink(&decoy, cache_dir.join(&refs[1].addr)).unwrap();
    // A FIFO: an open that waited for a writer would hang the load.
    std::fs::remove_file(cache_dir.join(&refs[2].addr)).unwrap();
    let fifo = std::ffi::CString::new(cache_dir.join(&refs[2].addr).to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let decoy_before = std::fs::read(&decoy).unwrap();

    let a = ops(&inner);
    // On its own thread and runtime: an open that blocks on the FIFO blocks
    // the thread that polls it, so a timeout inside the same runtime would
    // never fire, and the regression would hang the suite instead of failing.
    let (tx, rx) = std::sync::mpsc::channel();
    let (st, c) = (store.clone(), cfg.clone());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _ = tx.send(rt.block_on(async move { manifest::load(st.as_ref(), &c).await.map(|l| l.map(|l| l.manifest)) }));
    });
    let got = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the load waited on a FIFO in the cache")
        .unwrap()
        .unwrap();
    let d = delta(a, ops(&inner));
    assert_eq!(got.entries, m.entries);
    assert_eq!(d.get, 3, "the pointer and the two planted names, fetched: {d:?}");
    assert_eq!(std::fs::read(&decoy).unwrap(), decoy_before, "a write went through the link");
}

/// An address comes from the bucket. Only what `chunk_address` mints may
/// name a file in the cache, so no pointer can make the cache read or
/// write outside its directory.
#[test]
fn no_address_but_a_minted_one_reaches_the_cache_directory() {
    for bad in ["../escape", "a/b", "", ".", "ABCDEF0123456789ABCDEF0123456789", "0123456789abcdef0123456789abcde"] {
        assert!(!chunk::is_chunk_address(bad), "{bad:?} passed as an address");
    }
    assert!(chunk::is_chunk_address(&chunk::chunk_address(b"x")));
    let dir = tempfile::tempdir().unwrap();
    let cache = DiskChunkCache::new(dir.path().join("state").join("chunk-cache"));
    std::fs::create_dir_all(dir.path().join("state")).unwrap();
    cache.put("../escape", b"payload");
    assert!(!dir.path().join("state").join("escape").exists(), "a remote name became a local path");
    assert!(cache.get("../escape").is_none());
}

/// A credential that may read but not list: a cached load falls back to
/// fetching what its cache served, so the existence check is paid the old
/// way, never skipped.
#[tokio::test]
async fn a_load_that_may_not_list_fetches_what_the_cache_served() {
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = inner.clone();
    let dir = tempfile::tempdir().unwrap();
    let cfg = small_chunks(dir.path());
    let m = manifest_of(2000, 1);
    let refs = publish(&store, &cfg, &m).await;
    inner.deny_list_matching(Some("/chunks/"));

    let a = ops(&inner);
    let got = manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap();
    let d = delta(a, ops(&inner));
    assert_eq!(got.manifest.entries, m.entries);
    assert_eq!(d.list, 1, "the LIST was tried");
    assert_eq!(d.get, 1 + refs.len() as u64, "a refused LIST must not skip the existence check");

    // And a chunk the bucket lost still refuses the load on that route.
    store.delete(&cfg.chunk_key(&refs[5].addr)).await.unwrap();
    let msg = match manifest::load(store.as_ref(), &cfg).await {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a cached load with no listing served a manifest whose chunk is gone"),
    };
    assert!(msg.contains("hole"), "wrong refusal: {msg}");
}

/// A chunk the live pointer names that the bucket lost refuses the load
/// with the cache as without it (the battery's `a_missing_chunk_refuses_
/// rather_than_shortening_the_manifest` is the cached arm; this is both).
#[tokio::test]
async fn a_missing_chunk_refuses_cached_or_not() {
    for cached in [true, false] {
        let inner = Arc::new(MemoryStore::new());
        let store: Arc<dyn ObjectStore> = inner.clone();
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = small_chunks(dir.path());
        if !cached {
            cfg.chunk_cache = None;
        }
        let refs = publish(&store, &cfg, &manifest_of(2000, 1)).await;
        if cached {
            assert!(cfg.state_dir().join(CHUNK_CACHE_DIR).join(&refs[7].addr).is_file());
        }
        store.delete(&cfg.chunk_key(&refs[7].addr)).await.unwrap();
        let msg = match manifest::load(store.as_ref(), &cfg).await {
            Err(e) => e.to_string(),
            Ok(m) => panic!(
                "cached={cached}: a manifest with a missing chunk LOADED, {} entries",
                m.map(|l| l.manifest.entries.len()).unwrap_or(0)
            ),
        };
        assert!(msg.contains("hole"), "cached={cached}: wrong refusal: {msg}");
    }
}

/// An uncached load's chunks are fetched in parallel, bounded by
/// `chunk_fetch_parallelism`; at a width of one they are fetched one at a
/// time, as before. Overlap is read off the store, not timed.
#[tokio::test]
async fn an_uncached_load_fetches_its_chunks_in_parallel() {
    for width in [16usize, 1] {
        let inner = Arc::new(MemoryStore::new());
        let store: Arc<dyn ObjectStore> = inner.clone();
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = small_chunks(dir.path());
        cfg.chunk_cache = None;
        cfg.chunk_fetch_parallelism = width;
        let m = manifest_of(2000, 1);
        let refs = publish(&store, &cfg, &m).await;
        assert!(refs.len() > width);
        inner.inject_request_delay_ms(5);
        inner.reset_peak_requests_in_flight();
        let got = manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap();
        inner.inject_request_delay_ms(0);
        assert_eq!(got.manifest.entries, m.entries);
        let peak = inner.peak_requests_in_flight();
        if width == 1 {
            assert_eq!(peak, 1, "a width of one overlapped requests");
        } else {
            assert!(peak > 1 && peak <= width as u64, "peak {peak} in flight at width {width}");
        }
    }
}

/// A barrier's own commit keeps its chunks, so the writer's next load of
/// its own document — the next commit's — fetches none.
#[tokio::test]
async fn a_writers_next_commit_loads_its_own_document_without_a_chunk_get() {
    let inner = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&inner, dir.path()).await;
    a.cfg.chunk_target = 64;
    a.cfg.chunk_min = 16;
    a.cfg.chunk_max = 256;
    a.checkout().await.unwrap();
    for i in 0..2000 {
        write(dir.path(), &format!("d/f{i:04}.txt"), "v1");
    }
    a.run_barrier().await.unwrap();
    let refs = chunk_refs(&manifest::load_pointer(a.store.as_ref(), &a.cfg).await.unwrap().unwrap().pointer);
    assert!(refs.len() >= 5);

    write(dir.path(), "d/f0300.txt", "v2, longer");
    let before = ops(&inner);
    let r = a.run_barrier().await.unwrap();
    let d = delta(before, ops(&inner));
    assert_eq!(r.uploaded, vec!["d/f0300.txt".to_string()]);
    assert!(refs.len() >= 20, "{} chunks: the fixture is too small to tell", refs.len());
    let load = r.steps.iter().find(|s| s.step == "load").expect("a commit loads").requests.unwrap();
    assert_eq!(load.get, 1, "the commit's load fetched chunks it wrote itself: {load:?}");
    assert!(d.get < refs.len() as u64, "the publish's GETs ({}) grew with the chunk count", d.get);
}

// ── lever C: the collection pass's cadence ──────────────────────────

/// The steps' requests, summed (`tests_steps`' rule: every request a
/// barrier sends is charged to exactly one step).
fn charged(r: &barrier::BarrierReport) -> RequestCounts {
    let mut c = RequestCounts::default();
    for s in &r.steps {
        let q = s.requests.expect("the memory double counts requests");
        c.get += q.get;
        c.head += q.head;
        c.put += q.put;
        c.copy += q.copy;
        c.delete += q.delete;
        c.list += q.list;
        c.multipart += q.multipart;
    }
    c
}

const PASS: [&str; 5] = ["retire_ledger", "reap", "orphan_sweep", "generation_sweep", "chunk_sweep"];

/// Inside the interval a publish lists nothing and deletes nothing, and
/// every request it does send is still charged to a step; once the
/// interval has passed, the next publish collects everything due since —
/// the versions the skipped publishes replaced included.
#[tokio::test]
async fn a_publish_inside_the_collection_interval_collects_nothing_and_the_next_pass_collects_it_all() {
    let inner = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&inner, dir.path()).await;
    a.cfg.collect_every_secs = 300;
    a.checkout().await.unwrap();
    write(dir.path(), "a.txt", "v1");
    let first = a.run_barrier().await.unwrap();
    for step in PASS {
        assert!(first.steps.iter().any(|s| s.step == step), "the first commit ran no {step}: its clock starts there");
    }

    let cited = |sc: &Syncer| {
        let store = sc.store.clone();
        let cfg = sc.cfg.clone();
        async move { manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap().manifest.entries["a.txt"].key.clone() }
    };
    let v1 = cited(&a).await;
    write(dir.path(), "a.txt", "v2, longer");
    let before = ops(&inner);
    let skipped = a.run_barrier().await.unwrap();
    let d = delta(before, ops(&inner));
    let v2 = cited(&a).await;
    assert_ne!(v1, v2);
    // Its one LIST is the commit's load proving its cached chunks exist
    // (lever A); the pass's three are gone.
    let load = skipped.steps.iter().find(|s| s.step == "load").expect("a commit loads").requests.unwrap();
    assert_eq!(d.list, load.list, "a publish inside the interval listed beyond its load: {d:?}");
    assert!(d.list <= 1);
    assert_eq!(d.delete, 0, "a publish inside the interval deleted: {d:?}");
    assert_eq!(skipped.collected, 0);
    for step in PASS {
        assert!(!skipped.steps.iter().any(|s| s.step == step), "a skipped pass still ran {step}");
    }
    assert_eq!(charged(&skipped), d, "a request no step was charged for: {:#?}", skipped.steps);
    assert!(inner.head(&v1).await.is_ok(), "the replaced version went while no pass ran");

    // The interval has passed.
    a.state.save_collect_at(now_unix() - 301).unwrap();
    write(dir.path(), "a.txt", "v3, longer still");
    let pass = a.run_barrier().await.unwrap();
    assert!(pass.steps.iter().any(|s| s.step == "reap"));
    assert!(
        matches!(inner.head(&v1).await, Err(flint_store::StoreError::NotFound(_))),
        "the pass left the version a skipped publish replaced"
    );
    assert!(
        matches!(inner.head(&v2).await, Err(flint_store::StoreError::NotFound(_))),
        "the pass left the version its own publish replaced"
    );
    let v3 = cited(&a).await;
    assert!(inner.head(&v3).await.is_ok(), "the pass took the cited version");
    assert!(a.state.load_collect_at() >= now_unix() - 5, "the pass did not restart the clock");
}

/// The logs a pass meets are read in parallel: a pass every few minutes
/// meets every log written since the last one, under the lease. At a
/// width of one they are read one at a time (the control).
#[tokio::test]
async fn a_collection_pass_reads_the_logs_it_has_not_seen_in_parallel() {
    for width in [16usize, 1] {
        let inner = Arc::new(MemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        let mut a = syncer(&inner, dir.path()).await;
        a.cfg.collect_every_secs = 300;
        a.cfg.retire_grace_secs = 600;
        a.cfg.chunk_fetch_parallelism = width;
        a.checkout().await.unwrap();
        write(dir.path(), "a.txt", "v0");
        a.run_barrier().await.unwrap();
        for k in 1..=8 {
            write(dir.path(), "a.txt", &format!("v{k} {}", "x".repeat(k)));
            a.run_barrier().await.unwrap();
        }
        let logs = inner.list(&manifest::retired_prefix(&a.cfg)).await.unwrap().len();
        assert!(logs >= 8, "{logs} retire logs; the skipped publishes should each have written one");

        a.state.save_collect_at(now_unix() - 301).unwrap();
        write(dir.path(), "a.txt", "the pass");
        inner.inject_request_delay_ms(5);
        inner.reset_peak_requests_in_flight();
        let r = a.run_barrier().await.unwrap();
        inner.inject_request_delay_ms(0);
        let ledger = r.steps.iter().find(|s| s.step == "retire_ledger").expect("the pass ran").requests.unwrap();
        assert!(ledger.get >= 8, "the ledger read {} logs", ledger.get);
        let peak = inner.peak_requests_in_flight();
        if width == 1 {
            assert_eq!(peak, 1, "a width of one overlapped requests");
        } else {
            assert!(peak > 1, "the ledger read its logs one at a time at width {width}");
        }
    }
}

/// The cache keeps one document's chunks: a load drops what the document
/// it loaded no longer names.
#[tokio::test]
async fn the_cache_keeps_only_the_chunks_of_the_document_loaded_last() {
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = inner.clone();
    let dir = tempfile::tempdir().unwrap();
    let cfg = small_chunks(dir.path());
    let m1 = manifest_of(2000, 1);
    publish(&store, &cfg, &m1).await;
    let mut m2 = manifest_of(1000, 2);
    m2.entries.insert("zzz/new.txt".into(), entry_at("new", 2));
    let refs2 = publish(&store, &cfg, &m2).await;
    manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap();
    let held: HashSet<String> = std::fs::read_dir(cfg.state_dir().join(CHUNK_CACHE_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let named: HashSet<String> = refs2.iter().map(|r| r.addr.clone()).collect();
    assert_eq!(held, named, "the cache holds chunks the live document does not name");
}

/// The bucket hands back another chunk's bytes for a chunk's key: the load
/// refuses with the address error (the loader's own check, now that
/// `assemble_verified` does not repeat it), and the cache keeps nothing
/// under the address the bytes are not.
#[tokio::test]
async fn a_fetched_body_that_is_not_the_addressed_chunk_refuses_and_is_not_kept() {
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = inner.clone();
    let (wd, rd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let writer = small_chunks(wd.path());
    let reader = small_chunks(rd.path());
    let refs = publish(&store, &writer, &manifest_of(2000, 1)).await;
    let (_, other) = store.get_whole(&writer.chunk_key(&refs[4].addr), None).await.unwrap();
    let crc = flint_store::crc64_nvme(&other);
    let st = flint_store::GenerationStamps {
        generation: 1,
        epoch: 1,
        flush_uuid: "swap".into(),
        boundary_source: None,
        posix: None,
    };
    store
        .put_whole(&writer.chunk_key(&refs[3].addr), other, &flint_store::PutCondition::Unconditional, &st, crc)
        .await
        .unwrap();

    let msg = match manifest::load(store.as_ref(), &reader).await {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a load accepted another chunk's bytes"),
    };
    assert!(msg.contains("does not match its address"), "wrong refusal: {msg}");
    let kept = reader.state_dir().join(CHUNK_CACHE_DIR).join(&refs[3].addr);
    assert!(
        std::fs::read(&kept).map(|b| chunk::chunk_address(&b) == refs[3].addr).unwrap_or(true),
        "the cache kept bytes under an address they are not"
    );
}
