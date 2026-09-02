//! The concurrency stress (docs/plans/nfs-delegations-design.md §9):
//! readers racing a writer loop over hardlink pairs, ten thousand
//! grant attempts, through the PRODUCTION funnel (`deleg_fence`) with
//! the real stateid and delegation managers.
//!
//! What it is for. The fence protocol's whole argument is an
//! interleaving one — every ordering either sees the mutator (refuse)
//! or the mutator's consult sees the record (recall) — and the unit
//! tests above it are all sequential. This is the leg that runs the
//! interleavings for real, and it scores three things the design
//! names:
//!
//! 1. the **post-run invariant scan** (`check_invariants`): index and
//!    counter consistency, and `live guard ⇒ no Granted record`;
//! 2. the **write-time check**: while a write open is registered on a
//!    file, no client holds a Granted delegation on it — probed from
//!    inside the writer's window, where a post-hoc scan cannot look;
//!    plus the release-time exclusivity assert armed on every
//!    proceeding guard (`MutationGuard::drop`), which fires in the
//!    middle of the run if any interleaving lets a foreign record
//!    survive a fence;
//! 3. a **granted floor**, so a run in which the rig refuses
//!    everything (and therefore can never violate anything) is a
//!    failure, not a pass.
//!
//! Hardlink pairs are the point of the `(dev,ino)` keying: each file
//! is reached through two filehandles, and the writer's open lands on
//! whichever alias the reader is NOT using. An fh-keyed write-open
//! predicate would answer "no writers" about the other name and the
//! grant would land inside a write window — which is exactly what
//! the write-time probe would catch.

use super::*;
use crate::nfs::v4::protocol::StateId;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FILES: u64 = 8;
const READERS: u64 = 4;
const READER_ITERS: usize = 2500; // 4 × 2500 = 10k grant attempts
const WRITERS: u64 = 2;
const WRITER_ITERS: usize = 1000;

/// A cheap deterministic-per-thread PRNG; the interleavings are the
/// randomness that matters, and they come from the scheduler.
fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn ident_of(file: u64) -> (u64, u64) {
    (77, 1000 + file)
}

/// The two names of one inode.
fn alias_fh(file: u64, alias: u64) -> Vec<u8> {
    vec![0xA0, file as u8, alias as u8]
}

