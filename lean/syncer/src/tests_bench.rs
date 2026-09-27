//! A barrier's cost against the size of the workspace: not a test, a
//! measurement (`#[ignore]`). One writer seeds N files, a second checks
//! out, and the second's barriers are timed idle, then while a peer's
//! one-file change reaches it. The store is in memory, so the times are
//! CPU and local disk; the request counts are what S3 would add latency
//! to.
//!
//! FLINT_BENCH_N=20000 cargo test -p flint-lean-syncer --release --lib \
//!     tests_bench -- --ignored --nocapture

use super::tests::syncer;
use super::*;
use flint_store::memory::MemoryStore;
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

fn ops(store: &MemoryStore) -> String {
    let c = store.op_counts();
    let s = c.iter().filter(|(_, n)| **n > 0).map(|(k, n)| format!("{k}={n}")).collect::<Vec<_>>().join(" ");
    store.reset_op_counts();
    s
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn barrier_cost_against_workspace_size() {
    let n: usize = std::env::var("FLINT_BENCH_N").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000);
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    a.cfg.retire_grace_secs = manifest::RETIRE_GRACE_SECS; // NEW-ONLY
    b.cfg.retire_grace_secs = manifest::RETIRE_GRACE_SECS; // NEW-ONLY
    a.checkout().await.unwrap();
    for i in 0..n {
        put(dir_a.path(), i, &format!("v1 of {i}"));
    }
    let t = Instant::now();
    let mut seeded = 0;
    for _ in 0..6 {
        seeded += a.run_barrier().await.unwrap().uploaded.len();
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
    store.reset_op_counts();

    let t = Instant::now();
    b.checkout().await.unwrap();
    println!("BENCH n={n} checkout_ms={} ops: {}", t.elapsed().as_millis(), ops(&store));
    for k in 0..3 {
        let t = Instant::now();
        b.run_barrier().await.unwrap();
        println!("BENCH n={n} idle{k}_ms={} ops: {}", t.elapsed().as_millis(), ops(&store));
    }

    // A peer's one-file change: its publish, then B's barriers until the
    // bytes are in B's tree.
    let hot = n / 2;
    put(dir_a.path(), hot, "v2 from the peer");
    let t = Instant::now();
    a.run_barrier().await.unwrap();
    println!("BENCH n={n} peer_publish_one_ms={} ops: {}", t.elapsed().as_millis(), ops(&store));
    let t = Instant::now();
    let mut barriers = 0;
    while got(dir_b.path(), hot).as_deref() != Some("v2 from the peer") {
        assert!(barriers < 6, "the peer's change never arrived");
        let s = Instant::now();
        b.run_barrier().await.unwrap();
        barriers += 1;
        println!("BENCH n={n} arrive_barrier{barriers}_ms={} ops: {}", s.elapsed().as_millis(), ops(&store));
    }
    println!("BENCH n={n} arrival_total_ms={} barriers={barriers}", t.elapsed().as_millis());
    for k in 0..2 {
        let t = Instant::now();
        b.run_barrier().await.unwrap();
        println!("BENCH n={n} idle_after{k}_ms={} ops: {}", t.elapsed().as_millis(), ops(&store));
    }

    // B's own one-file publish at this size.
    put(dir_b.path(), 1, "v2 from B");
    let t = Instant::now();
    b.run_barrier().await.unwrap();
    println!("BENCH n={n} own_publish_one_ms={} ops: {}", t.elapsed().as_millis(), ops(&store));

    // A BUSY agent: it publishes every tick, nobody else commits. Each
    // publish's consume, and the idle tick after, show what the cheap path
    // costs a writer whose own commits keep moving the pointer.
    for k in 0..3 {
        put(dir_b.path(), 2, &format!("busy edit {k} {}", "x".repeat(k + 1)));
        let t = Instant::now();
        b.run_barrier().await.unwrap();
        println!("BENCH n={n} busy{k}_ms={} ops: {}", t.elapsed().as_millis(), ops(&store));
    }
    let t = Instant::now();
    b.run_barrier().await.unwrap();
    println!("BENCH n={n} idle_after_busy_ms={} ops: {}", t.elapsed().as_millis(), ops(&store));
}
