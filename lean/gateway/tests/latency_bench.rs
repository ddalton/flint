//! What a UI verb costs at a size, before the gateway's commits change
//! (latency plan §9 item 7: measure first). Wall time and requests for a
//! first save (which loads the document), later saves, an overwrite, a
//! rename and a delete. Run by hand, release (`--features fastalloc` for
//! the mimalloc arm; build for x86_64-unknown-linux-musl for what ships):
//!   FLINT_BENCH_N=200000 FLINT_BENCH_DELAY_MS=25 \
//!     cargo test --release --test latency_bench -- --ignored --nocapture

use std::sync::Arc;
use std::time::Instant;

use flint_lean::manifest::{self, LeanEntry, LeanManifest, PublishStamps};
use flint_lean::LeanConfig;
use flint_lean_gateway::{Bytes, MemoryStore, ObjectStore, PutFile, Workspace};
use flint_store::RequestCounts;

// The arm with mimalloc (`fastalloc`); without it, the libc's malloc, which
// is what the shipped musl binary runs today.
#[cfg(feature = "fastalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const PREFIX: &str = "tenant/proj1";

fn env(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn entry(i: usize) -> LeanEntry {
    LeanEntry {
        key: format!("{PREFIX}/files/src/d{:03}/f{i:07}.txt@seed", i % 512),
        etag: format!("\"e{i}\""),
        crc64_b64: "AAAAAAAAAAA=".into(),
        size: 10,
        mode: 0o644,
        mtime_unix: 1_700_000_000,
        generation: 1,
        epoch: 1,
    }
}

fn ops(inner: &MemoryStore, a: RequestCounts) -> String {
    let b = inner.request_counts().unwrap();
    format!(
        "get={} head={} put={} list={} copy={} delete={}",
        b.get - a.get,
        b.head - a.head,
        b.put - a.put,
        b.list - a.list,
        b.copy - a.copy,
        b.delete - a.delete
    )
}

#[tokio::test]
#[ignore]
async fn bench_ui_verbs() {
    let n = env("FLINT_BENCH_N", 20_000) as usize;
    let delay = env("FLINT_BENCH_DELAY_MS", 0);
    let inner = Arc::new(MemoryStore::new());
    let store: Arc<dyn ObjectStore> = inner.clone();
    let mut cfg = LeanConfig::new(PREFIX, "/nonexistent");
    cfg.chunk_cache = None;
    let t = Instant::now();
    let m = LeanManifest {
        seq: 1,
        entries: (0..n).map(|i| (format!("src/d{:03}/f{i:07}.txt", i % 512), entry(i))).collect(),
        ..Default::default()
    };
    let seed = PublishStamps { epoch: 1, flush_uuid: "seed", boundary_source: None };
    manifest::cas_write_chunked(store.as_ref(), &cfg, &m, None, &[], seed).await.unwrap();
    drop(m);
    println!("BENCH n={n} seed_ms={}", t.elapsed().as_millis());
    inner.inject_request_delay_ms(delay);
    // FLINT_BENCH_ARM=whole: every commit edits the whole document and the
    // next verb reloads it (the shape before latency plan §16).
    let arm = std::env::var("FLINT_BENCH_ARM").unwrap_or_else(|_| "shipped".into());
    let ws = match arm.as_str() {
        "whole" => Workspace::new(store.clone(), PREFIX).without_chunk_view(),
        _ => Workspace::new(store.clone(), PREFIX),
    };
    println!("BENCH n={n} arm={arm}");
    let create = PutFile { if_none_match: Some("*".into()), ..Default::default() };

    macro_rules! timed {
        ($op:expr, $fut:expr) => {{
            let a = inner.request_counts().unwrap();
            let t = Instant::now();
            let r = $fut.await.unwrap();
            println!("BENCH n={n} delay_ms={delay} op={} ms={} ops: {}", $op, t.elapsed().as_millis(), ops(&inner, a));
            r
        }};
    }
    timed!("save_first", ws.put_file("notes/a.md", Bytes::from("one"), &create));
    for k in 0..3 {
        timed!(format!("save{k}"), ws.put_file(&format!("notes/b{k}.md"), Bytes::from(format!("save {k}")), &create));
    }
    let etag = ws.get_file("notes/a.md").await.unwrap().etag;
    let over = PutFile { if_match: Some(etag), ..Default::default() };
    timed!("overwrite", ws.put_file("notes/a.md", Bytes::from("two"), &over));
    timed!("rename", ws.rename_file("notes/a.md", "notes/z.md", None));
    timed!("delete", ws.remove_file("notes/z.md", None, None));
    timed!("save_after", ws.put_file("notes/c.md", Bytes::from("three"), &create));
}
