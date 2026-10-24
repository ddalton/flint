//! The post-checkout re-hash (latency plan §17; LeanScanRacy's gate): a
//! checkout measures its entries' racy flags when it opens the agent's gate
//! (the marker), not when it wrote each file, so the first scan hashes only
//! what it wrote in its last window.

use std::sync::Arc;
use std::time::Duration;

use flint_store::memory::MemoryStore;

use super::barrier::CONSUME_WINDOW_HOOK;
use super::tests::{read, syncer, write};

#[tokio::test]
async fn a_checkout_measures_racy_at_the_gate_not_at_each_write() {
    // (the gate opens a window after the writes, the rule is on, the fewest
    // racy files the checkout waits out; 0 never waits)
    for (held, on, wait_min) in [(true, true, 0), (false, true, 0), (true, false, 0), (false, true, 20), (false, true, 21)] {
        let store = Arc::new(MemoryStore::new());
        let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut a = syncer(&store, dir_a.path()).await;
        a.checkout().await.unwrap();
        for i in 0..20 {
            write(dir_a.path(), &format!("f{i:02}.txt"), "seed");
        }
        a.floor_tick().await.unwrap();
        let mut b = syncer(&store, dir_b.path()).await;
        b.cfg.racy_at_gate = on;
        b.cfg.racy_wait_min = wait_min;
        if held {
            // The checkout's tail: the gate opens a window after the writes.
            CONSUME_WINDOW_HOOK.with(|h| {
                *h.borrow_mut() = Some((
                    String::new(),
                    "before-gate",
                    Box::new(|| std::thread::sleep(Duration::from_millis(2_100))),
                ))
            });
        }
        b.checkout().await.unwrap();
        assert!(CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_none()), "fixture: the gate's window never ran");
        let base = b.state.load_baseline().unwrap();
        assert_eq!(base.entries.len(), 20, "fixture: the checkout did not materialise the tree");
        let racy = base.entries.values().filter(|e| e.racy).count();
        let waits = on && wait_min > 0 && wait_min <= 20;
        if (held && on) || waits {
            assert_eq!(racy, 0, "a checkout's entries are still racy a window after the gate (wait_min {wait_min})");
            // What the gate vouches for still catches the agent: a same-size
            // rewrite right after it is a later mtime, and publishes.
            write(dir_b.path(), "f07.txt", "SEED");
            let r = b.run_barrier().await.unwrap();
            assert_eq!(r.uploaded, vec!["f07.txt".to_string()], "an agent's write after the gate was not published");
            assert_eq!(read(dir_b.path(), "f07.txt").as_deref(), Some("SEED"));
        } else if on {
            assert_eq!(racy, 20, "the gate opened at once (wait_min {wait_min}), so every entry the checkout wrote is racy");
        } else {
            assert_eq!(racy, 20, "FLINT_SYNC_RACY_AT_GATE=off: every entry the checkout wrote stays racy");
        }
    }
}
