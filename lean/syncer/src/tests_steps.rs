//! `BarrierReport::steps`: where a barrier's time went. The steps must
//! TILE the barrier — every request it sent is charged to exactly one
//! step — or a step's numbers say nothing, so these tests compare the
//! steps' requests with the store's own count across the barrier.

use super::tests::{syncer, write};
use super::*;
use flint_store::memory::MemoryStore;
use flint_store::RequestCounts;
use std::sync::Arc;

fn names(r: &barrier::BarrierReport) -> Vec<&'static str> {
    r.steps.iter().map(|s| s.step).collect()
}

/// The steps' requests, summed.
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

/// What the store sent between two of its counts.
fn sent(before: RequestCounts, after: RequestCounts) -> RequestCounts {
    RequestCounts {
        get: after.get - before.get,
        head: after.head - before.head,
        put: after.put - before.put,
        copy: after.copy - before.copy,
        delete: after.delete - before.delete,
        list: after.list - before.list,
        multipart: after.multipart - before.multipart,
    }
}

const COMMIT: [&str; 23] = [
    "cell",
    "rescope_replay",
    "consume",
    "baseline_load",
    "scan",
    "intent",
    "upload",
    "claim",
    "verify",
    "load",
    "fence",
    "merge",
    "cas",
    "retire_set",
    "surface",
    "retire_ledger",
    "reap",
    "orphan_sweep",
    "baseline",
    "generation_sweep",
    "chunk_sweep",
    "free",
    "release",
];

#[tokio::test]
async fn a_publishing_barrier_charges_every_request_to_one_of_its_steps() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    write(dir.path(), "a.txt", "v1");
    write(dir.path(), "b.txt", "v1");
    a.run_barrier().await.unwrap();

    // The second publish replaces a version, so its commit retires and
    // reaps one: every step of the section has something to do.
    write(dir.path(), "a.txt", "v2, longer");
    let before = store.request_counts().unwrap();
    let started = std::time::Instant::now();
    let r = a.run_barrier().await.unwrap();
    let wall = started.elapsed().as_micros() as u64;
    let after = store.request_counts().unwrap();

    assert_eq!(r.uploaded, vec!["a.txt".to_string()]);
    assert_eq!(names(&r), COMMIT.to_vec(), "{:#?}", r.steps);
    assert_eq!(charged(&r), sent(before, after), "a request no step was charged for: {:#?}", r.steps);
    let timed: u64 = r.steps.iter().map(|s| s.us).sum();
    assert!(timed <= wall, "the steps ({timed} us) outlast the barrier ({wall} us)");
    let step = |n: &str| r.steps.iter().find(|s| s.step == n).unwrap().requests.unwrap();
    assert!(step("upload").put >= 1, "the one changed file's PUT is the upload step's");
    assert_eq!(step("verify").head, 1, "the commit re-reads its one upload");
    assert!(step("cas").put >= 1, "the pointer CAS is the cas step's");
    // The battery's retire grace is 0, so the version this CAS replaced
    // is due at once and this same section reaps it.
    assert!(step("reap").delete >= 1, "the replaced version is the reap step's DELETE");
}

#[tokio::test]
async fn a_barrier_with_nothing_to_publish_takes_no_step_of_the_commit_section() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    write(dir.path(), "a.txt", "v1");
    a.run_barrier().await.unwrap();

    let before = store.request_counts().unwrap();
    let r = a.run_barrier().await.unwrap();
    let after = store.request_counts().unwrap();

    assert!(r.no_change);
    assert_eq!(names(&r), vec!["cell", "rescope_replay", "consume", "baseline_load", "scan", "idle_check"]);
    assert_eq!(charged(&r), sent(before, after), "{:#?}", r.steps);
}
