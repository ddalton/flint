//! The latency plan's lever A on the gateway
//! (`docs/plans/flint-lean-latency-plan-2026-10-23.md`): a UI save judged
//! and committed against the document the gateway already holds, its
//! chunks kept in memory beside it. Requests are counted at the store.

use std::sync::Arc;

use flint_lean::manifest::{self, Entries, LeanEntry, LeanManifest, PublishStamps};
use flint_lean::LeanConfig;
use flint_lean_gateway::{Bytes, MemoryStore, ObjectStore, PutFile, Workspace};
use flint_store::RequestCounts;

const PREFIX: &str = "tenant/proj1";

fn entry(i: usize) -> LeanEntry {
    LeanEntry {
        key: format!("{PREFIX}/files/src/f{i:05}.txt@seed"),
        etag: format!("\"e{i}\""),
        crc64_b64: "AAAAAAAAAAA=".into(),
        size: 10,
        mode: 0o644,
        mtime_unix: 1_700_000_000,
        generation: 1,
        epoch: 1,
    }
}

/// A workspace whose manifest is several chunks at the shipped chunk size.
async fn seeded(store: &Arc<dyn ObjectStore>, n: usize) -> usize {
    let mut cfg = LeanConfig::new(PREFIX, "/nonexistent");
    cfg.chunk_cache = None;
    let m = LeanManifest {
        seq: 1,
        entries: (0..n).map(|i| (format!("src/f{i:05}.txt"), entry(i))).collect(),
        ..Default::default()
    };
    manifest::cas_write_chunked(
        store.as_ref(),
        &cfg,
        &m,
        None,
        &[],
        PublishStamps { epoch: 1, flush_uuid: "seed", boundary_source: None },
    )
    .await
    .unwrap();
    let p = manifest::load_pointer(store.as_ref(), &cfg).await.unwrap().unwrap().pointer;
    match p.entries().unwrap() {
        Entries::Chunked(c) => c.len(),
        _ => panic!("not chunked"),
    }
}

fn delta(a: RequestCounts, b: RequestCounts) -> RequestCounts {
    RequestCounts {
        get: b.get - a.get,
        head: b.head - a.head,
        put: b.put - a.put,
        copy: b.copy - a.copy,
        delete: b.delete - a.delete,
        list: b.list - a.list,
        multipart: b.multipart - a.multipart,
    }
}

fn create() -> PutFile {
    PutFile { if_none_match: Some("*".into()), ..Default::default() }
}

/// The first save fetches the document once — the judge and the commit
/// share it — and every save after fetches no chunk at all: two pointer
/// reads and the one LIST that proves the cached chunks are still there.
/// Before the cache, each save loaded the whole document twice.
#[tokio::test]
async fn a_ui_save_fetches_no_manifest_chunk_once_the_gateway_holds_the_document() {
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = inner.clone();
    let chunks = seeded(&store, 40_000).await as u64;
    assert!(chunks >= 3, "{chunks} chunks: the fixture cannot tell one load from two");
    let ws = Workspace::new(store.clone(), PREFIX);

    let a = inner.request_counts().unwrap();
    ws.put_file("notes/a.md", Bytes::from("one"), &create()).await.unwrap();
    let first = delta(a, inner.request_counts().unwrap());
    assert_eq!(first.get, 2 + chunks, "the first save loaded the document more than once: {first:?}");

    for (k, path) in ["notes/b.md", "notes/c.md", "notes/d.md"].iter().enumerate() {
        let a = inner.request_counts().unwrap();
        ws.put_file(path, Bytes::from(format!("save {k}")), &create()).await.unwrap();
        let d = delta(a, inner.request_counts().unwrap());
        assert_eq!(d.get, 2, "save {k} fetched manifest chunks: {d:?}");
        assert_eq!(d.list, 1, "save {k}: the cached chunks' existence is proved once");
    }
    let snap = ws.snapshot().await.unwrap();
    for path in ["notes/a.md", "notes/b.md", "notes/c.md", "notes/d.md", "src/f00042.txt"] {
        assert!(snap.manifest.entries.contains_key(path), "{path} is not cited");
    }
    assert_eq!(snap.manifest.entries.len(), 40_004);
}

/// The control: a gateway whose chunk cache is off fetches every chunk
/// for every pointer move, so the same second save costs the document.
#[tokio::test]
async fn without_the_chunk_cache_a_save_after_a_save_refetches_the_document() {
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = inner.clone();
    let chunks = seeded(&store, 40_000).await as u64;
    let ws = Workspace::new(store.clone(), PREFIX).without_chunk_cache();
    ws.put_file("notes/a.md", Bytes::from("one"), &create()).await.unwrap();
    let a = inner.request_counts().unwrap();
    ws.put_file("notes/b.md", Bytes::from("two"), &create()).await.unwrap();
    let d = delta(a, inner.request_counts().unwrap());
    assert_eq!(d.get, 2 + chunks, "the control fetched {d:?}");
    assert_eq!(d.list, 0);
}
