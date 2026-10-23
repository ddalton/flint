//! A barrier's cost against the size of the workspace: not a test, a
//! measurement (`#[ignore]`). One writer seeds N files, a second checks
//! out, and the second's barriers are timed idle, then while a peer's
//! one-file change reaches it, then publishing its own one-file changes.
//!
//! Every timed barrier also prints one `STEPS {json}` line: where its
//! time went, step by step, with the requests each step sent
//! (`BarrierReport::steps`).
//!
//! The store is in memory by default, so the times are CPU and local
//! disk and the request counts are what S3 would add latency to:
//!
//! FLINT_BENCH_N=20000 cargo test -p flint-lean --release --lib \
//!     tests_bench -- --ignored --nocapture
//!
//! With `--features s3` and `FLINT_BENCH_S3_ENDPOINT` set it runs
//! against a real S3-compatible store instead (`FLINT_BENCH_S3_BUCKET`,
//! `FLINT_BENCH_S3_KEY`, `FLINT_BENCH_S3_SECRET`; region us-east-1),
//! under a fresh prefix per run. Every config is the shipped default.
//!
//! Latency plan 2026-10-23 knobs (in-memory store only):
//!
//! - `FLINT_BENCH_DELAY_MS`: every request pays this round trip once B
//!   has checked out (the seed and the checkout run at memory speed), so
//!   a chain of sequential requests is measured against one that overlaps.
//!   25 ms is about what the 2026-09-14 contention drill saw on real S3.
//! - `FLINT_BENCH_ARM=control`: the shape before levers A and C — no chunk
//!   cache, chunk fetches one at a time, the collection pass at every
//!   commit — from the same binary. The default arm is the shipped config.
//! - `FLINT_BENCH_SCAN_THREADS`: the threads the scan walks with (lever E;
//!   1 = one worker). Unset, the shipped default.

use super::*;
use flint_store::memory::MemoryStore;
use flint_store::{ObjectStore, RequestCounts};
use std::sync::Arc;
use std::time::Instant;

fn put(root: &std::path::Path, i: usize, body: &str) {
    let p = root.join(format!("d{:04}/f{i:07}.txt", i / 1000));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn got(root: &std::path::Path, i: usize) -> Option<String> {
    std::fs::read_to_string(root.join(format!("d{:04}/f{i:07}.txt", i / 1000))).ok()
}

/// The store under measurement, what it is called in the output, and the
/// in-memory double behind it when there is one (to inject a delay).
fn bench_store() -> (Arc<dyn ObjectStore>, &'static str, Option<Arc<MemoryStore>>) {
    #[cfg(feature = "s3")]
    if let Ok(endpoint) = std::env::var("FLINT_BENCH_S3_ENDPOINT") {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set with FLINT_BENCH_S3_ENDPOINT"));
        let s = flint_store::s3::S3Store::with_credentials(
            env("FLINT_BENCH_S3_BUCKET"),
            endpoint,
            "us-east-1",
            env("FLINT_BENCH_S3_KEY"),
            env("FLINT_BENCH_S3_SECRET"),
        )
        .expect("an S3 store from the FLINT_BENCH_S3_* settings");
        return (Arc::new(s), "s3", None);
    }
    let m = Arc::new(MemoryStore::new());
    (m.clone(), "memory", Some(m))
}

/// The arm under measurement (`FLINT_BENCH_ARM`).
fn bench_arm() -> &'static str {
    match std::env::var("FLINT_BENCH_ARM").as_deref() {
        Ok("control") => "control",
        // Lever G's control: everything shipped but the chunk view.
        Ok("noview") => "noview",
        // The re-hash's (latency plan §17): the shape before, and the gate
        // without its wait.
        Ok("nogate") => "nogate",
        Ok("nowait") => "nowait",
        Ok("shipped") | Err(_) => "shipped",
        // An arm this list lacks would run as shipped and time as a
        // control that is not one.
        Ok(other) => panic!("FLINT_BENCH_ARM={other}: no such arm"),
    }
}

