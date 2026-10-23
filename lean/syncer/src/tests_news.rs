//! Lever B, the news poll (`news.rs`; latency plan 2026-10-23).

use std::sync::Arc;
use std::time::{Duration, Instant};

use flint_store::memory::MemoryStore;
use flint_store::{ObjectStore, RequestCounts};

use super::news::NewsClock;
use super::sentinel::Verb;
use super::tests::{cfg_for, read, syncer, write};
use super::{inbox, Access, Syncer};

fn counts(store: &MemoryStore) -> RequestCounts {
    store.request_counts().unwrap()
}

fn since(a: &RequestCounts, b: &RequestCounts) -> (u64, u64, u64) {
    (b.get - a.get, b.list - a.list, (b.put + b.copy + b.delete + b.multipart + b.head) - (a.put + a.copy + a.delete + a.multipart + a.head))
}

async fn reader(store: &Arc<MemoryStore>, root: &std::path::Path) -> Syncer {
    let mut sc = syncer(store, root).await;
    sc.cfg.access = Access::Read;
    sc
}

/// A writer A and a writer B, both checked out at `x.txt` = v1.
async fn two_writers(store: &Arc<MemoryStore>, a_root: &std::path::Path, b_root: &std::path::Path) -> (Syncer, Syncer) {
    let mut a = syncer(store, a_root).await;
    a.checkout().await.unwrap();
    write(a_root, "x.txt", "v1");
    a.floor_tick().await.unwrap();
    let mut b = syncer(store, b_root).await;
    b.checkout().await.unwrap();
    b.floor_tick().await.unwrap();
    assert_eq!(read(b_root, "x.txt").as_deref(), Some("v1"), "fixture: B did not check out A's file");
    (a, b)
}

#[tokio::test]
async fn a_peers_publish_is_news_the_ticker_shows_before_the_news_tick_takes_it() {
    let store = Arc::new(MemoryStore::new());
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut a, mut b) = two_writers(&store, da.path(), db.path()).await;
    assert_eq!(b.news_poll().await.unwrap(), None, "fixture: news before the peer published");

    write(da.path(), "x.txt", "v2 from A");
    a.floor_tick().await.unwrap();
    let seq = manifest_seq(&a).await;

    assert_eq!(b.news_poll().await.unwrap(), Some(seq), "the peer's publish is not news");
    let t = b.load_remote_seq();
    assert_eq!(t.observed_seq, seq, "the ticker does not show the news");
    assert!(t.integrated_seq < seq, "the ticker says integrated before anything was taken: {t:?}");
    assert_eq!(read(db.path(), "x.txt").as_deref(), Some("v1"), "the poll itself wrote the tree");

    let o = b.news_tick().await.unwrap();
    assert_eq!(o.consumed, 1, "the news tick did not take the peer's change: {o:?}");
    assert_eq!(read(db.path(), "x.txt").as_deref(), Some("v2 from A"));
    assert!(b.load_remote_seq().integrated_seq >= seq, "the ticker still says not integrated");
    assert_eq!(b.news_poll().await.unwrap(), None, "news already taken is news again");
}

async fn manifest_seq(sc: &Syncer) -> u64 {
    super::manifest::load_pointer(sc.store.as_ref(), &sc.cfg).await.unwrap().unwrap().pointer.seq
}

/// The tree's own install is no news: the ticker already names it, so an
/// agent's own publish never costs it a second barrier.
#[tokio::test]
async fn a_writers_own_publish_is_not_news() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    write(dir.path(), "x.txt", "v1");
    let o = a.floor_tick().await.unwrap();
    assert_eq!(o.uploaded, 1, "fixture: nothing published");
    assert_eq!(a.news_poll().await.unwrap(), None, "the writer's own install reads as news");
    write(dir.path(), "x.txt", "v2, a size apart");
    let o = a.news_tick().await.unwrap();
    assert_eq!(o.uploaded, 1, "fixture: the news tick published nothing");
    assert_eq!(a.news_poll().await.unwrap(), None, "the news tick's own install reads as news");
}

