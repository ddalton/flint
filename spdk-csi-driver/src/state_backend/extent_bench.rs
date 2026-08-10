//! §8's cost-budget gate: the mdsbench A/B for the block-layout
//! allocator (docs/plans/pnfs-block-layout-design.md §8 "Cost budget").
//!
//! The Tier-1 mdsbench table (docs/plans/mds-performance-plan.md) was
//! measured through the wire with a kernel client — which cannot speak
//! layout type 5 until the ≥6.11 rig exists, and §8 orders this gate
//! BEFORE that client work. So this harness benches what is genuinely
//! NEW in the scsi path — the sqlite transaction core, through the
//! production `StateBackend` machinery (writer thread, barrier
//! round-trip, real file, WAL+NORMAL) — and compares per-op cost
//! against the Tier-1 per-op anchors. The dispatch/XDR/session cost
//! above it is shared with the files path and already priced by Tier-1.
//!
//! Run it on REAL LINUX (fsync/ext4 behavior is the point; macOS
//! numbers are decorative), with the cross-build recipe:
//!
//! ```sh
//! RUSTFLAGS="-C link-self-contained=no" \
//! CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=$SHIM/zigcc-aarch64 \
//! CC_aarch64_unknown_linux_musl=$SHIM/zigcc-aarch64 \
//! cargo test --no-run --lib --target aarch64-unknown-linux-musl
//! limactl shell flint-nfs-client -- env TMPDIR=/var/tmp \
//!   <test-binary> --ignored --nocapture mdsbench_block
//! ```
//!
//! Workload shapes (each in its own volume so the per-volume
//! `verify_volume_invariants` growth is isolated per bench):
//!
//!   admission   the LAYOUTGET fast-path SELECT (block_hosts, 2 rows)
//!   grant-open  per-open shape: hosts SELECT + one 4Mi fresh grant,
//!               fresh file each op (the w2-opencl analog)
//!   grant-4k    the §8 shatter: 1024 blksize-sized grants on ONE file
//!               (we advertise layout_blksize=4096, so this table shape
//!               is client-reachable) — first/last quartiles exposed as
//!               the slope detector. Historically grant's full-volume
//!               verify was O(rows) (~0.9 µs/row/txn, the cost-gate
//!               debt); the merge tranche windowed it, and the
//!               quartile spread is the evidence it stays flat
//!   commit-1    the worst-case single LAYOUTCOMMIT: one commit over all
//!               1024 shattered rows (1024 validates + 1024 promotes)
//!   commit-4k   steady-state small commits: 1024 individual 4KiB
//!   return+free LAYOUTRETURN of the shattered file + reclaim_complete
//!               (the REMOVE shape at worst-case row count)

use std::sync::Arc;
use std::time::Instant;

use crate::state_backend::{SqliteBackend, StateBackend};

const MIB: u64 = 1024 * 1024;

struct Timings(Vec<f64>); // µs per op

impl Timings {
    fn stats(&self) -> (f64, f64, f64) {
        let mut s = self.0.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean = s.iter().sum::<f64>() / s.len() as f64;
        let p50 = s[s.len() / 2];
        let p95 = s[((s.len() as f64 * 0.95) as usize).min(s.len() - 1)];
        (mean, p50, p95)
    }
    fn quartile_means(&self) -> (f64, f64) {
        let q = self.0.len() / 4;
        let first = self.0[..q].iter().sum::<f64>() / q as f64;
        let last = self.0[self.0.len() - q..].iter().sum::<f64>() / q as f64;
        (first, last)
    }
}

fn row(name: &str, n: usize, t: &Timings, note: &str) {
    let (mean, p50, p95) = t.stats();
    let total_ms = t.0.iter().sum::<f64>() / 1e3;
    println!(
        "{name:<12} {n:>6} ops  {total_ms:>9.1} ms total  {mean:>8.1} µs/op  \
         p50 {p50:>7.1}  p95 {p95:>7.1}   {note}"
    );
}

