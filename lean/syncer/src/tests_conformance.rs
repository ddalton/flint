//! Traces for checking the formal model against the code.
//!
//! Each scenario drives real syncers over the in-memory store with the
//! protocol event trace on, and logs what the trace cannot see — the
//! agent's writes and deletes, UI writes, sentinel touches — as `conf_*`
//! events into the same buffer, in order. `lean/formal/trace/` turns the
//! result into a behaviour `LeanSubtree.tla` must be able to produce
//! (`trace-check.sh`); an event the model cannot follow is a disagreement
//! between the model and the code, with its position in the trace.
//!
//! The scenarios assert only that they reached the state they were built
//! for. `FLINT_SYNC_CONFORMANCE_DIR=<dir>` writes `<dir>/<scenario>.ndjson`.
//!
//! Every write of the agent goes through [`Agent`], so no tree change is
//! missing from a trace: a hidden write would make the model reject an
//! upload it cannot explain, which reads as a model gap and is not one.

use std::sync::{Arc, Mutex};

use flint_store::memory::MemoryStore;
use sha2::{Digest, Sha256};

use super::control;
use super::lease;
use super::manifest;
use super::sentinel::Verb;
use super::tests::{
    backdate_baseline, clear_min_interval, gateway_current, hitl_remove, hitl_rename, hitl_write_at, hooked_syncer, read,
    syncer, touch_sentinel, write, Hooked, Hooks,
};
use super::trace::Sink;
use super::Syncer;

type Buf = Arc<Mutex<Vec<String>>>;

/// The in-memory store's etag for a whole PUT: the SHA-256 of the bytes.
fn etag_of(content: &str) -> String {
    format!("\"{:x}\"", Sha256::digest(content.as_bytes()))
}

struct Agent<'a> {
    name: &'static str,
    sc: &'a Syncer,
    root: &'a std::path::Path,
}

impl Agent<'_> {
    fn write(&self, rel: &str, content: &str) {
        write(self.root, rel, content);
        backdate_baseline(self.sc, rel);
        self.sc.trace("conf_agent_write", serde_json::json!({"writer": self.name, "path": rel, "etag": etag_of(content)}));
    }
    fn delete(&self, rel: &str) {
        std::fs::remove_file(self.root.join(rel)).unwrap();
        self.sc.trace("conf_agent_delete", serde_json::json!({"writer": self.name, "path": rel}));
    }
    fn touch(&self, nonce: &str) {
        touch_sentinel(self.root, control::PUBLISH, &format!(r#"{{"nonce":"{nonce}"}}"#));
        self.sc.trace("conf_touch", serde_json::json!({"writer": self.name, "nonce": nonce}));
    }
}

fn traced(sc: &mut Syncer, buf: &Buf) {
    sc.cfg.event_trace = Some(Sink::Memory(buf.clone()));
}

/// The trace starts here: every writer has checked out the seed. The
/// model's initial state is "every seeded path published once", so the
/// seed's entries and seq are the mapping's origin.
async fn start(store: &Arc<MemoryStore>, writers: &[(&'static str, &Syncer)]) {
    let m = manifest::load(store.as_ref(), &writers[0].1.cfg).await.unwrap().unwrap().manifest;
    let entries: serde_json::Map<String, serde_json::Value> =
        m.entries.iter().map(|(p, e)| (p.clone(), serde_json::Value::String(e.etag.clone()))).collect();
    // The seed's handles too: a UI save or delete leaves one uncited, and
    // the orphan sweep that takes it names it by key.
    let keys: serde_json::Map<String, serde_json::Value> =
        m.entries.iter().map(|(p, e)| (p.clone(), serde_json::Value::String(e.key.clone()))).collect();
    for (name, sc) in writers {
        sc.trace("conf_start", serde_json::json!({"writer": name, "seq": m.seq, "entries": entries, "keys": keys,
            "reader": sc.cfg.access.is_read()}));
    }
}

/// A UI write: the handle it landed at (a sweep that later names the handle
/// is mapped to the version by it) and the generation its commit installed.
/// Returns the handle.
async fn ui_write(store: &Arc<MemoryStore>, via: &Syncer, rel: &str, content: &str) -> String {
    ui_write_at(store, via, rel, content, super::now_unix()).await
}

async fn ui_write_at(store: &Arc<MemoryStore>, via: &Syncer, rel: &str, content: &str, at: u64) -> String {
    let (etag, key) = hitl_write_at(store, &via.cfg, rel, content, "reviewer", at).await.unwrap();
    let seq = committed_seq(store, via).await;
    via.trace("conf_hitl_write", serde_json::json!({"path": rel, "etag": etag, "key": key, "seq": seq}));
    key
}

/// A UI delete: one commit that stops citing the path (P2).
async fn ui_delete(store: &Arc<MemoryStore>, via: &Syncer, rel: &str) {
    hitl_remove(store, &via.cfg, rel, "reviewer").await;
    let seq = committed_seq(store, via).await;
    via.trace("conf_hitl_delete", serde_json::json!({"path": rel, "seq": seq}));
}

/// A UI rename: one commit that moves the citation. The destination names
/// the version the source cited, logged as `etag`.
async fn ui_rename(store: &Arc<MemoryStore>, via: &Syncer, from: &str, to: &str) {
    let etag = gateway_current(store, &via.cfg, from).await;
    hitl_rename(store, &via.cfg, from, to, "reviewer").await;
    let seq = committed_seq(store, via).await;
    via.trace("conf_hitl_rename", serde_json::json!({"from": from, "to": to, "etag": etag, "seq": seq}));
}

/// The generation the manifest is at: read right after a gateway commit,
/// with nothing else running, it is the one that commit installed.
async fn committed_seq(store: &Arc<MemoryStore>, via: &Syncer) -> u64 {
    manifest::load(store.as_ref(), &via.cfg).await.unwrap().unwrap().manifest.seq
}

/// Every traced event named `ev`, in order.
fn events(buf: &Buf, ev: &str) -> Vec<serde_json::Value> {
    buf.lock()
        .unwrap()
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["ev"] == ev)
        .collect()
}

/// What the removal pass decided, as `(path, action)`, in order.
fn removals(buf: &Buf) -> Vec<(String, String)> {
    events(buf, "removal")
        .iter()
        .map(|e| (e["path"].as_str().unwrap_or("").to_string(), e["action"].as_str().unwrap_or("").to_string()))
        .collect()
}

fn dump(name: &str, buf: &Buf) {
    let lines = buf.lock().unwrap().clone();
    let first = lines.iter().position(|l| l.contains("\"ev\":\"conf_start\"")).expect("no conf_start in the trace");
    if let Ok(dir) = std::env::var("FLINT_SYNC_CONFORMANCE_DIR") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(format!("{dir}/{name}.ndjson"), lines[first..].join("\n") + "\n").unwrap();
    }
}

async fn two_writers(
    seed: &[(&str, &str)],
) -> (Arc<MemoryStore>, Buf, Syncer, Syncer, tempfile::TempDir, tempfile::TempDir) {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let buf: Buf = Arc::new(Mutex::new(vec![]));
    let mut a = syncer(&store, dir_a.path()).await;
    let mut b = syncer(&store, dir_b.path()).await;
    traced(&mut a, &buf);
    traced(&mut b, &buf);
    a.checkout().await.unwrap();
    for (p, c) in seed {
        write(dir_a.path(), p, c);
    }
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    (store, buf, a, b, dir_a, dir_b)
}

/// `two_writers` with the second tree a READER (read access, `reader.rs`):
/// its floor tick is a pull, and it never writes to the bucket.
async fn writer_and_reader(
    seed: &[(&str, &str)],
) -> (Arc<MemoryStore>, Buf, Syncer, Syncer, tempfile::TempDir, tempfile::TempDir) {
    let store = Arc::new(MemoryStore::new());
    let (dir_a, dir_r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let buf: Buf = Arc::new(Mutex::new(vec![]));
    let mut a = syncer(&store, dir_a.path()).await;
    let mut r = syncer(&store, dir_r.path()).await;
    r.cfg.access = super::Access::Read;
    traced(&mut a, &buf);
    traced(&mut r, &buf);
    a.checkout().await.unwrap();
    for (p, c) in seed {
        write(dir_a.path(), p, c);
    }
    a.run_barrier().await.unwrap();
    r.checkout().await.unwrap();
    (store, buf, a, r, dir_a, dir_r)
}

/// `two_writers` over the hooked double, for a scenario that must run one
/// writer's barrier INSIDE another's (between its upload landing and its
/// claim). A hook fires only for the key it is armed on.
async fn two_writers_hooked(
    seed: &[(&str, &str)],
) -> (Arc<Hooked>, Arc<MemoryStore>, Buf, Syncer, Syncer, tempfile::TempDir, tempfile::TempDir) {
    let store = Arc::new(MemoryStore::new());
    let hs = Arc::new(Hooked(store.clone(), Hooks::default()));
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let buf: Buf = Arc::new(Mutex::new(vec![]));
    let mut a = hooked_syncer(&hs, dir_a.path());
    let mut b = hooked_syncer(&hs, dir_b.path());
    traced(&mut a, &buf);
    traced(&mut b, &buf);
    a.checkout().await.unwrap();
    for (p, c) in seed {
        write(dir_a.path(), p, c);
    }
    a.run_barrier().await.unwrap();
    b.checkout().await.unwrap();
    (hs, store, buf, a, b, dir_a, dir_b)
}

/// Run `f` on `other` from inside a store call of the barrier in flight:
/// the hook blocks that barrier until `f` returns. `other` comes back
/// through the returned slot; an empty slot means the hook never fired.
fn inside<F>(hs: &Hooked, key: &str, other: Syncer, f: F) -> Arc<Mutex<Option<Syncer>>>
where
    F: Fn(Syncer) -> std::pin::Pin<Box<dyn std::future::Future<Output = Syncer>>>
        + Send
        + Sync
        + 'static,
{
    let slot = Mutex::new(Some(other));
    let back: Arc<Mutex<Option<Syncer>>> = Arc::new(Mutex::new(None));
    let back_in = back.clone();
    hs.after_put(key, move || {
        let o = slot.lock().unwrap().take().expect("hook ran twice");
        let o = tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f(o)));
        *back_in.lock().unwrap() = Some(o);
    });
    back
}

