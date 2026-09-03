//! Concurrency **model checking** of the delegation table, with AWS
//! `shuttle` (feature `shuttle-test`, off by default).
//!
//! ```sh
//! cargo test --features shuttle-test --lib shuttle_
//! ```
//!
//! Why this exists, in one sentence: the publish-order defect
//! (`try_grant` making a record findable through `by_stateid` before
//! it was counted in `live_per_client`) shipped, survived the whole
//! unit suite, and then took a 60_000-iteration hand-tuned rig with a
//! spin-count in it before it would reproduce even once — and it still
//! only reproduced under full-suite load. That is not a repeatable
//! test, it is a lucky one. A scheduler that can simply *decide* to
//! preempt between the two statements finds it in a handful of
//! executions, deterministically, and hands back a seed that replays.
//!
//! **What makes this work on THIS code.** shuttle only preempts at
//! primitives it owns, so `delegation.rs` aliases its `Mutex` and
//! atomics behind the feature. The `DashMap`s stay real, which is
//! sound here only because of a discipline the table already keeps:
//! every lookup clones the `Arc` out and drops the shard guard BEFORE
//! taking an entry lock (`with_live_entry`). A shard guard held across
//! a shuttle scheduling point would be a lock the scheduler cannot see
//! and would hang the execution rather than fail it. `check_invariants`
//! is the single place that does hold one across an entry lock, so it
//! is called only after every worker has joined.
//!
//! **The first thing shuttle did here was hang, not fail, and that is
//! worth knowing before writing the next one of these.** `gc_map_entry`
//! took an entry lock from inside a `DashMap::remove_if` predicate —
//! deliberate in production, where it makes the removal and the `dead`
//! marking one atomic step against the shard lock. But shuttle runs
//! every thread as a coroutine on ONE OS thread, so a coroutine that
//! parks on the entry lock is descheduled while still holding the
//! shard's real, OS-level write lock; the next coroutine to touch that
//! shard blocks the OS thread outright and the execution stops dead.
//! The backtrace was unambiguous (`DashMap::entry` →
//! `lock_exclusive_slow` on the shard, from `with_live_entry`) but
//! nothing reported it: 0% CPU, no output, no timeout. The rule that
//! falls out: under a coroutine-scheduled checker, a lock the checker
//! owns must never be taken while a lock it does not own is held. GC
//! now takes it without blocking under the feature.
//!
//! **These tests are only worth their runtime if they can fail.** Each
//! one below names the mutation it was verified RED against, and the
//! number of executions that mutation needed. A model checker that has
//! never rejected anything is indistinguishable from one that is not
//! looking.

use super::*;
use crate::nfs::v4::protocol::StateId;
use std::sync::Arc;
use std::time::Duration;

const CLIENT_A: u64 = 7;
const CLIENT_B: u64 = 9;

fn sid_of(k: u64) -> StateId {
    let mut other = [0u8; 12];
    other[..8].copy_from_slice(&k.to_be_bytes());
    StateId { seqid: 1, other }
}

fn table() -> Arc<StateManager> {
    let sm = Arc::new(StateManager::new_in_memory("shuttle"));
    // Zero cooldown: the damping timer is real wall-clock and shuttle
    // does not control time, so leaving it armed would turn "the
    // scheduler chose this interleaving" into "the clock did", and
    // most executions would refuse before reaching the code of
    // interest.
    sm.delegations.set_cooldown(Duration::from_millis(0));
    sm
}