#[tokio::test]
#[ignore = "perf bench, not a correctness gate — run explicitly per the module doc"]
async fn mdsbench_block_allocator_cost() {
    // Measure the PRODUCTION verify path: the test-build differential
    // belt re-runs the full O(rows) verifier after every windowed check
    // — precisely the slope this bench exists to price — so it must be
    // off here (and only here; this test runs alone via `--ignored`).
    crate::state_backend::extent_alloc::BENCH_SKIP_FULL_VERIFY
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let dir = tempfile::tempdir().unwrap(); // honours TMPDIR (lima: /var/tmp, ext4)
    let backend: Arc<dyn StateBackend> =
        Arc::new(SqliteBackend::open(dir.path().join("state.db")).unwrap());
    let host = "nqn.2024-11.com.flint:node:bench";

    println!();
    println!("mdsbench-block [{}]", std::env::consts::OS);
    println!(
        "state.db on {:?} — production path: writer thread + barrier, WAL+NORMAL",
        dir.path()
    );
    println!();

    // ── admission: the per-LAYOUTGET fast-path SELECT ────────────────
    backend.extent_register_volume("v-adm", 8 << 30).await.unwrap().unwrap();
    backend.block_host_admit("v-adm", 1, host, 0).await.unwrap().unwrap();
    backend.block_host_admit("v-adm", 2, "nqn.2024-11.com.flint:node:b2", 0)
        .await.unwrap().unwrap();
    let mut t = Vec::new();
    for _ in 0..5000 {
        let ti = Instant::now();
        backend.block_hosts("v-adm").await.unwrap().unwrap();
        t.push(ti.elapsed().as_secs_f64() * 1e6);
    }
    let adm = Timings(t);
    row("admission", 5000, &adm, "barrier + 2-row SELECT (≈ writer-path floor)");

    // ── grant-open: fresh file, one 4Mi extent, hosts SELECT first ───
    backend.extent_register_volume("v-open", 8 << 30).await.unwrap().unwrap();
    backend.block_host_admit("v-open", 1, host, 0).await.unwrap().unwrap();
    let mut t = Vec::new();
    for i in 0..1000u64 {
        let ti = Instant::now();
        backend.block_hosts("v-open").await.unwrap().unwrap();
        let g = backend
            .extent_grant("v-open", i, 1, 0, 4 * MIB, true)
            .await.unwrap().unwrap();
        assert_eq!(g.len(), 1);
        t.push(ti.elapsed().as_secs_f64() * 1e6);
    }
    let open = Timings(t);
    let (q1, q4) = open.quartile_means();
    row("grant-open", 1000, &open, "per-open LAYOUTGET shape (SELECT + 4Mi grant txn)");
    println!(
        "{:>14} first-quartile mean {q1:.1} µs vs last {q4:.1} µs  \
         (slope detector: 1 → 1000 extents; the windowed verify must keep this flat)",
        ""
    );

    // ── grant-4k: the §8 shatter, 1024 rows on one file ──────────────
    backend.extent_register_volume("v-shatter", 1 << 30).await.unwrap().unwrap();
    let mut t = Vec::new();
    for i in 0..1024u64 {
        let ti = Instant::now();
        let g = backend
            .extent_grant("v-shatter", 1, 1, i * 4096, 4096, true)
            .await.unwrap().unwrap();
        t.push(ti.elapsed().as_secs_f64() * 1e6);
        assert_eq!(g.len(), 1);
    }
    let shatter = Timings(t);
    let (q1, q4) = shatter.quartile_means();
    row("grant-4k", 1024, &shatter, "blksize-sized grants shattering ONE file");
    println!(
        "{:>14} first-quartile mean {q1:.1} µs vs last {q4:.1} µs  \
         (slope detector at row-count depth; residual drift is btree-shaped, not the old linear verify)",
        ""
    );

    // ── commit-1: worst-case single LAYOUTCOMMIT over the shatter ────
    let ti = Instant::now();
    let promoted = backend
        .extent_commit("v-shatter", 1, 1, 0, 1024 * 4096)
        .await.unwrap().unwrap();
    let commit1_ms = ti.elapsed().as_secs_f64() * 1e3;
    assert_eq!(promoted, 1024, "the whole shatter must promote");
    println!(
        "commit-1     {:>6} ops  {commit1_ms:>9.1} ms total  \
         (ONE LAYOUTCOMMIT validating+promoting all 1024 rows)",
        1
    );

    // ── commit-4k: steady-state small commits ────────────────────────
    backend.extent_register_volume("v-small", 1 << 30).await.unwrap().unwrap();
    for i in 0..1024u64 {
        backend.extent_grant("v-small", 1, 1, i * 4096, 4096, true)
            .await.unwrap().unwrap();
    }
    let mut t = Vec::new();
    for i in 0..1024u64 {
        let ti = Instant::now();
        let p = backend
            .extent_commit("v-small", 1, 1, i * 4096, 4096)
            .await.unwrap().unwrap();
        t.push(ti.elapsed().as_secs_f64() * 1e6);
        assert_eq!(p, 1);
    }
    let small = Timings(t);
    row("commit-4k", 1024, &small, "individual 4KiB LAYOUTCOMMITs (no verify in commit)");

    // ── return + reclaim: the REMOVE shape at worst-case row count ───
    let ti = Instant::now();
    let returned = backend
        .extent_layout_return("v-shatter", 1, 1, 0, 1024 * 4096)
        .await.unwrap().unwrap();
    let return_ms = ti.elapsed().as_secs_f64() * 1e3;
    assert_eq!(returned, 1024);
    let ti = Instant::now();
    let out = backend
        .extent_reclaim_complete("v-shatter", 1, 0, i64::MAX as u64, 0)
        .await.unwrap().unwrap();
    let reclaim_ms = ti.elapsed().as_secs_f64() * 1e3;
    // The RETURN merged the 1024 contiguous quiescent rows (the merge
    // policy's whole point — the shatter un-shatters at quiescence), so
    // the reclaim frees ONE row carrying all the bytes.
    assert_eq!(out.freed_extents, 1, "the shattered file merged at return");
    assert_eq!(out.freed_bytes, 1024 * 4096, "…carrying every byte");
    println!(
        "return+free  {:>6} ops  {:>9.1} ms total  \
         (LAYOUTRETURN incl. 1024-row merge {return_ms:.1} ms + reclaim_complete \
         {reclaim_ms:.1} ms of the ONE merged row)",
        2,
        return_ms + reclaim_ms
    );

    println!();
    println!(
        "Tier-1 anchors (wire path, kernel client, 8 procs — \
         docs/plans/mds-performance-plan.md):"
    );
    println!("  w2-opencl  8,689 ops/s · 0.14 cpu_ms/op   (per-open files-layout churn)");
    println!("  w3-stat    3,531 ops/s · 0.51 cpu_ms/op   (LOOKUP/GETATTR floor)");
    println!("  w1-create    489 ops/s · 0.93 cpu_ms/op   (create+write+unlink)");
    println!();
}