/// A checkout writes no ticker, so without `news_start` a fresh tree's
/// first poll reads its own checkout as news (the control below).
#[tokio::test]
async fn a_fresh_checkout_is_not_news_once_the_ticker_is_primed() {
    let store = Arc::new(MemoryStore::new());
    let (da, db, dc) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, da.path()).await;
    a.checkout().await.unwrap();
    write(da.path(), "x.txt", "v1");
    a.floor_tick().await.unwrap();

    let mut b = syncer(&store, db.path()).await;
    b.checkout().await.unwrap();
    b.news_start().unwrap();
    assert_eq!(b.news_poll().await.unwrap(), None, "a fresh checkout reads as news");

    let mut c = syncer(&store, dc.path()).await;
    c.checkout().await.unwrap();
    assert!(c.news_poll().await.unwrap().is_some(), "control: an unprimed ticker no longer reads the checkout as news");
}

/// What a poll costs: a writer's cell and pointer, a reader's pointer.
/// No LIST, no write, no HEAD, and no chunk.
#[tokio::test]
async fn a_poll_is_two_gets_for_a_writer_and_one_for_a_reader() {
    let store = Arc::new(MemoryStore::new());
    let (da, dr) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, da.path()).await;
    a.checkout().await.unwrap();
    write(da.path(), "x.txt", "v1");
    a.floor_tick().await.unwrap();
    let mut r = reader(&store, dr.path()).await;
    r.checkout().await.unwrap();
    r.floor_tick().await.unwrap();

    let c0 = counts(&store);
    assert_eq!(a.news_poll().await.unwrap(), None);
    let c1 = counts(&store);
    assert_eq!(since(&c0, &c1), (2, 0, 0), "a writer's poll is not the cell and the pointer");
    assert_eq!(r.news_poll().await.unwrap(), None);
    let c2 = counts(&store);
    assert_eq!(since(&c1, &c2), (1, 0, 0), "a reader's poll is not the pointer alone");

    // With news the poll costs the same: what it learns rides those reads.
    write(da.path(), "x.txt", "v2, a size apart");
    a.floor_tick().await.unwrap();
    let c3 = counts(&store);
    assert!(r.news_poll().await.unwrap().is_some(), "fixture: no news");
    assert_eq!(since(&c3, &counts(&store)), (1, 0, 0), "news costs the reader's poll more than the pointer");
}

/// §2.5's door at the poll's pace: a gateway boundary request becomes the
/// pending publish the sentinel arm honours, one poll after it is made,
/// not at the next barrier.
#[tokio::test]
async fn a_gateway_boundary_request_is_pending_one_poll_later() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    inbox::gateway_request(store.as_ref(), &a.cfg, inbox::RequestedVerb::Boundary, "ci@example").await.unwrap();
    assert!(a.load_pending(Verb::Publish).unwrap().is_none(), "fixture: pending before the poll");
    a.news_poll().await.unwrap();
    let p = a.load_pending(Verb::Publish).unwrap().expect("the poll did not read the gateway's request");
    assert!(p.nonces.iter().any(|n| n.contains("ci@example")), "{:?}", p.nonces);
}

/// The early barrier is the floor's barrier without the floor's honours:
/// a pending publish (here the gateway's) is left to the sentinel arm and
/// the floor, under their interval and budget. The floor, run after, does
/// honour it: the fixture's pending was honourable.
#[tokio::test]
async fn the_news_tick_leaves_a_pending_publish_to_the_floor_and_the_sentinel_arm() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    write(dir.path(), "x.txt", "v1");
    a.request_boundary("n-1", None).unwrap();

    a.news_tick().await.unwrap();
    assert!(a.load_pending(Verb::Publish).unwrap().is_some(), "the news tick honoured the pending publish");

    let o = a.floor_tick().await.unwrap();
    assert!(a.load_pending(Verb::Publish).unwrap().is_none(), "fixture: the floor did not honour it either");
    assert!(o.acks.iter().any(|k| k.nonces.iter().any(|n| n == "n-1")), "no ack names the request: {:?}", o.acks);
}