/// A syncer with the shipped configuration, under `prefix`.
fn bench_syncer(store: &Arc<dyn ObjectStore>, root: &std::path::Path, prefix: &str) -> Syncer {
    let mut cfg = LeanConfig::new(prefix, root);
    if bench_arm() == "control" {
        cfg.chunk_cache = None;
        cfg.chunk_fetch_parallelism = 1;
        cfg.collect_every_secs = 0;
    }
    if bench_arm() == "noview" {
        cfg.chunk_view = false;
    }
    if bench_arm() == "nogate" {
        cfg.racy_at_gate = false;
    }
    if bench_arm() == "nowait" {
        cfg.racy_wait_min = 0;
    }
    // Lever E's arms from one binary: the walk's thread count.
    if let Some(n) = std::env::var("FLINT_BENCH_SCAN_THREADS").ok().and_then(|v| v.parse().ok()) {
        cfg.scan_threads = n;
    }
    let state = super::state::SyncerState::open(cfg.state_dir()).unwrap();
    Syncer {
        store: store.clone(),
        cfg,
        state,
        lease: None,
        cell_written_at: None,
        noted_not_regular: Default::default(),
    }
}

/// Requests since the last call, by kind.
struct Ops {
    store: Arc<dyn ObjectStore>,
    last: RequestCounts,
}

impl Ops {
    fn new(store: &Arc<dyn ObjectStore>) -> Self {
        Self { store: store.clone(), last: store.request_counts().unwrap_or_default() }
    }

    fn take(&mut self) -> String {
        let now = self.store.request_counts().unwrap_or_default();
        let d = |a: u64, b: u64| b.saturating_sub(a);
        let s = [
            ("get", d(self.last.get, now.get)),
            ("head", d(self.last.head, now.head)),
            ("put", d(self.last.put, now.put)),
            ("copy", d(self.last.copy, now.copy)),
            ("delete", d(self.last.delete, now.delete)),
            ("list", d(self.last.list, now.list)),
            ("multipart", d(self.last.multipart, now.multipart)),
        ]
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(k, n)| format!("{k}={n}"))
        .collect::<Vec<_>>()
        .join(" ");
        self.last = now;
        s
    }
}

