//! Lever G's chunk view (`docview`; latency plan §12): what the shadow
//! check cannot see. A hole is refused as a full load refuses it; a
//! one-file publish reads a few chunks, not the document; and a handle
//! cited at another path is not retired by an edit at its own.

use std::sync::Arc;

use flint_store::memory::MemoryStore;
use flint_store::ObjectStore;

use super::docview::{DocView, Opened};
use super::manifest;
use super::tests::{gateway_commit, read, sweep_mid_read, syncer, write};
use super::{now_unix, ui_flush};

/// A chunk swept from under a view: at its LIST (the chunk cache on) the
/// view decides as a full load does, restarting onto a moved pointer and
/// refusing under an unchanged one. At a GET after the LIST (the cache
/// off) it never returns a document missing entries: it restarts or it
/// refuses, and the barrier retries.
#[tokio::test]
async fn a_view_refuses_a_hole_and_never_serves_a_short_document() {
    for (move_pointer, cached) in [(true, true), (false, true), (true, false), (false, false)] {
        let (inner, store, cfg, m2, doomed_key, _dir) = sweep_mid_read(move_pointer, cached).await;
        let got = match DocView::open(store.clone(), &cfg, None).await {
            Ok(Opened::View(mut v)) => v.assemble().await.map(|l| l.manifest),
            Ok(Opened::Full(_)) => panic!("fixture: a chunked document did not open as a view"),
            Err(e) => Err(e),
        };
        assert!(inner.head(&doomed_key).await.is_err(), "the sweep never ran (cached: {cached}): this arm decided nothing");
        match (got, move_pointer, cached) {
            (Ok(m), true, _) => {
                assert_eq!(m.entries, m2.entries, "the view restarted onto a mixed or stale generation");
                assert_eq!(m.seq, 2);
            }
            (Ok(_), false, _) => panic!("a chunk vanished under an UNCHANGED pointer and the view served the document"),
            (Err(e), true, true) => panic!("a view swept at its LIST under a moved pointer did not restart: {e}"),
            (Err(e), _, _) => assert!(e.to_string().contains("hole"), "wrong refusal: {e}"),
        }
    }
}

async fn seed(view: bool) -> (Arc<MemoryStore>, super::Syncer, tempfile::TempDir, usize) {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.cfg.chunk_cache = None;
    a.cfg.chunk_target = 32;
    a.cfg.chunk_min = 8;
    a.cfg.chunk_max = 128;
    a.cfg.chunk_view = view;
    // The shadow check reads the whole document, which is what is counted.
    a.cfg.g_shadow = false;
    a.checkout().await.unwrap();
    let n = 2000;
    for i in 0..n {
        write(dir.path(), &format!("d{:02}/f{i:04}.txt", i % 20), "seed");
    }
    for _ in 0..6 {
        a.floor_tick().await.unwrap();
        let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap();
        if m.manifest.entries.len() == n {
            break;
        }
    }
    let p = manifest::load_pointer(store.as_ref(), &a.cfg).await.unwrap().unwrap().pointer;
    let chunks = match p.entries().unwrap() {
        manifest::Entries::Chunked(c) => c.len(),
        _ => panic!("fixture: not chunked"),
    };
    (store, a, dir, chunks)
}