/// A grant racing the return of the very stateid it is publishing.
///
/// This is the shipped defect's exact shape: the returner reaches the
/// record through `by_stateid`, decrements, and finishes — all inside
/// the window between the grant publishing its index and counting
/// itself live. The count then lands on a record that no longer
/// exists and never comes back down, so the client is eventually
/// refused its own quota forever.
///
/// **Verified RED** against the pre-fix ordering (index + accounting
/// moved back outside `with_live_entry`): shuttle rejected it, and
/// `check_invariants` reported the phantom.
fn publish_order_scenario() {
    let _flag = with_delegations(true);
    let sm = table();
    let ident = FileId::new(77, 4242);
    // Did the grant actually happen? Without this the whole check is
    // vacuous in the quietest possible way: a refused grant races
    // nothing, returns nothing, and violates nothing, so every
    // execution passes and the run reports 2000 green explorations of
    // an empty table.
    let granted_ok = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let granter = {
        let sm = Arc::clone(&sm);
        let granted_ok = Arc::clone(&granted_ok);
        shuttle::thread::spawn(move || {
            let got = sm.delegations.try_grant(
                ident,
                CLIENT_A,
                vec![0xB0, 0x0B],
                std::path::PathBuf::from("/race"),
                || true,
                || sid_of(1),
            );
            if got.is_ok() {
                granted_ok.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        })
    };
    let returner = {
        let sm = Arc::clone(&sm);
        shuttle::thread::spawn(move || {
            let _ = sm.delegations.return_delegation(&sid_of(1));
        })
    };
    granter.join().unwrap();
    returner.join().unwrap();

    assert!(
        granted_ok.load(std::sync::atomic::Ordering::SeqCst),
        "no grant was made, so this execution raced nothing — check the \
         delegation gate and the refusal counters before trusting a green run"
    );

    // Quiescent — see the module note on `check_invariants`.
    let violations = sm.delegations.check_invariants();
    assert!(violations.is_empty(), "invariant scan: {violations:#?}");
}

/// Two clients granting on the SAME file at the same time, each
/// returning its own stateid. Exercises the entry lock as the
/// serialising point for `records`, the two indices and the
/// per-client counters at once.
fn two_clients_scenario() {
    let _flag = with_delegations(true);
    let sm = table();
    let ident = FileId::new(77, 5150);

    let mut hs = Vec::new();
    for (n, client) in [(1u64, CLIENT_A), (2u64, CLIENT_B)] {
        let sm = Arc::clone(&sm);
        hs.push(shuttle::thread::spawn(move || {
            let got = sm.delegations.try_grant(
                ident,
                client,
                vec![0xB0, n as u8],
                std::path::PathBuf::from("/two"),
                || true,
                || sid_of(n),
            );
            if got.is_ok() {
                let _ = sm.delegations.return_delegation(&sid_of(n));
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }

    let violations = sm.delegations.check_invariants();
    assert!(violations.is_empty(), "invariant scan: {violations:#?}");
    assert_eq!(
        sm.delegations.live_count(),
        0,
        "every grant that succeeded was returned by the same thread, so nothing may be live"
    );
}

/// A grant racing a whole-client teardown
/// (`cleanup_client_delegations`, the CLIENT-teardown path). The
/// teardown walks `by_client`, which the grant is appending to — the
/// other index, reached by the other path.
fn grant_racing_client_teardown_scenario() {
    let _flag = with_delegations(true);
    let sm = table();
    let ident = FileId::new(77, 6060);

    let granter = {
        let sm = Arc::clone(&sm);
        shuttle::thread::spawn(move || {
            let _ = sm.delegations.try_grant(
                ident,
                CLIENT_A,
                vec![0xB0, 0x0C],
                std::path::PathBuf::from("/teardown"),
                || true,
                || sid_of(1),
            );
        })
    };
    let reaper = {
        let sm = Arc::clone(&sm);
        shuttle::thread::spawn(move || {
            let _ = sm.delegations.cleanup_client_delegations(CLIENT_A);
        })
    };
    granter.join().unwrap();
    reaper.join().unwrap();

    let violations = sm.delegations.check_invariants();
    assert!(violations.is_empty(), "invariant scan: {violations:#?}");
}

// ── the checks ───────────────────────────────────────────────────────
//
// **`check_dfs`, not `check_pct`, and that is a measured choice rather
// than a taste.** The first cut used `check_pct(scenario, 2_000, 3)` —
// randomized Probabilistic Concurrency Testing, the usual
// recommendation. Against the KNOWN-BAD pre-fix ordering it passed all
// 2000 executions: a green result that proved nothing. Exhaustive DFS
// found the same bug in **1.24 seconds**, and printed the schedule to
// replay it with:
//
// ```text
// invariant scan: ["live_per_client[7] 1 != 0 live records"]
// failing schedule: "910226f8acd1910100004992249224494a120800000000"
// ```
//
// That is the production defect exactly, and worth holding next to what
// it cost to find the first time: 60_000 iterations in a hand-tuned rig
// with a spin count in it, reproducing only in-suite on a loaded 2-vCPU
// Linux box.
//
// Why PCT missed it: the vulnerable window is two statements wide, and
// the returner has to be scheduled inside it AND run its whole
// decrement before the granter resumes. PCT samples few-preemption
// schedules at random priority-change points; nothing aims it at a
// specific two-operation seam. These scenarios are small enough to
// enumerate, so enumerate them.
//
// Keep them small. DFS is exhaustive, so every extra thread or
// operation multiplies the space — that is the price of the teeth.

#[test]
fn shuttle_a_grant_is_counted_before_it_becomes_findable() {
    shuttle::check_dfs(publish_order_scenario, None);
}

/// Bounded, unlike its neighbours, and the bound is not a formality:
/// unbounded DFS on this one does not finish in 15 minutes. Two threads
/// that each grant AND return, against a table whose every map
/// operation is a scheduling point, is a bigger space than the
/// two-operation race next door. The cap buys a fixed ~seconds of
/// exploration; it does NOT prove exhaustion, so this check is evidence
/// and not a proof.
#[test]
fn shuttle_two_clients_grant_and_return_on_one_file() {
    shuttle::check_dfs(two_clients_scenario, Some(20_000));
}

#[test]
fn shuttle_a_grant_racing_its_client_teardown() {
    shuttle::check_dfs(grant_racing_client_teardown_scenario, None);
}
