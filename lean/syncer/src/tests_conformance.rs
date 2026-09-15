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
use super::manifest;
use super::sentinel::Verb;
use super::tests::{backdate_baseline, clear_min_interval, hitl_write, read, syncer, touch_sentinel, write};
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
    for (name, sc) in writers {
        sc.trace("conf_start", serde_json::json!({"writer": name, "seq": m.seq, "entries": entries}));
    }
}

async fn ui_write(store: &Arc<MemoryStore>, via: &Syncer, rel: &str, content: &str) {
    let etag = hitl_write(store, &via.cfg, rel, content, "reviewer").await.unwrap();
    via.trace("conf_hitl_write", serde_json::json!({"path": rel, "etag": etag}));
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
/// publish: the ack is partial, and the re-touch publishes the delete.
#[tokio::test]
async fn conformance_outranked_delete_publish() {
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
    assert_eq!(ack.status, "partial", "fixture: {ack:?}");
    let agent_a = Agent { name: "A", sc: &a, root: dir_a.path() };
    agent_a.touch("n-2");
    a.poll_sentinels().unwrap();
    clear_min_interval(&a);
    let again = a.honor_pending(Verb::Publish, false).await.unwrap().unwrap();
    assert_eq!(again.status, "ok", "fixture: {again:?}");
    b.run_barrier().await.unwrap();
    dump("outranked_delete_publish", &buf);
}