/// `inside`, from the barrier's re-read of `key` (its HEAD in the commit
/// section, after its claim and before its CAS: review H2's window).
fn inside_after_head<F>(hs: &Hooked, key: &str, other: Syncer, f: F) -> Arc<Mutex<Option<Syncer>>>
where
    F: Fn(Syncer) -> std::pin::Pin<Box<dyn std::future::Future<Output = Syncer>>>
        + Send
        + Sync
        + 'static,
{
    let slot = Mutex::new(Some(other));
    let back: Arc<Mutex<Option<Syncer>>> = Arc::new(Mutex::new(None));
    let back_in = back.clone();
    hs.after_head(key, move || {
        let o = slot.lock().unwrap().take().expect("hook ran twice");
        let o = tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f(o)));
        *back_in.lock().unwrap() = Some(o);
    });
    back
}

/// A delete and an edit cross between two writers: the delete reaches the
/// other tree through its queue, the edit through the other's, and idle
/// barriers install nothing.
#[tokio::test]
async fn conformance_edit_and_delete_cross() {
    let (store, buf, mut a, mut b, dir_a, dir_b) = two_writers(&[("x.txt", "seed"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "A", sc: &a, root: dir_a.path() }.delete("x.txt");
    Agent { name: "B", sc: &b, root: dir_b.path() }.write("keep.txt", "B's edit");
    for _ in 0..4 {
        a.run_barrier().await.unwrap();
        b.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir_b.path(), "x.txt"), None, "fixture: A's delete never reached B");
    assert_eq!(read(dir_a.path(), "keep.txt").as_deref(), Some("B's edit"), "fixture: B's edit never reached A");
    dump("edit_and_delete_cross", &buf);
}

/// A UI write re-creates a path one writer deleted while the other
/// writer's queue still holds the deletion (the queued-tombstone finding).
#[tokio::test]
async fn conformance_ui_write_over_a_queued_delete() {
    let (store, buf, mut a, mut b, dir_a, dir_b) = two_writers(&[("x.txt", "seed"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "A", sc: &a, root: dir_a.path() }.delete("x.txt");
    a.run_barrier().await.unwrap();
    a.run_barrier().await.unwrap();
    b.run_barrier().await.unwrap();
    ui_write(&store, &a, "x.txt", "from the UI").await;
    for _ in 0..3 {
        b.run_barrier().await.unwrap();
        a.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("from the UI"), "fixture: the UI write is not in B's tree");
    assert_eq!(read(dir_a.path(), "x.txt").as_deref(), Some("from the UI"), "fixture: the UI write is not in A's tree");
    dump("ui_write_over_a_queued_delete", &buf);
}

/// The user's rule (2026-09-24), where it bites under P2: the UI deletes a
/// file the agent is editing. The delete commits at once; the agent's edit
/// publishes over it at the next barrier, with a record naming what the
/// delete removed. (Before P2 the UI write in the scenario above took this
/// route through a writer; now the UI's own commit re-creates the path.)
#[tokio::test]
async fn conformance_agent_edit_over_a_ui_delete() {
    let (store, buf, mut a, mut b, dir_a, dir_b) = two_writers(&[("x.txt", "seed"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("x.txt", "A's edit");
    ui_delete(&store, &a, "x.txt").await;
    for _ in 0..2 {
        a.run_barrier().await.unwrap();
        b.run_barrier().await.unwrap();
    }
    assert!(
        events(&buf, "surface").iter().any(|e| e["path"] == "x.txt" && e["deleted"] == true),
        "fixture: the edit over the delete left no record"
    );
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("A's edit"), "fixture: the edit did not reach B");
    dump("agent_edit_over_a_ui_delete", &buf);
}

/// Both writers edit one file: the second upload finds the first writer's
/// bytes at the key, and whatever the conflict rules make of it, the trees
/// converge.
#[tokio::test]
async fn conformance_both_edit_one_file() {
    let (store, buf, mut a, mut b, dir_a, dir_b) = two_writers(&[("shared.txt", "seed"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("shared.txt", "A's v2");
    a.run_barrier().await.unwrap();
    Agent { name: "B", sc: &b, root: dir_b.path() }.write("shared.txt", "B's v2");
    for _ in 0..4 {
        b.run_barrier().await.unwrap();
        a.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir_a.path(), "shared.txt"), read(dir_b.path(), "shared.txt"), "fixture: the trees did not converge");
    dump("both_edit_one_file", &buf);
}

/// P1-lite's one gap: a UI save lands after B's consume and before B's
/// merge. B's merge sees it and the tree does not have it, so the barrier
/// must leave it OWED; B's next consume takes it although the pointer is
/// where B's own install left it (the model's `MergeMarksOwed`,
/// `Inv_ShortcutSound`). The save is logged from inside the barrier, so the
/// trace has it where it happened.
#[tokio::test]
async fn conformance_ui_save_lands_inside_a_barrier() {
    let (store, buf, mut a, mut b, _dir_a, dir_b) = two_writers(&[("doc.txt", "seed"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "B", sc: &b, root: dir_b.path() }.write("keep.txt", "B's edit");
    let (store2, cfg, buf2) = (store.clone(), b.cfg.clone(), buf.clone());
    super::barrier::CONSUME_WINDOW_HOOK.with(|h| {
        *h.borrow_mut() = Some((
            String::new(),
            "before-scan",
            Box::new(move || {
                let at = super::now_unix();
                let (etag, key) =
                    futures::executor::block_on(hitl_write_at(&store2, &cfg, "doc.txt", "saved mid-barrier", "reviewer", at))
                        .unwrap();
                let seq = futures::executor::block_on(manifest::load(store2.as_ref(), &cfg)).unwrap().unwrap().manifest.seq;
                buf2.lock().unwrap().push(
                    serde_json::json!({"ev": "conf_hitl_write", "path": "doc.txt", "etag": etag, "key": key, "seq": seq}).to_string(),
                );
            }),
        ))
    });
    b.run_barrier().await.unwrap();
    assert!(super::barrier::CONSUME_WINDOW_HOOK.with(|h| h.borrow().is_none()), "fixture: the window never ran");
    assert_eq!(read(dir_b.path(), "doc.txt").as_deref(), Some("seed"), "fixture: the save already reached B's tree");
    b.run_barrier().await.unwrap();
    assert_eq!(read(dir_b.path(), "doc.txt").as_deref(), Some("saved mid-barrier"), "the save B's merge saw was never taken");
    a.run_barrier().await.unwrap();
    dump("ui_save_lands_inside_a_barrier", &buf);
}

/// A UI write is consumed by both writers and cited.
#[tokio::test]
async fn conformance_ui_write_cited() {
    let (store, buf, mut a, mut b, _dir_a, dir_b) = two_writers(&[("doc.txt", "seed"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    ui_write(&store, &a, "doc.txt", "reviewed").await;
    for _ in 0..3 {
        a.run_barrier().await.unwrap();
        b.run_barrier().await.unwrap();
    }
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries["doc.txt"].etag, etag_of("reviewed"), "fixture: the UI write was never cited");
    assert_eq!(read(dir_b.path(), "doc.txt").as_deref(), Some("reviewed"));
    dump("ui_write_cited", &buf);
}

/// The agent's delete meets a peer's edit it never saw, through a declared
/// publish. M3 (P1-lite): the delete lands over the edit at once, the edit
/// is preserved under a record, and the ack is `ok`. (Before M3 the delete
/// was outranked and the ack partial; that scenario was
/// `outranked_delete_publish`.)
#[tokio::test]
async fn conformance_delete_over_a_peers_edit() {
    let (store, buf, mut a, mut b, dir_a, dir_b) = two_writers(&[("shared.txt", "v1"), ("keep.txt", "keep")]).await;
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "B", sc: &b, root: dir_b.path() }.write("shared.txt", "B's v2");
    b.run_barrier().await.unwrap();
    let agent_a = Agent { name: "A", sc: &a, root: dir_a.path() };
    agent_a.delete("shared.txt");
    agent_a.touch("n-1");
    a.poll_sentinels().unwrap();
    clear_min_interval(&a);
    let ack = a.honor_pending(Verb::Publish, false).await.unwrap().unwrap();
    assert_eq!(ack.status, "ok", "fixture: {ack:?}");
    // The merge's outcome for the delete is DATA in the trace: over theirs
    // (B's v2 moved off A's base), and outranked nowhere.
    let merges: Vec<serde_json::Value> = buf
        .lock()
        .unwrap()
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["ev"] == "merge" && e["deletes"] == 1)
        .collect();
    assert_eq!(merges.len(), 1, "A's merges: {merges:?}");
    assert_eq!(merges[0]["over_theirs"], serde_json::json!(["shared.txt"]));
    assert_eq!(merges[0]["outranked"], serde_json::json!([]));
    b.run_barrier().await.unwrap();
    assert_eq!(read(dir_b.path(), "shared.txt"), None, "fixture: A's delete never reached B");
    dump("delete_over_a_peers_edit", &buf);
}

/// The publish request's `partial` (rows 15-17, H10/L-135's shape): the
/// floor's barrier carries A's declaration and publishes it; the UI deletes
/// the declared file before the honour, whose barrier takes the delete. The
/// carrier names the upload the document no longer cites, and the ack says
/// so (`partial`, dropped work.txt): AK1 on the run the code performed.
#[tokio::test]
async fn conformance_a_ui_delete_of_a_carried_file_before_the_honour() {
    let (store, buf, mut a, b, dir_a, _dir_b) = two_writers(&[("keep.txt", "keep")]).await;
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("work.txt", "the declared bytes");
    Agent { name: "A", sc: &a, root: dir_a.path() }.touch("n-1");
    a.poll_sentinels().unwrap();
    a.cadence_barrier().await.unwrap();
    assert!(gateway_current(&store, &a.cfg, "work.txt").await.is_some(), "fixture: the carry did not publish");
    ui_delete(&store, &a, "work.txt").await;
    clear_min_interval(&a);
    let ack = a.honor_pending(Verb::Publish, true).await.unwrap().unwrap();
    assert_eq!(ack.status, "partial", "fixture: {ack:?}");
    assert_eq!(ack.report.dropped, vec!["work.txt".to_string()], "fixture: {ack:?}");
    dump("a_ui_delete_of_a_carried_file_before_the_honour", &buf);
}

/// A second touch FOLDS into the record the floor's barrier carried (row 15,
/// D7: the record keeps its id across folds). The honour answers both
/// nonces in one `ok` ack: AK3 and the fold on the run the code performed.
#[tokio::test]
async fn conformance_a_second_touch_folds_into_the_carried_record() {
    let (store, buf, mut a, b, dir_a, _dir_b) = two_writers(&[("keep.txt", "keep")]).await;
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("one.txt", "first");
    Agent { name: "A", sc: &a, root: dir_a.path() }.touch("n-1");
    a.poll_sentinels().unwrap();
    a.cadence_barrier().await.unwrap();
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("two.txt", "second");
    Agent { name: "A", sc: &a, root: dir_a.path() }.touch("n-2");
    a.poll_sentinels().unwrap();
    clear_min_interval(&a);
    let ack = a.honor_pending(Verb::Publish, false).await.unwrap().unwrap();
    assert_eq!(ack.status, "ok", "fixture: {ack:?}");
    assert_eq!(ack.nonces, vec!["n-1".to_string(), "n-2".to_string()], "fixture: {ack:?}");
    dump("a_second_touch_folds_into_the_carried_record", &buf);
}

/// A restart: a new incarnation of the writer over the same tree and state,
/// through the binary's startup (`run_loop`): the release of a cell a
/// previous container left held (it traces `start`, the model's Restart),
/// the checkout (a no-op on a live tree), and the settle of a pending
/// request. The store is the one the writer had, hooked or not.
async fn restart(store: Arc<dyn flint_store::ObjectStore>, buf: &Buf, root: &std::path::Path) -> Syncer {
    let cfg = super::tests::cfg_for(root);
    let state = super::state::SyncerState::open(cfg.state_dir()).unwrap();
    let mut sc = Syncer { store, cfg, state, lease: None, cell_written_at: None, noted_not_regular: Default::default() };
    traced(&mut sc, buf);
    lease::release_stale_own(&mut sc).await.unwrap();
    sc.checkout().await.unwrap();
    sc.settle_pending_at_startup().await.unwrap();
    sc
}

/// A crash: the barrier stops at the first DELETE of `key` (step 6, after
/// its CAS, before its collector and step 7), as a killed process would —
/// no event and no store effect after that point.
async fn crash_at_delete(hs: &Arc<Hooked>, key: &str, run: impl std::future::Future<Output = ()>) {
    use futures::FutureExt;
    hs.before_delete(key, || panic!("the crash"));
    let r = std::panic::AssertUnwindSafe(run).catch_unwind().await;
    let msg = r.err().and_then(|e| e.downcast_ref::<&str>().map(|s| s.to_string()));
    assert_eq!(msg.as_deref(), Some("the crash"), "fixture: the barrier never reached the delete");
}

/// The restart row (NR1, row 16): the agent's delete, unpublished when the
/// container restarts, is not brought back by the restart — a live tree is
/// never re-materialised — and the next barriers publish it.
#[tokio::test]
async fn conformance_an_unpublished_delete_survives_a_restart() {
    let (store, buf, a, mut b, dir_a, dir_b) = two_writers(&[("gone.txt", "doomed"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "A", sc: &a, root: dir_a.path() }.delete("gone.txt");
    drop(a);
    let mut a = restart(store.clone(), &buf, dir_a.path()).await;
    assert_eq!(read(dir_a.path(), "gone.txt"), None, "the restart brought the agent's delete back");
    a.run_barrier().await.unwrap();
    a.run_barrier().await.unwrap();
    assert!(gateway_current(&store, &a.cfg, "gone.txt").await.is_none(), "fixture: the delete never published");
    b.run_barrier().await.unwrap();
    assert_eq!(read(dir_b.path(), "gone.txt"), None, "fixture: the delete never reached B");
    dump("an_unpublished_delete_survives_a_restart", &buf);
}

/// A crash between the CAS and step 7 (the restart rows' window, H10's and
/// FINDINGS Table 3's): the install landed, step 7 never ran, so the
/// baseline still holds v1. The restarted writer's next barrier converges
/// on its own upload (ContentConverges) and publishes nothing again; B
/// takes the edit.
#[tokio::test]
async fn conformance_a_restart_between_the_cas_and_step_7() {
    let (hs, store, buf, mut a, mut b, dir_a, dir_b) = two_writers_hooked(&[("x.txt", "v1"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    let v1 = gateway_current(&store, &a.cfg, "x.txt").await.unwrap();
    let v1_key = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries["x.txt"].key.clone();
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("x.txt", "A's v2");
    backdate_baseline(&a, "x.txt");
    crash_at_delete(&hs, &v1_key, async {
        let _ = a.run_barrier().await;
    })
    .await;
    assert_ne!(gateway_current(&store, &a.cfg, "x.txt").await, Some(v1), "fixture: the install did not land");
    drop(a);
    let mut a = restart(hs.clone(), &buf, dir_a.path()).await;
    let seq = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq;
    a.run_barrier().await.unwrap();
    assert_eq!(
        manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.seq,
        seq,
        "the restarted writer re-published its own install"
    );
    b.run_barrier().await.unwrap();
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("A's v2"), "fixture: the edit never reached B");
    dump("a_restart_between_the_cas_and_step_7", &buf);
}

/// A crash between the CAS and step 7 of the HONOUR's barrier, then a UI
/// delete of the file it published. Step 7 never ran, so the restarted
/// tree still reads the agent's v2 as unpublished, and the settle's barrier
/// publishes it again over the delete it never integrated — the delete
/// override (mine wins, the deleted version recorded) — and answers `ok`
/// from the document that cites it. (The model's Restart keeps the stale
/// baseline too; FINDINGS Table 3's rows live in this window.)
#[tokio::test]
async fn conformance_a_restart_before_step_7_republishes_over_a_ui_delete() {
    let (hs, store, buf, mut a, b, dir_a, _dir_b) = two_writers_hooked(&[("x.txt", "v1"), ("keep.txt", "keep")]).await;
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
    start(&store, &[("A", &a), ("B", &b)]).await;
    let v1_key = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries["x.txt"].key.clone();
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("x.txt", "A's declared v2");
    backdate_baseline(&a, "x.txt");
    Agent { name: "A", sc: &a, root: dir_a.path() }.touch("n-1");
    a.poll_sentinels().unwrap();
    clear_min_interval(&a);
    crash_at_delete(&hs, &v1_key, async {
        let _ = a.honor_pending(Verb::Publish, false).await;
    })
    .await;
    assert!(a.read_ack(Verb::Publish).is_none(), "fixture: an ack was written before the crash");
    ui_delete(&store, &a, "x.txt").await;
    drop(a);
    let a = restart(hs.clone(), &buf, dir_a.path()).await;
    let ack = a.read_ack(Verb::Publish).expect("the restart answered nothing");
    assert_eq!(ack.status, "ok", "fixture: {ack:?}");
    assert!(
        ack.report.conflicts.iter().any(|c| c.path == "x.txt" && c.kind.starts_with("commit-recreated-deleted")),
        "fixture: the delete override left no record: {ack:?}"
    );
    assert!(gateway_current(&store, &a.cfg, "x.txt").await.is_some(), "fixture: x.txt is not cited");
    dump("a_restart_before_step_7_republishes_over_a_ui_delete", &buf);
}

/// H10's restart route (rows 15-17): the honour's barrier installs A's
/// declared edit and runs step 7, and the container dies before the ack;
/// the UI deletes the declared file; the restarted writer's settle re-runs
/// the barrier, which takes the delete, and answers from ITS document,
/// which no longer cites the file: `partial` (AK1 across a restart, on the
/// run the code performed).
#[tokio::test]
async fn conformance_a_restart_before_the_ack_answers_a_ui_delete_partial() {
    let (store, buf, mut a, b, dir_a, _dir_b) = two_writers(&[("x.txt", "v1"), ("keep.txt", "keep")]).await;
    let posture = a.sentinel_preflight().unwrap();
    a.write_capabilities(&posture).unwrap();
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("x.txt", "A's declared v2");
    backdate_baseline(&a, "x.txt");
    Agent { name: "A", sc: &a, root: dir_a.path() }.touch("n-1");
    a.poll_sentinels().unwrap();
    // The honour's barrier, through step 7; the ack is never written.
    a.declared_barrier_as("sentinel").await.unwrap();
    assert!(a.read_ack(Verb::Publish).is_none(), "fixture: an ack was written");
    ui_delete(&store, &a, "x.txt").await;
    drop(a);
    let a = restart(store.clone(), &buf, dir_a.path()).await;
    let ack = a.read_ack(Verb::Publish).expect("the restart answered nothing");
    assert_eq!(ack.status, "partial", "fixture: {ack:?}");
    assert_eq!(ack.report.dropped, vec!["x.txt".to_string()], "fixture: {ack:?}");
    dump("a_restart_before_the_ack_answers_a_ui_delete_partial", &buf);
}

/// A's holder id as the trace names it (its `conf_start` line).
fn holder_of(buf: &Buf, writer: &str) -> String {
    buf.lock()
        .unwrap()
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| e["ev"] == "conf_start" && e["writer"] == writer)
        .and_then(|e| e["holder"].as_str().map(String::from))
        .expect("fixture: no conf_start for the writer")
}

/// The narrow and the widen (scoped-read design §4; NarrowUnlinkFirst,
/// ScopeIgnored, WidenOverwrites): A narrows to `inputs`, which unlinks
/// `outputs/` from its tree and publishes not one delete; B's later edit
/// there is not fetched into A's tree (the scope is respected); the widen
/// back takes B's version.
#[tokio::test]
async fn conformance_a_narrow_then_a_widen() {
    let (store, buf, mut a, mut b, dir_a, dir_b) =
        two_writers(&[("inputs/a.txt", "in"), ("outputs/big.bin", "out v1"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    a.rescope(Some(vec!["inputs".into()])).await.unwrap();
    assert_eq!(read(dir_a.path(), "outputs/big.bin"), None, "fixture: the narrow did not unlink");
    a.run_barrier().await.unwrap();
    a.run_barrier().await.unwrap();
    assert!(gateway_current(&store, &a.cfg, "outputs/big.bin").await.is_some(), "the narrow published a delete");
    Agent { name: "B", sc: &b, root: dir_b.path() }.write("outputs/big.bin", "out v2, B's");
    b.run_barrier().await.unwrap();
    a.run_barrier().await.unwrap();
    assert_eq!(read(dir_a.path(), "outputs/big.bin"), None, "a narrowed tree fetched an out-of-scope edit");
    a.rescope(None).await.unwrap();
    assert_eq!(read(dir_a.path(), "outputs/big.bin").as_deref(), Some("out v2, B's"), "the widen did not take B's");
    dump("a_narrow_then_a_widen", &buf);
}

/// A crash between the uncite and the unlink of a narrow (the design's
/// third mutation check, NarrowUnlinkFirst), then a restart: the intent is
/// on disk, and the next barrier replays it before its scan — the leftover
/// is unlinked, not re-uploaded, and nothing is deleted from the bucket.
#[tokio::test]
async fn conformance_a_narrow_crashed_before_its_unlink_is_replayed() {
    use futures::FutureExt;
    let (store, buf, mut a, b, dir_a, _dir_b) =
        two_writers(&[("inputs/a.txt", "in"), ("outputs/big.bin", "out"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    super::barrier::CONSUME_WINDOW_HOOK.with(|h| {
        *h.borrow_mut() = Some(("outputs/big.bin".into(), "rescope-before-unlink", Box::new(|| panic!("the crash"))))
    });
    let r = std::panic::AssertUnwindSafe(a.rescope(Some(vec!["inputs".into()]))).catch_unwind().await;
    assert!(r.is_err(), "fixture: the narrow did not stop in its window");
    assert!(read(dir_a.path(), "outputs/big.bin").is_some(), "fixture: the leftover is not on disk");
    drop(a);
    let mut a = restart(store.clone(), &buf, dir_a.path()).await;
    let r = a.run_barrier().await.unwrap();
    assert!(r.rescope_replayed, "fixture: the barrier did not replay the narrow: {r:?}");
    assert!(r.uploaded.is_empty() && r.deleted.is_empty(), "the replay published the leftover: {r:?}");
    assert_eq!(read(dir_a.path(), "outputs/big.bin"), None, "the replay did not unlink the leftover");
    a.run_barrier().await.unwrap();
    assert!(gateway_current(&store, &a.cfg, "outputs/big.bin").await.is_some(), "the narrow deleted from the bucket");
    dump("a_narrow_crashed_before_its_unlink_is_replayed", &buf);
}

/// The agent writes a dropped path BETWEEN the uncite and the unlink
/// (UnlinkBlind, L-131): the unlink must not take bytes it did not uncite.
/// Kept, uncited, recorded; the next barrier publishes it as the agent's.
#[tokio::test]
async fn conformance_a_narrow_keeps_what_the_agent_wrote_after_the_uncite() {
    let (store, buf, mut a, b, dir_a, _dir_b) =
        two_writers(&[("inputs/a.txt", "in"), ("outputs/big.bin", "out"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    let (root, holder, tb) = (dir_a.path().to_path_buf(), holder_of(&buf, "A"), buf.clone());
    super::barrier::CONSUME_WINDOW_HOOK.with(|h| {
        *h.borrow_mut() = Some((
            "outputs/big.bin".into(),
            "rescope-before-unlink",
            Box::new(move || {
                let content = "the agent's edit after the uncite";
                write(&root, "outputs/big.bin", content);
                let line = super::trace::line(Some(&holder), "conf_agent_write",
                    serde_json::json!({"writer": "A", "path": "outputs/big.bin", "etag": etag_of(content)}));
                tb.lock().unwrap().push(line);
            }),
        ))
    });
    let r = a.rescope(Some(vec!["inputs".into()])).await.unwrap();
    assert!(r.kept_dirty.contains(&"outputs/big.bin".to_string()), "fixture: {r:?}");
    assert_eq!(read(dir_a.path(), "outputs/big.bin").as_deref(), Some("the agent's edit after the uncite"));
    let r1 = a.run_barrier().await.unwrap();
    assert!(r1.uploaded.iter().any(|p| p == "outputs/big.bin"), "fixture: the agent's add was not published: {r1:?}");
    dump("a_narrow_keeps_what_the_agent_wrote_after_the_uncite", &buf);
}

/// Deposal before the CAS (row 17; review H2's position, InstallUnfenced,
/// DP1): A's barrier has claimed the cell and is re-reading the upload it
/// will cite when B's barrier, its quiet polls run out, deposes A (the
/// takeover rotates the manifest) and commits its own change. A's next read
/// of the cell fences it: no install onto its successor's document. A's
/// upload stands; its next barrier publishes the edit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conformance_a_holder_deposed_before_its_cas_is_fenced() {
    let (hs, store, buf, mut a, b, dir_a, dir_b) = two_writers_hooked(&[("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("a.txt", "A's edit");
    let root_b = dir_b.path().to_path_buf();
    let b_back = inside_after_head(&hs, &a.cfg.file_key("a.txt"), b, move |mut b| {
        let root_b = root_b.clone();
        Box::pin(async move {
            Agent { name: "B", sc: &b, root: &root_b }.write("b.txt", "B's commit");
            b.run_barrier().await.unwrap();
            b
        })
    });
    let r = a.run_barrier().await;
    let mut b = b_back.lock().unwrap().take().expect("fixture: the hook never ran");
    assert!(matches!(r, Err(super::LeanError::Fenced(_))), "a deposed holder's barrier must end fenced: {r:?}");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(m.entries.contains_key("b.txt"), "fixture: B's commit did not land");
    assert!(!m.entries.contains_key("a.txt"), "A, deposed, installed onto its successor's document");
    a.run_barrier().await.unwrap();
    assert!(gateway_current(&store, &a.cfg, "a.txt").await.is_some(), "A's next barrier did not publish its edit");
    b.run_barrier().await.unwrap();
    assert_eq!(read(dir_b.path(), "a.txt").as_deref(), Some("A's edit"), "fixture: A's edit never reached B");
    dump("a_holder_deposed_before_its_cas_is_fenced", &buf);
}

/// The sync verb between barriers (`sync.rs`, Sync): B edits one file,
/// deletes another, and edits the file A's agent is editing. A's sync takes
/// the edit and the delete and leaves the agent's file, recorded as skipped
/// (the consume's cheap path); A's next barrier publishes it over B's, with
/// B's version recorded (R7).
#[tokio::test]
async fn conformance_a_writer_syncs_between_barriers() {
    let (store, buf, mut a, mut b, dir_a, dir_b) =
        two_writers(&[("x.txt", "x v1"), ("y.txt", "y"), ("z.txt", "z v1")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    let bw = Agent { name: "B", sc: &b, root: dir_b.path() };
    bw.write("x.txt", "B's x");
    bw.write("z.txt", "B's z");
    bw.delete("y.txt");
    // Declared: a cadence barrier defers a first absence.
    b.declared_barrier().await.unwrap();
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("z.txt", "A's z");
    let r = a.sync().await.unwrap();
    assert_eq!(r.applied, vec!["x.txt".to_string()], "fixture: {r:?}");
    assert_eq!(r.deleted, vec!["y.txt".to_string()], "fixture: {r:?}");
    assert_eq!(r.conflicts, vec!["z.txt".to_string()], "the sync took the agent's file: {r:?}");
    assert_eq!(read(dir_a.path(), "z.txt").as_deref(), Some("A's z"));
    a.run_barrier().await.unwrap();
    assert_eq!(gateway_current(&store, &a.cfg, "z.txt").await, Some(etag_of("A's z")), "fixture: A's z not published");
    dump("a_writer_syncs_between_barriers", &buf);
}

/// A reader follows a writer (`reader.rs`, RPullRead/RPullSync): each tick
/// with news pulls, a tick without skips (ReaderSkips, Inv_ReaderSound). Its
/// agent's publish request is answered `refused-read-only` and publishes
/// nothing (HonorReader).
#[tokio::test]
async fn conformance_a_reader_follows_a_writer() {
    let (store, buf, mut a, mut r, dir_a, dir_r) = writer_and_reader(&[("x.txt", "x v1"), ("y.txt", "y")]).await;
    start(&store, &[("A", &a), ("R", &r)]).await;
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("x.txt", "A's x");
    a.run_barrier().await.unwrap();
    r.floor_tick().await.unwrap();
    assert_eq!(read(dir_r.path(), "x.txt").as_deref(), Some("A's x"), "fixture: the reader did not pull");
    let t = r.floor_tick().await.unwrap();
    assert!(t.no_change, "a reader with nothing new synced again: {t:?}");
    Agent { name: "A", sc: &a, root: dir_a.path() }.delete("y.txt");
    a.declared_barrier().await.unwrap();
    Agent { name: "R", sc: &r, root: dir_r.path() }.touch("r1");
    r.poll_sentinels().unwrap();
    let t = r.floor_tick().await.unwrap();
    assert_eq!(t.acks.first().map(|a| a.status.as_str()), Some("refused-read-only"), "fixture: {t:?}");
    assert_eq!(read(dir_r.path(), "y.txt"), None, "the reader did not take the delete");
    dump("a_reader_follows_a_writer", &buf);
}

/// L-129 (review 2026-09-29, S4; StillOwed): a reader's sync skips a path
/// its agent made, which a writer then published; the agent removes its
/// file, and the reader's next tick, with the pointer unmoved, still pulls:
/// the path is owed again.
#[tokio::test]
async fn conformance_a_reader_resyncs_a_path_it_skipped_once_clean() {
    let (store, buf, mut a, mut r, dir_a, dir_r) = writer_and_reader(&[("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("R", &r)]).await;
    Agent { name: "R", sc: &r, root: dir_r.path() }.write("new.txt", "R's scratch");
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("new.txt", "A's new");
    a.run_barrier().await.unwrap();
    r.floor_tick().await.unwrap();
    assert_eq!(read(dir_r.path(), "new.txt").as_deref(), Some("R's scratch"), "the reader's sync took its agent's file");
    Agent { name: "R", sc: &r, root: dir_r.path() }.delete("new.txt");
    let t = r.floor_tick().await.unwrap();
    assert!(!t.no_change, "the reader skipped a path still owed: {t:?}");
    assert_eq!(read(dir_r.path(), "new.txt").as_deref(), Some("A's new"), "the reader did not take the path once clean");
    let t = r.floor_tick().await.unwrap();
    assert!(t.no_change, "fixture: {t:?}");
    dump("a_reader_resyncs_a_path_it_skipped_once_clean", &buf);
}

/// A directory of the tree made read-only: a write into it fails, as a full
/// disk would. Returns the guard that makes it writable again.
struct ReadOnly(std::path::PathBuf);

impl ReadOnly {
    fn dir(root: &std::path::Path, rel: &str) -> ReadOnly {
        use std::os::unix::fs::PermissionsExt;
        let d = root.join(rel);
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o555)).unwrap();
        ReadOnly(d)
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// A consume whose write fails (MaxFetchFails, ConsumeKeepsLeft, L-128):
/// the path stays as it was and owed, nothing is recorded as derived, and
/// the next barrier derives in full and takes it.
#[tokio::test]
async fn conformance_a_consume_whose_write_fails_takes_the_path_at_the_next() {
    let (store, buf, mut a, mut b, dir_a, dir_b) =
        two_writers(&[("d/x.txt", "x v1"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "B", sc: &b, root: dir_b.path() }.write("d/x.txt", "B's x");
    b.run_barrier().await.unwrap();
    {
        let _ro = ReadOnly::dir(dir_a.path(), "d");
        a.run_barrier().await.unwrap();
        assert_eq!(read(dir_a.path(), "d/x.txt").as_deref(), Some("x v1"), "fixture: the write did not fail");
    }
    a.run_barrier().await.unwrap();
    assert_eq!(read(dir_a.path(), "d/x.txt").as_deref(), Some("B's x"), "the next barrier did not take the path left owed");
    dump("a_consume_whose_write_fails_takes_the_path_at_the_next", &buf);
}

/// A sync whose write fails (SyncKeepsLeft, L-128): it takes the rest,
/// records nothing as derived, and the next sync takes the path.
#[tokio::test]
async fn conformance_a_sync_whose_write_fails_leaves_the_path_owed() {
    let (store, buf, mut a, mut b, dir_a, dir_b) =
        two_writers(&[("d/x.txt", "x v1"), ("e.txt", "e v1")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "B", sc: &b, root: dir_b.path() }.write("d/x.txt", "B's x");
    Agent { name: "B", sc: &b, root: dir_b.path() }.write("e.txt", "B's e");
    b.run_barrier().await.unwrap();
    {
        let _ro = ReadOnly::dir(dir_a.path(), "d");
        let r = a.sync().await.unwrap();
        assert_eq!(r.applied, vec!["e.txt".to_string()], "fixture: {r:?}");
    }
    let r = a.sync().await.unwrap();
    assert_eq!(r.applied, vec!["d/x.txt".to_string()], "the next sync did not take the path left owed: {r:?}");
    dump("a_sync_whose_write_fails_leaves_the_path_owed", &buf);
}

/// A path the agent deleted unpublished that a peer's commit deleted too:
/// the sync lets go of it (the baseline drops the entry), as the consume's
/// convergence does (ContentConverges; the model's `Sync` converges since
/// 2026-10-17 -- before, this trace was the check's one known mismatch).
#[tokio::test]
async fn conformance_a_sync_lets_go_of_a_path_both_deleted() {
    let (store, buf, mut a, mut b, dir_a, dir_b) =
        two_writers(&[("x.txt", "x v1"), ("y.txt", "y")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "B", sc: &b, root: dir_b.path() }.write("x.txt", "B's x");
    Agent { name: "B", sc: &b, root: dir_b.path() }.delete("y.txt");
    b.declared_barrier().await.unwrap();
    Agent { name: "A", sc: &a, root: dir_a.path() }.delete("y.txt");
    let r = a.sync().await.unwrap();
    assert_eq!(r.applied, vec!["x.txt".to_string()], "fixture: {r:?}");
    let base = a.state.load_baseline().unwrap();
    assert!(!base.entries.contains_key("y.txt"), "fixture: the sync kept the entry");
    dump("a_sync_lets_go_of_a_path_both_deleted", &buf);
}

/// A UI delete of a clean file: the first writer's barrier performs it (the
/// tree's unlink and the document's delete in one barrier) and the other
/// writer's tree follows.
#[tokio::test]
async fn conformance_ui_delete_applied() {
    let (store, buf, mut a, mut b, dir_a, dir_b) = two_writers(&[("x.txt", "seed"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    ui_delete(&store, &a, "x.txt").await;
    for _ in 0..3 {
        a.run_barrier().await.unwrap();
        b.run_barrier().await.unwrap();
    }
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("x.txt"), "fixture: the delete was never published");
    assert_eq!((read(dir_a.path(), "x.txt"), read(dir_b.path(), "x.txt")), (None, None), "fixture: a tree kept x");
    // P2: the delete was COMMITTED by the gateway; each tree took it as a
    // queued deletion, never through a removal pass.
    assert!(removals(&buf).is_empty(), "a removal pass ran: {:?}", removals(&buf));
    dump("ui_delete_applied", &buf);
}


/// A UI rename: one writer's barrier moves the file (adopts the
/// destination, removes the source, cites the one handle at its new name),
/// and the other writer follows.
#[tokio::test]
async fn conformance_ui_rename() {
    let (store, buf, mut a, mut b, dir_a, dir_b) = two_writers(&[("x.txt", "seed"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    let seed_key = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries["x.txt"].key.clone();
    ui_rename(&store, &a, "x.txt", "y.txt").await;
    for _ in 0..3 {
        a.run_barrier().await.unwrap();
        b.run_barrier().await.unwrap();
    }
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("x.txt"), "fixture: the source kept its citation");
    assert_eq!(m.entries["y.txt"].key, seed_key, "fixture: the destination does not cite the moved handle");
    for d in [dir_a.path(), dir_b.path()] {
        assert_eq!((read(d, "x.txt"), read(d, "y.txt").as_deref()), (None, Some("seed")), "fixture: a tree did not move");
    }
    // P2: the rename was ONE committed CAS; the trees followed through
    // their queues.
    assert!(removals(&buf).is_empty(), "a removal pass ran: {:?}", removals(&buf));
    dump("ui_rename", &buf);
}

/// Two UI saves in a row: each commit retires what it replaced and LOGS it
/// (M1), and the next writer's commit section reaps the logs past the
/// retire age (0 here) and names what it took. (Before the retire logs the
/// orphan sweep found both by their write age.)
#[tokio::test]
async fn conformance_superseded_ui_write_swept() {
    let (store, buf, mut a, mut b, dir_a, _dir_b) = two_writers(&[("doc.txt", "seed"), ("keep.txt", "keep")]).await;
    a.cfg.untracked_grace_secs = 0;
    a.cfg.untracked_sweep_secs = 3600;
    a.state.save_orphan_sweep_at(super::now_unix() - 3601).unwrap();
    start(&store, &[("A", &a), ("B", &b)]).await;
    let start_doc = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    let seed = start_doc.entries["doc.txt"].key.clone();
    let keep_seed = start_doc.entries["keep.txt"].key.clone();
    let t = super::now_unix();
    let first = ui_write_at(&store, &a, "doc.txt", "first", t).await;
    let later = ui_write_at(&store, &a, "doc.txt", "later", t + 5).await;
    // The sweep runs inside a commit section, so A has something to publish.
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("keep.txt", "A's edit");
    for _ in 0..2 {
        a.run_barrier().await.unwrap();
        b.run_barrier().await.unwrap();
    }
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries["doc.txt"].key, later, "fixture: the later write was not cited");
    assert_eq!(read(dir_a.path(), "doc.txt").as_deref(), Some("later"));
    let swept: Vec<serde_json::Value> =
        events(&buf, "sweep").into_iter().filter(|e| e["what"] == "retired").collect();
    assert_eq!(swept.len(), 1, "fixture: the retire reap did not run once: {swept:?}");
    // P2: each save retired what it replaced and the gateway deletes
    // nothing, so the reap takes both — the seed and the first save — and
    // what A's own commit retired (keep.txt's seed).
    let mut took: Vec<String> = serde_json::from_value(swept[0]["keys"].clone()).unwrap();
    took.sort();
    let mut want = vec![seed, first, keep_seed];
    want.sort();
    assert_eq!(took, want, "the reap names what it took");
    assert!(
        !events(&buf, "sweep").iter().any(|e| e["what"] == "orphans" && e["removed"] != 0),
        "the orphan sweep took a logged retirement"
    );
    dump("superseded_ui_write_swept", &buf);
}


/// RepairYieldsToLaterUI. A adopts a UI write; while A's barrier is in
/// flight (its upload landed, not yet claimed) the UI writes the path again
/// and B publishes the later write. A's commit would re-cite its adoption
/// as a repair: it yields to the later acknowledged write the document
/// already cites — no record, no revert — and A's tree takes the later
/// version through its queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conformance_repair_yields_to_a_later_ui_write() {
    let (hs, store, buf, mut a, b, dir_a, dir_b) = two_writers_hooked(&[("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    let t = super::now_unix();
    let first = ui_write_at(&store, &a, "p.txt", "first", t).await;
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("keep.txt", "A's edit");
    let later: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let (st, later_in) = (store.clone(), later.clone());
    let b_back = inside(&hs, &a.cfg.file_key("keep.txt"), b, move |mut b| {
        let (st, later_in) = (st.clone(), later_in.clone());
        Box::pin(async move {
            *later_in.lock().unwrap() = Some(ui_write_at(&st, &b, "p.txt", "later", t + 5).await);
            b.run_barrier().await.expect("B's barrier");
            b
        })
    });
    let ra = a.run_barrier().await.unwrap();
    let mut b = b_back.lock().unwrap().take().expect("fixture: the hook never ran");
    let later = later.lock().unwrap().clone().unwrap();
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries["p.txt"].key, later, "A's repair re-cited its adoption over the later UI write: {ra:?}");
    assert!(ra.surfaced.is_empty(), "the later write was surfaced instead of winning: {ra:?}");
    for _ in 0..2 {
        a.run_barrier().await.unwrap();
        b.run_barrier().await.unwrap();
    }
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries["p.txt"].key, later);
    assert!(!m.entries.values().any(|e| e.key == first), "the superseded adoption stayed cited");
    for d in [dir_a.path(), dir_b.path()] {
        assert_eq!(read(d, "p.txt").as_deref(), Some("later"), "fixture: a tree kept the superseded version");
    }
    dump("repair_yields_to_a_later_ui_write", &buf);
}

/// CommitVerifiesUploads (R4a). A's upload lands; before A claims, B's
/// commit runs the orphan sweep and takes it (no grace). A's commit
/// re-reads what it is about to cite and WITHHOLDS the swept upload —
/// never cited, recorded — and the next barrier uploads it afresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conformance_commit_withholds_a_swept_upload() {
    let (hs, store, buf, mut a, mut b, dir_a, dir_b) = two_writers_hooked(&[("keep.txt", "keep")]).await;
    b.cfg.untracked_grace_secs = 0;
    b.state.save_orphan_sweep_at(super::now_unix() - b.cfg.untracked_sweep_secs - 1).unwrap();
    start(&store, &[("A", &a), ("B", &b)]).await;
    Agent { name: "B", sc: &b, root: dir_b.path() }.write("keep.txt", "B's edit");
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("x.txt", "A's upload");
    let b_back = inside(&hs, &a.cfg.file_key("x.txt"), b, |mut b| {
        Box::pin(async move {
            let rb = b.run_barrier().await.expect("B's barrier");
            assert!(rb.swept >= 1, "fixture: B's sweep did not take A's in-flight upload: {rb:?}");
            b
        })
    });
    let ra = a.run_barrier().await.unwrap();
    let mut b = b_back.lock().unwrap().take().expect("fixture: the hook never ran");
    assert_eq!(ra.parked, vec!["x.txt".to_string()], "the commit cited a handle the sweep took: {ra:?}");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("x.txt"), "cited a swept handle");
    let swept: Vec<serde_json::Value> = events(&buf, "sweep").into_iter().filter(|e| e["what"] == "orphans").collect();
    let uploaded: Vec<serde_json::Value> = events(&buf, "upload").into_iter().filter(|e| e["path"] == "x.txt").collect();
    assert_eq!(swept[0]["keys"], serde_json::json!([uploaded[0]["key"]]), "the sweep names A's upload");
    for _ in 0..2 {
        a.run_barrier().await.unwrap();
        b.run_barrier().await.unwrap();
    }
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries.get("x.txt").map(|e| e.etag.clone()), Some(etag_of("A's upload")), "fixture: never uploaded afresh");
    assert_eq!(read(dir_b.path(), "x.txt").as_deref(), Some("A's upload"));
    dump("commit_withholds_a_swept_upload", &buf);
}

/// ForeignPerPath. B holds the seed at x and is uploading a new file at y
/// when the UI renames x to y and A publishes the move: the document cites
/// the SEED's handle at y. B's commit publishes over it, and R7 surfaces
/// it — B's tree never held that handle AT y. A flat "every version this
/// tree ever integrated" rule reads the seed as known and records nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conformance_r7_is_per_path_across_a_rename() {
    let (hs, store, buf, a, mut b, dir_a, dir_b) = two_writers_hooked(&[("x.txt", "seed"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    let seed_key = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries["x.txt"].key.clone();
    Agent { name: "B", sc: &b, root: dir_b.path() }.write("y.txt", "B's y");
    let st = store.clone();
    let a_back = inside(&hs, &b.cfg.file_key("y.txt"), a, move |mut a| {
        let st = st.clone();
        Box::pin(async move {
            ui_rename(&st, &a, "x.txt", "y.txt").await;
            a.run_barrier().await.expect("A's barrier");
            a
        })
    });
    let rb = b.run_barrier().await.unwrap();
    let mut a = a_back.lock().unwrap().take().expect("fixture: the hook never ran");
    assert_eq!(rb.surfaced, vec!["y.txt".to_string()], "B published over the moved seed with no record: {rb:?}");
    let surfaced = events(&buf, "surface");
    assert_eq!(surfaced.len(), 1, "{surfaced:?}");
    for _ in 0..2 {
        a.run_barrier().await.unwrap();
        b.run_barrier().await.unwrap();
    }
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("x.txt"), "fixture: the move's source stayed cited");
    assert_eq!(m.entries["y.txt"].etag, etag_of("B's y"), "fixture: mine did not win at y");
    assert!(!m.entries.values().any(|e| e.key == seed_key), "the seed stayed cited");
    for d in [dir_a.path(), dir_b.path()] {
        assert_eq!((read(d, "x.txt"), read(d, "y.txt").as_deref()), (None, Some("B's y")), "fixture: a tree diverged");
    }
    dump("r7_is_per_path_across_a_rename", &buf);
}

/// RenameMovesEntry. The UI writes p1 — a pending entry, cited by nothing
/// — and renames it to p3 before any writer consumes. The entry IS the
/// citation being moved: it leaves p1, so no writer ever adopts the handle
/// at the old name, and the removal carries the version the write was
/// written over, which every tree holds clean.
#[tokio::test]
async fn conformance_rename_of_a_pending_ui_write() {
    let (store, buf, mut a, mut b, dir_a, dir_b) = two_writers(&[("p1.txt", "seed"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    let ui = ui_write(&store, &a, "p1.txt", "from the UI").await;
    ui_rename(&store, &a, "p1.txt", "p3.txt").await;
    for _ in 0..3 {
        b.run_barrier().await.unwrap();
        a.run_barrier().await.unwrap();
    }
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("p1.txt"), "fixture: the source stayed cited");
    assert_eq!(m.entries["p3.txt"].key, ui);
    for d in [dir_a.path(), dir_b.path()] {
        assert_eq!((read(d, "p1.txt"), read(d, "p3.txt").as_deref()), (None, Some("from the UI")), "fixture: a tree did not move");
    }
    dump("rename_of_a_pending_ui_write", &buf);
}


/// ParkedKeepsMergeBase (L-126). The UI saves p and B publishes it; A's
/// agent edits p from the seed, and B's commit sweeps A's upload before A
/// claims, so A's commit withholds it (parked). The next barrier re-uploads
/// A's edit over the user's save, which A never integrated — and R7 records
/// it, because the parked path kept the merge base it had.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conformance_withheld_republished_over_a_ui_save() {
    let (hs, store, buf, mut a, mut b, dir_a, dir_b) = two_writers_hooked(&[("p.txt", "seed"), ("keep.txt", "keep")]).await;
    start(&store, &[("A", &a), ("B", &b)]).await;
    ui_write(&store, &a, "p.txt", "the user's save").await;
    b.run_barrier().await.unwrap();
    // B's orphan sweep is due at its NEXT commit, with no grace.
    b.cfg.untracked_grace_secs = 0;
    b.state.save_orphan_sweep_at(super::now_unix() - b.cfg.untracked_sweep_secs - 1).unwrap();
    Agent { name: "B", sc: &b, root: dir_b.path() }.write("keep.txt", "B's edit");
    Agent { name: "A", sc: &a, root: dir_a.path() }.write("p.txt", "A's edit");
    let b_back = inside(&hs, &a.cfg.file_key("p.txt"), b, |mut b| {
        Box::pin(async move {
            let rb = b.run_barrier().await.expect("B's barrier");
            assert!(rb.swept >= 1, "fixture: B's sweep did not take A's in-flight upload: {rb:?}");
            b
        })
    });
    let ra = a.run_barrier().await.unwrap();
    let mut b = b_back.lock().unwrap().take().expect("fixture: the hook never ran");
    assert_eq!(ra.parked, vec!["p.txt".to_string()], "fixture: {ra:?}");
    let ra2 = a.run_barrier().await.unwrap();
    assert_eq!(ra2.surfaced, vec!["p.txt".to_string()], "the re-upload replaced the user's save with no record: {ra2:?}");
    for _ in 0..2 {
        b.run_barrier().await.unwrap();
        a.run_barrier().await.unwrap();
    }
    assert_eq!(read(dir_b.path(), "p.txt").as_deref(), Some("A's edit"), "fixture: B did not converge");
    dump("withheld_republished_over_a_ui_save", &buf);
}