/// One barrier, timed: its `BENCH` line and its `STEPS` line.
async fn timed(sc: &mut Syncer, ops: &mut Ops, n: usize, store: &str, label: &str) -> barrier::BarrierReport {
    let t = Instant::now();
    let r = sc.run_barrier().await.unwrap();
    let ms = t.elapsed().as_millis() as u64;
    println!("BENCH n={n} {label}_ms={ms} ops: {}", ops.take());
    let line = serde_json::json!({
        "store": store, "n": n, "barrier": label, "ms": ms,
        "uploaded": r.uploaded.len(), "no_change": r.no_change, "steps": r.steps,
    });
    println!("STEPS {line}");
    r
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn barrier_cost_against_workspace_size() {
    let n: usize = std::env::var("FLINT_BENCH_N").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000);
    let (store, kind, mem) = bench_store();
    let prefix = format!("bench/{}", uuid::Uuid::new_v4());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = bench_syncer(&store, dir_a.path(), &prefix);
    let mut b = bench_syncer(&store, dir_b.path(), &prefix);
    let mut ops = Ops::new(&store);
    a.checkout().await.unwrap();
    for i in 0..n {
        put(dir_a.path(), i, &format!("v1 of {i}"));
    }
    let t = Instant::now();
    let mut seeded = 0;
    for k in 0..6 {
        seeded += timed(&mut a, &mut ops, n, kind, &format!("seed{k}")).await.uploaded.len();
        if seeded >= n {
            break;
        }
    }
    assert_eq!(seeded, n, "the seed never published");
    println!("BENCH n={n} seed_publish_ms={}", t.elapsed().as_millis());
    // The seed tree shares this filesystem with B's, and checkout ends in
    // syncfs(2): without this flush B's checkout also pays for writing A's
    // N files back, as much as the seed's duration left unflushed (a 3 s
    // seed left all of them; a 79 s seed, none). A real peer is on another
    // machine.
    let t = Instant::now();
    unsafe { libc::sync() };
    println!("BENCH n={n} seed_flush_ms={}", t.elapsed().as_millis());
    ops.take();

    let t = Instant::now();
    b.checkout().await.unwrap();
    println!("BENCH n={n} checkout_ms={} ops: {}", t.elapsed().as_millis(), ops.take());
    // From here on every request pays the injected round trip, if any.
    let delay: u64 = std::env::var("FLINT_BENCH_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    if let Some(m) = mem.as_ref().filter(|_| delay > 0) {
        m.inject_request_delay_ms(delay);
    }
    println!("BENCH n={n} arm={} delay_ms={delay}", bench_arm());
    // How many entries the first scan must hash (latency plan §17).
    let racy = |b: &Syncer| b.state.load_baseline().map(|x| x.entries.values().filter(|e| e.racy).count()).unwrap_or(0);
    println!("BENCH n={n} racy_after_checkout={}", racy(&b));
    for k in 0..3 {
        timed(&mut b, &mut ops, n, kind, &format!("idle{k}")).await;
        if k == 0 {
            println!("BENCH n={n} racy_after_idle0={}", racy(&b));
            // FLINT_BENCH_STOP=idle0: the post-checkout re-hash alone.
            if std::env::var("FLINT_BENCH_STOP").as_deref() == Ok("idle0") {
                return;
            }
        }
    }

    // A peer's one-file change: its publish, then B's barriers until the
    // bytes are in B's tree.
    let hot = n / 2;
    put(dir_a.path(), hot, "v2 from the peer");
    timed(&mut a, &mut ops, n, kind, "peer_publish_one").await;
    let t = Instant::now();
    let mut barriers = 0;
    while got(dir_b.path(), hot).as_deref() != Some("v2 from the peer") {
        assert!(barriers < 6, "the peer's change never arrived");
        barriers += 1;
        timed(&mut b, &mut ops, n, kind, &format!("arrive_barrier{barriers}")).await;
    }
    println!("BENCH n={n} arrival_total_ms={} barriers={barriers}", t.elapsed().as_millis());
    for k in 0..2 {
        timed(&mut b, &mut ops, n, kind, &format!("idle_after{k}")).await;
    }

    // B's own one-file publish at this size.
    put(dir_b.path(), 1, "v2 from B");
    timed(&mut b, &mut ops, n, kind, "own_publish_one").await;

    // A BUSY agent: it publishes every tick, nobody else commits. Each
    // publish's consume, and the idle tick after, show what the cheap path
    // costs a writer whose own commits keep moving the pointer.
    for k in 0..3 {
        put(dir_b.path(), 2, &format!("busy edit {k} {}", "x".repeat(k + 1)));
        timed(&mut b, &mut ops, n, kind, &format!("busy{k}")).await;
    }
    timed(&mut b, &mut ops, n, kind, "idle_after_busy").await;

    // The same one-file publish with the orphan sweep due: what it adds
    // to the commit section, once per `untracked_sweep_secs` (an hour by
    // default) on a workspace this size.
    b.cfg.untracked_sweep_secs = 1;
    // The orphan sweep runs inside the collection pass, so the pass must
    // be due too (latency plan lever C).
    b.cfg.collect_every_secs = b.cfg.collect_every_secs.min(1);
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    put(dir_b.path(), 3, "v2 from B, sweep due");
    let r = timed(&mut b, &mut ops, n, kind, "own_publish_one_sweep_due").await;
    println!("BENCH n={n} swept={}", r.swept);
    // Lever D's two costs: the parses it saves, and the memory it keeps.
    // Peak RSS covers BOTH writers and, on the in-memory store, the store.
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let kib = |key: &str| {
        status.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)).unwrap_or("?").to_string()
    };
    println!(
        "BENCH n={n} baseline_parses_b={} vm_hwm_kib={} vm_rss_kib={}",
        b.state.baseline_parses(),
        kib("VmHWM:"),
        kib("VmRSS:")
    );
}