/// The point of G, as requests: with the chunk cache off every chunk read is
/// a GET, and a one-file publish's commit section (its load, merge and CAS
/// steps) reads the chunk its path falls in, where the whole-document path
/// reads them all. The consume is G2's and is not counted. The edit replaces
/// a handle, so the retire set asks about aliases: the writer encoded every
/// chunk itself, and knows theirs without reading one.
#[tokio::test]
async fn a_one_file_publish_reads_a_few_chunks_not_the_document() {
    let mut gets = vec![];
    for view in [true, false] {
        let (_store, mut a, dir, chunks) = seed(view).await;
        assert!(chunks >= 30, "fixture: only {chunks} chunks");
        write(dir.path(), "d07/f0007.txt", "edited, a size apart");
        let r = a.run_barrier().await.unwrap();
        assert_eq!(r.uploaded.len(), 1, "fixture: the edit did not publish");
        let commit: u64 = r
            .steps
            .iter()
            .filter(|s| ["load", "fence", "merge", "cas", "retire_set", "surface"].contains(&s.step))
            .filter_map(|s| s.requests.as_ref().map(|c| c.get))
            .sum();
        let all: Vec<String> =
            r.steps.iter().map(|s| format!("{} {}", s.step, s.requests.as_ref().map_or(0, |c| c.get))).collect();
        println!("view {view}: commit GETs {commit}; by step: {}", all.join(", "));
        gets.push((view, commit, chunks));
    }
    let (_, view_gets, chunks) = gets[0];
    let (_, full_gets, _) = gets[1];
    println!("a one-file publish of {chunks} chunks: {view_gets} GETs through the view, {full_gets} through the whole document");
    assert!(full_gets >= chunks as u64, "control: the whole-document path read {full_gets} of {chunks} chunks");
    assert!(view_gets + (chunks as u64) / 2 <= full_gets, "the view read {view_gets} GETs against {full_gets}");
    assert!(view_gets <= 10, "a one-file publish through the view sent {view_gets} GETs");
}

/// §12, claim 2. The gateway cites x's handle at z as well (z is then an
/// alias: its key names x). An edit at x drops the handle there, and it is
/// still cited at z, so the commit does not retire it and the collection
/// pass keeps its bytes. The control: once z goes too, it is collected.
#[tokio::test]
async fn a_handle_cited_at_another_path_survives_an_edit_at_its_own() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    write(dir.path(), "x.txt", "the bytes x and z share");
    write(dir.path(), "keep.txt", "keep");
    a.floor_tick().await.unwrap();
    let key = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries["x.txt"].key.clone();
    gateway_commit(&store, &a.cfg, &ui_flush(now_unix()), |doc| {
        let e = doc.entries["x.txt"].clone();
        doc.entries.insert("z.txt".into(), e);
    })
    .await
    .unwrap();
    a.floor_tick().await.unwrap();
    assert_eq!(read(dir.path(), "z.txt").as_deref(), Some("the bytes x and z share"), "fixture: z never arrived");

    write(dir.path(), "x.txt", "x edited, a size apart");
    let o = a.floor_tick().await.unwrap();
    assert_eq!(o.uploaded, 1, "fixture: the edit did not publish");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries["z.txt"].key, key, "fixture: z no longer cites x's old handle");
    assert_ne!(m.entries["x.txt"].key, key, "fixture: x still cites its old handle");
    assert!(store.head(&key).await.is_ok(), "a handle z still cites was collected");

    // The control: the agent deletes z, the last path citing the handle.
    // Its commit retires it (z is the alias, so this goes through the
    // view's alias path too), and the collection pass takes it (the tests'
    // config: no retire age, a pass at every commit).
    std::fs::remove_file(dir.path().join("z.txt")).unwrap();
    for _ in 0..3 {
        a.floor_tick().await.unwrap();
        if store.head(&key).await.is_err() {
            break;
        }
    }
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert!(!m.entries.contains_key("z.txt"), "fixture: the delete of z never published");
    assert!(store.head(&key).await.is_err(), "control: a handle nothing cites was never collected");
}

/// The other half of claim 2: the edit is at the ALIAS, z, and the path the
/// key names, x, still cites the handle. The view finds that citation by the
/// key's own path, not among the aliases, and keeps the handle.
#[tokio::test]
async fn a_handle_its_own_path_still_cites_survives_an_edit_at_an_alias() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut a = syncer(&store, dir.path()).await;
    a.checkout().await.unwrap();
    write(dir.path(), "x.txt", "the bytes x and z share");
    a.floor_tick().await.unwrap();
    let key = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest.entries["x.txt"].key.clone();
    gateway_commit(&store, &a.cfg, &ui_flush(now_unix()), |doc| {
        let e = doc.entries["x.txt"].clone();
        doc.entries.insert("z.txt".into(), e);
    })
    .await
    .unwrap();
    a.floor_tick().await.unwrap();
    assert_eq!(read(dir.path(), "z.txt").as_deref(), Some("the bytes x and z share"), "fixture: z never arrived");

    write(dir.path(), "z.txt", "z edited, a size apart");
    let o = a.floor_tick().await.unwrap();
    assert_eq!(o.uploaded, 1, "fixture: the edit did not publish");
    let m = manifest::load(store.as_ref(), &a.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries["x.txt"].key, key, "fixture: x no longer cites its handle");
    assert_ne!(m.entries["z.txt"].key, key, "fixture: z still cites x's handle");
    assert!(store.head(&key).await.is_ok(), "a handle x still cites was collected");
}