#[tokio::test]
async fn a_reader_takes_news_on_its_news_tick_and_writes_nothing() {
    let store = Arc::new(MemoryStore::new());
    let (da, dr) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = syncer(&store, da.path()).await;
    a.checkout().await.unwrap();
    write(da.path(), "x.txt", "v1");
    a.floor_tick().await.unwrap();
    let mut r = reader(&store, dr.path()).await;
    r.checkout().await.unwrap();
    r.floor_tick().await.unwrap();

    write(da.path(), "x.txt", "v2 from A");
    a.floor_tick().await.unwrap();
    let c0 = counts(&store);
    assert!(r.news_poll().await.unwrap().is_some(), "the writer's publish is not news to the reader");
    r.news_tick().await.unwrap();
    assert_eq!(read(dr.path(), "x.txt").as_deref(), Some("v2 from A"));
    assert_eq!(since(&c0, &counts(&store)).2, 0, "the reader wrote to the bucket");
}

/// The arm as the loop drives it, on a made-up clock: a peer's publish is
/// taken on the next tick; a second, inside the minimum interval, is told
/// (the ticker) but waits; past the interval it is taken; and an idle
/// workspace polls nothing at all.
#[tokio::test]
async fn the_news_arm_takes_news_at_once_then_at_most_once_an_interval_and_idles_at_zero_requests() {
    let store = Arc::new(MemoryStore::new());
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mut a, mut b) = two_writers(&store, da.path(), db.path()).await;
    let t0 = Instant::now();
    let mut clock = NewsClock::new(&b.cfg, t0);
    let at = |s: u64| t0 + Duration::from_secs(s);

    assert!(b.news_arm(&mut clock, at(0)).await.unwrap().is_none(), "a barrier with no news");

    write(da.path(), "x.txt", "v2 .");
    a.floor_tick().await.unwrap();
    let o = b.news_arm(&mut clock, at(1)).await.unwrap().expect("news did not run the barrier");
    assert_eq!((o.consumed, read(db.path(), "x.txt").as_deref()), (1, Some("v2 .")));

    write(da.path(), "x.txt", "v3 ..");
    a.floor_tick().await.unwrap();
    assert!(b.news_arm(&mut clock, at(2)).await.unwrap().is_none(), "a second barrier inside the interval");
    assert!(clock.pending(), "the news inside the interval was dropped");
    assert_eq!(b.load_remote_seq().observed_seq, manifest_seq(&a).await, "the ticker did not show it at once");
    assert_eq!(read(db.path(), "x.txt").as_deref(), Some("v2 ."), "fixture: taken inside the interval");
    let o = b.news_arm(&mut clock, at(6)).await.unwrap().expect("past the interval the news waited on");
    assert_eq!((o.consumed, read(db.path(), "x.txt").as_deref()), (1, Some("v3 ..")));
    assert!(!clock.pending());

    // The last sign of work was the news at 2 s (the arm takes no other):
    // past its window, not one request.
    let idle = at(2 + b.cfg.news_active_secs + 1);
    write(da.path(), "x.txt", "v4 ...");
    a.floor_tick().await.unwrap();
    let c0 = counts(&store);
    assert!(b.news_arm(&mut clock, idle).await.unwrap().is_none());
    assert_eq!(counts(&store).total(), c0.total(), "an idle workspace's news arm sent a request");
    assert_eq!(read(db.path(), "x.txt").as_deref(), Some("v3 .."), "fixture: idle took the news");
}

#[test]
fn the_news_clock_counts_work_and_waits_out_the_interval() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_for(dir.path());
    let (active, gap) = (Duration::from_secs(cfg.news_active_secs), Duration::from_secs(cfg.news_min_interval_secs));
    let t0 = Instant::now();
    let mut c = NewsClock::new(&cfg, t0);
    assert!(c.enabled() && c.active(t0) && c.active(t0 + active - Duration::from_secs(1)));
    assert!(!c.active(t0 + active), "active past the window with no work");
    c.touch(t0 + Duration::from_secs(100));
    assert!(c.active(t0 + active), "work did not restart the window");
    assert!(!c.due(t0), "due with no news");
    c.saw_news(t0);
    assert!(c.due(t0), "news with no barrier yet is not due");
    c.ran_early(t0);
    assert!(!c.due(t0 + gap - Duration::from_millis(1)), "due inside the interval");
    assert!(c.due(t0 + gap), "a failed barrier's news is never retried");
    c.taken();
    assert!(!c.due(t0 + gap), "news a barrier took is still due");

    let mut off = cfg_for(dir.path());
    off.news_poll_secs = 0;
    let c = NewsClock::new(&off, t0);
    assert!(!c.enabled() && !c.active(t0), "news_poll_secs = 0 is not off");
}
