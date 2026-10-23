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
/// share it — and every save after fetches nothing but two pointer reads:
/// the document its own commit installed is held under the pointer it wrote
/// (latency plan §16), so nothing is reloaded and no LIST is needed. Before
/// the cache, each save loaded the whole document twice; before §16, each
/// re-assembled it from cached chunks after a LIST.
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
        assert_eq!(d.list, 0, "save {k} reloaded the document its own commit installed: {d:?}");
    }
    let snap = ws.snapshot().await.unwrap();
    for path in ["notes/a.md", "notes/b.md", "notes/c.md", "notes/d.md", "src/f00042.txt"] {
        assert!(snap.manifest.entries.contains_key(path), "{path} is not cited");
    }
    assert_eq!(snap.manifest.entries.len(), 40_004);
}

/// The chunk cache, and its control: once someone else moves the pointer
/// (here a second gateway), the next save loads the new document. With the
/// cache it fetches only the chunks it lacks; with the cache off it fetches
/// every chunk. (A save after the gateway's OWN save fetches nothing either
/// way: it holds what it installed, latency plan §16.)
#[tokio::test]
async fn without_the_chunk_cache_a_save_after_a_peers_commit_refetches_the_document() {
    let mut fetched = vec![];
    for cached in [true, false] {
        let inner = Arc::new(MemoryStore::new());
        let store: Arc<dyn ObjectStore> = inner.clone();
        let chunks = seeded(&store, 40_000).await as u64;
        let ws = match cached {
            true => Workspace::new(store.clone(), PREFIX),
            false => Workspace::new(store.clone(), PREFIX).without_chunk_cache(),
        };
        ws.put_file("notes/a.md", Bytes::from("one"), &create()).await.unwrap();
        // A peer: another gateway, with its own cache.
        Workspace::new(store.clone(), PREFIX).put_file("notes/x.md", Bytes::from("peer"), &create()).await.unwrap();
        let a = inner.request_counts().unwrap();
        ws.put_file("notes/b.md", Bytes::from("two"), &create()).await.unwrap();
        let d = delta(a, inner.request_counts().unwrap());
        fetched.push((cached, d.get, chunks));
    }
    let (_, cached_gets, chunks) = fetched[0];
    let (_, uncached_gets, _) = fetched[1];
    assert_eq!(uncached_gets, 2 + chunks, "the control did not fetch the document: {fetched:?}");
    assert!(cached_gets < 2 + chunks / 2, "the cache fetched most of the document: {fetched:?}");
}

/// The document a commit installs, made in place from the one it edited
/// (latency plan §16), is the document a fresh load reads: after saves, an
/// overwrite, a rename and a delete, the held snapshot and a load agree on
/// every entry, the generation and the tombstones.
#[tokio::test]
async fn the_held_document_after_each_commit_is_what_a_load_reads() {
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = inner.clone();
    seeded(&store, 40_000).await;
    let ws = Workspace::new(store.clone(), PREFIX);
    ws.put_file("notes/a.md", Bytes::from("one"), &create()).await.unwrap();
    ws.put_file("notes/b.md", Bytes::from("two"), &create()).await.unwrap();
    let etag = ws.get_file("notes/a.md").await.unwrap().etag;
    let over = PutFile { if_match: Some(etag), ..Default::default() };
    ws.put_file("notes/a.md", Bytes::from("one, again"), &over).await.unwrap();
    ws.rename_file("notes/a.md", "notes/z.md", None).await.unwrap();
    ws.remove_file("src/f00042.txt", None, None).await.unwrap();
    let a = inner.request_counts().unwrap();
    let held = ws.view().await.unwrap().unwrap();
    let d = delta(a, inner.request_counts().unwrap());
    assert_eq!((d.get, d.list), (1, 0), "the document the last commit installed was not held: {d:?}");
    let mut cfg = LeanConfig::new(PREFIX, "/nonexistent");
    cfg.chunk_cache = None;
    let loaded = manifest::load(store.as_ref(), &cfg).await.unwrap().unwrap().manifest;
    assert_eq!(held.manifest.seq, loaded.seq, "the held generation is not the bucket's");
    assert_eq!(held.manifest.tombstones, loaded.tombstones, "the held tombstones are not the bucket's");
    assert!(held.manifest.entries == loaded.entries, "the held entries are not the bucket's");
    assert!(loaded.entries.contains_key("notes/z.md") && !loaded.entries.contains_key("notes/a.md"));
    assert!(!loaded.entries.contains_key("src/f00042.txt"));
}