/// `clashes_view` answers what `path_clashes` answers over the whole merged
/// document, over random documents full of file/directory name collisions,
/// random upserts and random deletes, read through a real view.
#[tokio::test]
async fn clashes_view_is_path_clashes_over_random_documents() {
    use std::collections::{BTreeMap, BTreeSet};

    use super::barrier::{clashes_view, path_clashes};
    use super::tests::{cfg_for, entry_at};

    let mut r: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = |n: u64| {
        r ^= r << 13;
        r ^= r >> 7;
        r ^= r << 17;
        r % n.max(1)
    };
    // Few names, shallow trees: `a` and `a/b` collide often.
    let names = ["a", "b", "c", "ab"];
    let mut path = |depth: u64| -> String {
        let d = 1 + depth;
        (0..d).map(|_| names[next(names.len() as u64) as usize]).collect::<Vec<_>>().join("/")
    };
    let mut clashing = 0;
    for seed in 0..300u64 {
        let store = Arc::new(MemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = cfg_for(dir.path());
        cfg.chunk_target = 4;
        cfg.chunk_min = 1;
        cfg.chunk_max = 8;
        let mut theirs = super::manifest::LeanManifest { seq: 1, ..Default::default() };
        for _ in 0..(seed % 40) {
            let p = path(seed % 3);
            theirs.entries.insert(p.clone(), entry_at(&p, 1));
        }
        manifest::cas_write_chunked(
            store.as_ref(),
            &cfg,
            &theirs,
            None,
            &[],
            manifest::PublishStamps { epoch: 1, flush_uuid: "u1", boundary_source: None },
        )
        .await
        .unwrap();
        let mut upserts: BTreeMap<String, super::manifest::LeanEntry> = BTreeMap::new();
        for _ in 0..(1 + seed % 6) {
            let p = path(seed % 4);
            upserts.insert(p.clone(), entry_at(&p, 2));
        }
        let deletes: BTreeSet<String> = theirs
            .entries
            .keys()
            .filter(|p| !upserts.contains_key(*p))
            .filter(|_| seed % 3 == 0)
            .take(2)
            .cloned()
            .collect();
        let mut overlay: BTreeMap<String, Option<super::manifest::LeanEntry>> =
            upserts.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
        for p in &deletes {
            overlay.insert(p.clone(), None);
        }
        let mut merged = theirs.entries.clone();
        for (k, v) in &overlay {
            match v {
                Some(e) => {
                    merged.insert(k.clone(), e.clone());
                }
                None => {
                    merged.remove(k);
                }
            }
        }
        let store_dyn: Arc<dyn ObjectStore> = store.clone();
        let Opened::View(mut view) = DocView::open(store_dyn, &cfg, None).await.unwrap() else {
            panic!("fixture: not a view");
        };
        let got = clashes_view(&mut view, &overlay, &upserts).await.unwrap();
        let want = path_clashes(&merged, &upserts);
        clashing += usize::from(!want.is_empty());
        assert_eq!(got, want, "seed {seed}: theirs {:?}, upserts {:?}, deletes {:?}", theirs.entries.keys().collect::<Vec<_>>(), upserts.keys().collect::<Vec<_>>(), deletes);
    }
    assert!(clashing >= 30, "only {clashing} of 300 seeds clash: the test decides little");
}