#[test]
fn readers_racing_writers_over_hardlink_pairs_hold_the_fence_invariants() {
    let _flag = with_delegations(true);
    let sm = Arc::new(StateManager::new_in_memory("stress"));
    // The shipped 30s cooldown would turn the run into eight grants
    // and ten thousand Cooldown refusals. A millisecond keeps the
    // damping semantics (the writer's retry beats the re-grant) at a
    // scale the run can see through.
    sm.delegations.set_cooldown(Duration::from_millis(1));

    // The cooperative client: every recall order is acked and
    // returned by a background thread, so the writer's DELAY resolves
    // the way it does against a healthy Linux client — a little later,
    // not immediately, which is what widens the under-recall window
    // the readers hammer.
    let queue: Arc<Mutex<VecDeque<RecallOrder>>> = Arc::new(Mutex::new(VecDeque::new()));
    {
        let q = Arc::clone(&queue);
        sm.install_recall_spawner(Arc::new(move |orders| {
            q.lock().unwrap().extend(orders);
        }));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let grants = Arc::new(AtomicU64::new(0));
    let grants_by_alias = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
    let writer_delays = Arc::new(AtomicU64::new(0));
    let writer_proceeds = Arc::new(AtomicU64::new(0));
    let returned_by_client = Arc::new(AtomicU64::new(0));

    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        // ── the client's recall handler ──────────────────────────────
        {
            let sm = Arc::clone(&sm);
            let queue = Arc::clone(&queue);
            let stop = Arc::clone(&stop);
            let returned = Arc::clone(&returned_by_client);
            scope.spawn(move || loop {
                let next = queue.lock().unwrap().pop_front();
                match next {
                    Some(order) => {
                        std::thread::yield_now();
                        sm.delegations.note_first_transmit(&order.stateid);
                        sm.delegations.note_recall_acked(&order.stateid);
                        // A reader may have returned it voluntarily
                        // first; either way the record resolves.
                        if sm.delegations.return_delegation(&order.stateid).is_ok() {
                            returned.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    None => {
                        if stop.load(Ordering::Acquire) && queue.lock().unwrap().is_empty() {
                            break;
                        }
                        std::thread::yield_now();
                    }
                }
            });
        }

        // ── readers: OPEN-for-read = try_grant, the ioops shape ───────
        for r in 0..READERS {
            let sm = Arc::clone(&sm);
            let grants = Arc::clone(&grants);
            let by_alias = [Arc::clone(&grants_by_alias[0]), Arc::clone(&grants_by_alias[1])];
            workers.push(scope.spawn(move || {
                let client = 1 + r;
                let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ (r + 1);
                let mut held: Vec<StateId> = Vec::new();
                for i in 0..READER_ITERS {
                    let file = xorshift(&mut rng) % FILES;
                    let alias = xorshift(&mut rng) % 2;
                    let (dev, ino) = ident_of(file);
                    let fh = alias_fh(file, alias);
                    let res = sm.delegations.try_grant(
                        FileId::new(dev, ino),
                        client,
                        fh.clone(),
                        std::path::PathBuf::from(format!("/f{file}/name{alias}")),
                        || {
                            !sm.stateids.file_has_write_open(dev, ino)
                                && !sm.write_layout_held_by_other(&format!("f{file}"), client)
                        },
                        || sm.stateids.allocate_delegation(client, fh.clone()),
                    );
                    if let Ok(sid) = res {
                        grants.fetch_add(1, Ordering::Relaxed);
                        by_alias[alias as usize].fetch_add(1, Ordering::Relaxed);
                        held.push(sid);
                    }
                    // Voluntary returns keep the holder set churning so
                    // the same file is re-granted many times over.
                    if i % 3 == 0 {
                        if let Some(sid) = held.pop() {
                            let _ = sm.delegations.return_delegation(&sid);
                        }
                    }
                }
                for sid in held {
                    let _ = sm.delegations.return_delegation(&sid);
                }
            }));
        }

        // ── writers: OPEN-for-write → WRITE → CLOSE, the handler shape ─
        for w in 0..WRITERS {
            let sm = Arc::clone(&sm);
            let delays = Arc::clone(&writer_delays);
            let proceeds = Arc::clone(&writer_proceeds);
            workers.push(scope.spawn(move || {
                let client = 10 + w;
                let mut rng = 0xD1B5_4A32_D192_ED03u64 ^ (w + 1);
                for _ in 0..WRITER_ITERS {
                    let file = xorshift(&mut rng) % FILES;
                    let alias = xorshift(&mut rng) % 2;
                    let (dev, ino) = ident_of(file);
                    let fh = alias_fh(file, alias);
                    let ident = FileId::new(dev, ino);
                    // OPEN(write): fence, then register the open under
                    // the guard, then drop the guard — the open state
                    // outlives the OPEN handler, as it does in ioops.
                    let mut spins = 0u64;
                    let open_sid = loop {
                        match sm.deleg_fence((dev, ino), Some(client), false, "open_write") {
                            FenceVerdict::Proceed(g) => {
                                let sid = sm.stateids.record_open(
                                    client,
                                    format!("w{client}").into_bytes(),
                                    fh.clone(),
                                    2, // OPEN4_SHARE_ACCESS_WRITE
                                    0,
                                    None,
                                    Some((dev, ino)),
                                );
                                drop(g);
                                break sid;
                            }
                            FenceVerdict::Delay => {
                                delays.fetch_add(1, Ordering::Relaxed);
                                spins += 1;
                                assert!(
                                    spins < 200_000,
                                    "writer {client} starved on file {file}: the recall never \
                                     resolved or grants outran every retry"
                                );
                                std::thread::yield_now();
                            }
                        }
                    };
                    proceeds.fetch_add(1, Ordering::Relaxed);

                    // THE WRITE WINDOW. The guard is gone; only the
                    // registered write open protects this file now, via
                    // the grant's precheck — through EITHER alias.
                    for _ in 0..3 {
                        std::thread::yield_now();
                        let holders = sm.delegations.granted_holders(ident);
                        assert!(
                            holders.is_empty(),
                            "file {file}: clients {holders:?} hold a GRANTED delegation while \
                             client {client} has a write open registered on alias {alias} — \
                             the write-open predicate missed the other name"
                        );
                    }
                    // WRITE itself: fenced again (site 6). The own open is
                    // not a record, so this proceeds without delay.
                    match sm.deleg_fence((dev, ino), Some(client), false, "write") {
                        FenceVerdict::Proceed(g) => {
                            assert!(sm.delegations.granted_holders(ident).is_empty());
                            drop(g);
                        }
                        FenceVerdict::Delay => {
                            // A reader's grant can only have landed if the
                            // precheck failed to see our open: a defect.
                            panic!(
                                "file {file}: WRITE delayed while client {client} holds the \
                                 write open — a delegation was granted over a live writer"
                            );
                        }
                    }
                    sm.stateids.close_open_state(&open_sid.other);
                }
            }));
        }
        // Readers and writers done ⇒ release the client thread, which
        // drains what is left and exits; the scope then joins it. (The
        // first cut set `stop` AFTER the scope, which joins the client
        // thread that was waiting for `stop`: a deadlock that showed up
        // as a ten-minute silent hang.)
        for h in workers {
            h.join().expect("a worker panicked — see its assertion above");
        }
        stop.store(true, Ordering::Release);
    });

    // ── the scores ───────────────────────────────────────────────────
    let violations = sm.delegations.check_invariants();
    assert!(violations.is_empty(), "invariant scan: {violations:#?}");

    let g = grants.load(Ordering::Relaxed);
    let (a0, a1) = (
        grants_by_alias[0].load(Ordering::Relaxed),
        grants_by_alias[1].load(Ordering::Relaxed),
    );
    let d = writer_delays.load(Ordering::Relaxed);
    let p = writer_proceeds.load(Ordering::Relaxed);
    let m = sm.delegations.meter();
    eprintln!(
        "deleg stress: grants {g} (alias0 {a0}, alias1 {a1}) · writer delays {d} proceeds {p} · \
         returned-by-client {} · delay(open_write) {} delay(write) {} · revoked {}",
        returned_by_client.load(Ordering::Relaxed),
        m.delay_count("open_write"),
        m.delay_count("write"),
        m.revoked_total(),
    );
    // The floor: a rig that grants nothing can violate nothing.
    assert!(g >= 200, "granted floor: only {g} of {} attempts granted", READERS as usize * READER_ITERS);
    assert!(a0 > 0 && a1 > 0, "both hardlink aliases must have been granted through");
    // The rig must have SEEN contention, or the interleavings it
    // claims to exercise never happened.
    assert!(d > 0, "no writer was ever DELAYed — readers and writers never overlapped");
    assert_eq!(p, (WRITERS as usize * WRITER_ITERS) as u64, "every writer iteration proceeded");
    assert_eq!(m.delay_count("open_write"), d, "every DELAY is attributed to its site");
    assert_eq!(m.delay_count("write"), 0, "a WRITE under its own open is never delayed");
    assert_eq!(m.revoked_total(), 0, "a cooperative client is never revoked");
    assert!(returned_by_client.load(Ordering::Relaxed) > 0, "the recall path ran");
    // Quiescent: everything returned, nothing under recall, no leaks.
    assert_eq!(sm.delegations.live_count(), 0);
    assert_eq!(sm.delegations.files_under_recall(), 0);
    for c in 1..=READERS {
        assert_eq!(sm.delegations.count_for_client(c), 0, "client {c} leaked records");
    }
}

/// The release-time exclusivity assert must be REAL: a guard armed
/// for a proceeding mutator that finds a foreign non-revoked record
/// at release panics (debug builds). This drives one by hand, through
/// the private state the funnel would never produce — the only way to
/// prove the assert is not dead code.
#[test]
#[should_panic(expected = "deleg fence hole")]
#[cfg(debug_assertions)]
fn the_release_time_exclusivity_check_fires_on_a_foreign_live_record() {
    let m = DelegationManager::with_limits(4096, 65536, Duration::ZERO);
    let ident = FileId::new(5, 5);
    // A proceeding (checked) guard for client 2 on an empty file…
    let guard = match m.mutation_fence(ident, Some(2), false) {
        FenceOutcome::Clear(g) => g,
        _ => unreachable!(),
    };
    // …then a foreign grant lands anyway. `try_grant` refuses this
    // (MutationPending), so the record is planted underneath it.
    m.plant_record_for_test(
        ident,
        1,
        StateId { seqid: 1, other: [3u8; 12] },
    );
    drop(guard); // panics here
}

/// The publish-order regression: a grant must be COUNTED before it is
/// FINDABLE.
///
/// `try_grant` used to push the record under the entry lock and then
/// index it and bump the live counters after releasing that lock.
/// Every removal path reaches a record through `by_stateid`, so the
/// gap between the index insert and the counter bump is a window in
/// which a return can run to completion — and its decrement is
/// silently dropped, because `dec_live_client` is a no-op on a
/// `live_per_client` entry that does not exist yet. The grant's
/// `+= 1` then lands on a record that is already gone: a phantom live
/// delegation that never clears, and which counts against the
/// client's quota for the life of the server. `live_global` survives
/// the identical interleaving (fetch_sub and fetch_add on a u64
/// commute), which is why only the per-client map ever showed it.
///
/// Reaching the window is the whole trick. A returner that learns the
/// stateid from the grant's RETURN VALUE is by construction too late,
/// so the granter publishes the id it is ABOUT to mint and the
/// returner spins on it — already inside the window before the grant
/// opens it. That is what makes this loud where the full stress rig
/// was rare: the leak took a 2-vCPU Linux box under whole-suite load
/// to surface once, and 20 standalone runs under CPU hogs never
/// reproduced it.
///
/// The ids are FRESH every grant, as the real `allocate_delegation`
/// mints them. An earlier cut of this rig reused one stateid forever
/// and produced index violations of its own — `unindex` removes by
/// key, so a reused id lets a return delete the index of the NEXT
/// record. That is a property of the rig, not of the server, and a
/// rig that manufactures its own violations cannot testify about the
/// server's.
#[test]
fn a_grant_is_counted_before_it_becomes_findable() {
    let _flag = with_delegations(true);
    let sm = Arc::new(StateManager::new_in_memory("publish-order"));
    sm.delegations.set_cooldown(Duration::from_millis(0));

    const ITERS: usize = 60_000;
    const CLIENT: u64 = 7;
    let ident = FileId::new(77, 4242);

    fn sid_of(k: u64) -> StateId {
        let mut other = [0u8; 12];
        other[..8].copy_from_slice(&k.to_be_bytes());
        StateId { seqid: 1, other }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let pending = Arc::new(AtomicU64::new(1));
    let grants = Arc::new(AtomicU64::new(0));
    let returns = Arc::new(AtomicU64::new(0));

    std::thread::scope(|scope| {
        {
            let sm = Arc::clone(&sm);
            let stop = Arc::clone(&stop);
            let pending = Arc::clone(&pending);
            let returns = Arc::clone(&returns);
            scope.spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    // Both the id the granter is publishing NOW and the
                    // one before it: the granter advances as soon as a
                    // grant lands, so the live record can be either.
                    // Spinning on only one of them deadlocks the rig.
                    let k = pending.load(Ordering::Acquire);
                    for cand in [k, k.saturating_sub(1)] {
                        if cand > 0 && sm.delegations.return_delegation(&sid_of(cand)).is_ok() {
                            returns.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            });
        }
        let granter = {
            let sm = Arc::clone(&sm);
            let pending = Arc::clone(&pending);
            let grants = Arc::clone(&grants);
            scope.spawn(move || {
                let mut k: u64 = 1;
                for _ in 0..ITERS {
                    pending.store(k, Ordering::Release);
                    let got = sm.delegations.try_grant(
                        ident,
                        CLIENT,
                        vec![0xB0, 0x0B],
                        std::path::PathBuf::from("/race"),
                        || true,
                        || sid_of(k),
                    );
                    if got.is_ok() {
                        grants.fetch_add(1, Ordering::Relaxed);
                        // Clear it ourselves if the returner did not
                        // get there first. Without this the loop is
                        // mostly AlreadyHolder refusals waiting on the
                        // other thread, and the only way to make
                        // progress is `yield_now` on each one — a
                        // syscall per refusal that made this the
                        // slowest test in the suite (35s in the 2-vCPU
                        // VM) and starved its neighbours enough to
                        // fail an unrelated tier test. Spinning
                        // instead is worse: the granter monopolises
                        // its core and the returner never runs, which
                        // collapses the run to ~10 grants. Doing the
                        // cleanup here means every iteration grants,
                        // so the races per second go up while the
                        // syscalls go to zero. The returner still
                        // races the publish window — and only IT can
                        // trip the defect, since this return is
                        // ordered after the grant on the same thread.
                        //
                        // Spin briefly first so the returner gets a
                        // real chance at the entry lock before we take
                        // it back. Without this the granter reclaims
                        // the record almost every time, and the
                        // returner's win rate collapses — the FIX in
                        // particular holds the entry lock across the
                        // whole publish, so the returner is already
                        // fighting a narrower window than it is in the
                        // control arm.
                        for _ in 0..256 {
                            std::hint::spin_loop();
                        }
                        let _ = sm.delegations.return_delegation(&sid_of(k));
                        k += 1;
                    }
                }
            })
        };
        granter.join().expect("granter panicked");
        stop.store(true, Ordering::Release);
    });

    // Settle: drop whatever the last iteration left, so the scan reads
    // a quiescent manager rather than a half-finished handoff.
    let k = pending.load(Ordering::Acquire);
    for cand in [k, k.saturating_sub(1)] {
        if cand > 0 {
            let _ = sm.delegations.return_delegation(&sid_of(cand));
        }
    }

    let g = grants.load(Ordering::Relaxed);
    let r = returns.load(Ordering::Relaxed);
    let violations = sm.delegations.check_invariants();
    eprintln!("publish-order race: grants {g} · returns {r} · violations {violations:?}");

    // Floors first: a run that granted nothing, or in which the
    // returner never won a race, never opened the window and cannot
    // testify about it. INCONCLUSIVE is not PASS.
    assert!(g > 500, "granted floor: only {g} of {ITERS} attempts granted");
    // 8 control runs won between 572 and 2424 races; 100 is well clear
    // of a flake and still says plainly that the returner was inside
    // the window, many times, rather than watching from outside it.
    assert!(
        r > 100,
        "return floor: the returner only won {r} races — it never entered the window"
    );
    assert!(violations.is_empty(), "invariant scan: {violations:#?}");
}
