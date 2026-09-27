//! The library API, verb by verb, on the in-memory store — what an
//! embedder's backend sees, with no HTTP anywhere. The gateway's own
//! battery (`battery.rs`) drives the same verbs through the wire; this
//! file pins what the typed surface promises: the refusals, what they
//! carry, and the order of writes.

use std::sync::Arc;
use std::time::Duration;

use flint_lean_gateway::{
    crc64_nvme, crc64_to_b64, Bytes, LeanEntry, LeanManifest, MemoryStore, ObjectStore, PutFile,
    StoreError, VerbError, Workspace,
};

const PREFIX: &str = "tenant/proj1";

fn store() -> Arc<dyn ObjectStore> {
    Arc::new(MemoryStore::new())
}

fn ws(store: &Arc<dyn ObjectStore>) -> Workspace {
    Workspace::new(store.clone(), PREFIX)
}

/// A syncer's lease on the workspace, so the epoch-validated verbs
/// have a cell to validate against. Returns the epoch.
async fn hold_lease(store: &Arc<dyn ObjectStore>, w: &Workspace) -> u64 {
    let lease = store.epoch_acquire(&w.config().epoch_key(), "syncer-1", None).await.unwrap();
    lease.epoch
}

/// Every handle of `path` the bucket holds — what a HEAD of the bare
/// path answered before handles.
async fn handles_of(s: &Arc<dyn ObjectStore>, w: &Workspace, path: &str) -> Vec<String> {
    let cfg = w.config();
    let mut keys: Vec<String> = s
        .list(&cfg.files_prefix())
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .filter(|k| cfg.handle_parts(k).map(|(p, _)| p == path).unwrap_or(false))
        .collect();
    keys.sort();
    keys
}

/// The handle the manifest cites at `path`.
async fn tracked_key(w: &Workspace, path: &str) -> String {
    w.snapshot().await.unwrap().manifest.entries.get(path).map(|e| e.key.clone()).expect("cited")
}

/// A syncer's publish, in one CAS: a document citing `path` at `etag`
/// with the CRC of `body` — at the handle the current document already
/// names for that version (a UI save cites itself, P2), else the bare path.
async fn cite(w: &Workspace, epoch: u64, seq: u64, path: &str, etag: &str, body: &[u8]) -> String {
    let key = w
        .snapshot()
        .await
        .unwrap()
        .manifest
        .entries
        .get(path)
        .filter(|e| e.etag == etag)
        .map(|e| e.key.clone())
        .unwrap_or_else(|| w.config().file_key(path));
    let mut m = LeanManifest { seq, ..Default::default() };
    m.entries.insert(
        path.to_string(),
        LeanEntry {
            key,
            etag: etag.to_string(),
            crc64_b64: crc64_to_b64(crc64_nvme(body)),
            size: body.len() as u64,
            mode: 0o644,
            mtime_unix: 0,
            generation: seq,
            epoch,
        },
    );
    let current = w.snapshot().await.unwrap().manifest_etag;
    w.cas_manifest(&m, current.as_deref(), epoch, &format!("test-{seq}")).await.unwrap()
}

/// P2 (simplification step 5, 2026-09-25): a UI save COMMITS. The gateway
/// PUTs a fresh handle, CASes the manifest itself — mine wins over nothing
/// it did not read, since every overwrite names what it read — and
/// acknowledges AFTER the CAS. It never waits on the writers' lease: here a
/// writer holds it with its commit window open (a stalled holder), and the
/// save lands anyway (the user's G1). No inbox entry, no syncer needed.
#[tokio::test]
async fn a_ui_save_commits_directly_even_while_a_writer_holds_the_lease() {
    let s = store();
    let w = ws(&s);
    hold_lease(&s, &w).await;

    let create = PutFile { if_none_match: Some("*".into()), ..Default::default() };
    let e1 = w.put_file("a.txt", Bytes::from("one"), &create).await.unwrap();
    let snap = w.snapshot().await.unwrap();
    let cited = snap.manifest.entries.get("a.txt").expect("the save is not cited");
    assert_eq!(cited.etag, e1);
    assert!(w.config().handle_parts(&cited.key).is_some(), "not a handle: {}", cited.key);
    assert_eq!(cited.epoch, 0, "a gateway commit carries no lease epoch");
    let seq1 = snap.manifest.seq;

    let ok = PutFile { if_match: Some(e1.clone()), ..Default::default() };
    let e2 = w.put_file("a.txt", Bytes::from("two"), &ok).await.unwrap();
    let snap = w.snapshot().await.unwrap();
    assert_eq!(snap.manifest.entries["a.txt"].etag, e2);
    assert_eq!(snap.manifest.seq, seq1 + 1);
    assert_eq!(w.get_file("a.txt").await.unwrap().body, Bytes::from("two"));

    // A save naming what is no longer there commits nothing.
    let stale = PutFile { if_match: Some(e1.clone()), ..Default::default() };
    let err = w.put_file("a.txt", Bytes::from("three"), &stale).await.unwrap_err();
    assert!(matches!(&err, VerbError::FileChanged { current: Some(c) } if c == &e2), "{err}");
    assert_eq!(w.snapshot().await.unwrap().manifest.entries["a.txt"].etag, e2);
}

/// A mirror — a workspace published by exactly one writer — takes no UI
/// writes: under P2 a save would commit into the publisher's document.
/// Refused before any bytes move.
#[tokio::test]
async fn a_save_to_a_mirror_is_refused_before_it_writes() {
    let s = store();
    let w = ws(&s);
    let epoch = hold_lease(&s, &w).await;
    let m = LeanManifest { seq: 1, sole_writer: true, ..Default::default() };
    w.cas_manifest(&m, None, epoch, "mirror").await.unwrap();
    let err = w.put_file("a.txt", Bytes::from("x"), &PutFile::default()).await.unwrap_err();
    assert!(matches!(err, VerbError::ReadOnly), "{err}");
    assert!(handles_of(&s, &w, "a.txt").await.is_empty(), "a refused save wrote bytes");
    assert!(w.snapshot().await.unwrap().manifest.entries.is_empty());

    // A delete and a rename are UI writes too.
    let mut m = w.snapshot().await.unwrap().manifest;
    m.seq += 1;
    m.entries.insert("p.txt".into(), LeanEntry {
        key: w.config().file_key("p.txt"), etag: "\"p\"".into(), crc64_b64: "AAAAAAAAAAA=".into(),
        size: 1, mode: 0o644, mtime_unix: 0, generation: 1, epoch,
    });
    let current = w.snapshot().await.unwrap().manifest_etag;
    w.cas_manifest(&m, current.as_deref(), epoch, "mirror-2").await.unwrap();
    assert!(matches!(w.remove_file("p.txt", None, None).await.unwrap_err(), VerbError::ReadOnly));
    assert!(matches!(w.rename_file("p.txt", "q.txt", None).await.unwrap_err(), VerbError::ReadOnly));
    assert!(w.snapshot().await.unwrap().manifest.entries.contains_key("p.txt"));
}

/// Every UI commit keeps the document's tombstones to the same window the
/// writers' merge does (`TOMBSTONE_KEEP_SEQS`): one past it is dropped.
#[tokio::test]
async fn a_ui_commit_drops_tombstones_past_the_keep_window() {
    let s = store();
    let w = ws(&s);
    let epoch = hold_lease(&s, &w).await;
    let seq = 20_000;
    let mut m = LeanManifest { seq, ..Default::default() };
    m.tombstones.insert("old.txt".into(), flint_lean::manifest::Tombstone { etag: "\"o\"".into(), seq: 1 });
    m.tombstones.insert("young.txt".into(), flint_lean::manifest::Tombstone { etag: "\"y\"".into(), seq: seq - 1 });
    w.cas_manifest(&m, None, epoch, "old").await.unwrap();
    w.put_file("a.txt", Bytes::from("x"), &PutFile::default()).await.unwrap();
    let doc = w.snapshot().await.unwrap().manifest;
    assert!(!doc.tombstones.contains_key("old.txt"), "a tombstone past the keep window survived");
    assert!(doc.tombstones.contains_key("young.txt"));
}

/// A save's commit is the installing party's: it clears a tombstone at the
/// path it cites again and does not inherit a writer's boundary stamp (a UI
/// save is not a declared-coherent boundary) — as `manifest::merge` does.
#[tokio::test]
async fn a_save_clears_the_paths_tombstone_and_inherits_no_boundary_stamp() {
    let s = store();
    let w = ws(&s);
    let epoch = hold_lease(&s, &w).await;
    let mut m = LeanManifest { seq: 1, boundary_source: Some("declared".into()), ..Default::default() };
    m.tombstones.insert("a.txt".into(), flint_lean::manifest::Tombstone { etag: "\"gone\"".into(), seq: 1 });
    w.cas_manifest(&m, None, epoch, "writer").await.unwrap();
    let create = PutFile { if_none_match: Some("*".into()), ..Default::default() };
    w.put_file("a.txt", Bytes::from("back"), &create).await.unwrap();
    let doc = w.snapshot().await.unwrap().manifest;
    assert!(!doc.tombstones.contains_key("a.txt"), "the save left the path's tombstone");
    assert_eq!(doc.boundary_source, None, "a save inherited the writer's boundary stamp");
}

/// A save whose CAS loses to a writer's publish of ANOTHER path re-reads,
/// re-judges and commits onto it: both are cited.
#[tokio::test]
async fn a_save_that_loses_its_cas_to_another_path_commits_onto_it() {
    let mem = Arc::new(MemoryStore::new());
    let plain: Arc<dyn ObjectStore> = mem.clone();
    let w0 = ws(&plain);
    let epoch = hold_lease(&plain, &w0).await;
    let hook = Arc::new(AfterPut {
        inner: mem.clone(),
        trigger: w0.config().file_key("a.txt"),
        cfg: w0.config().clone(),
        epoch,
        armed: true.into(),
        fired: false.into(),
        hold: false.into(),
        path: "other.txt".into(),
        after_read: true.into(),
        race_pending: false.into(),
    });
    let s: Arc<dyn ObjectStore> = hook.clone();
    let w = ws(&s);
    let etag = w.put_file("a.txt", Bytes::from("mine"), &PutFile::default()).await.unwrap();
    assert!(hook.fired.load(std::sync::atomic::Ordering::SeqCst), "the fixture never raced the save");
    let doc = w.snapshot().await.unwrap().manifest;
    assert_eq!(doc.entries["a.txt"].etag, etag);
    assert!(doc.entries.contains_key("other.txt"), "the save's commit dropped the writer's publish");
}

#[tokio::test]
async fn a_write_is_durable_readable_and_cited_at_once() {
    let s = store();
    let w = ws(&s);
    let etag = w.put_file("a.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();

    let blob = w.get_file("a.txt").await.unwrap();
    assert_eq!(blob.body, Bytes::from("one"));
    assert_eq!(blob.etag, etag);

    // Object at a HANDLE of its own, CITED by the manifest the save
    // installed (P2), and nothing in the cell.
    let snap = w.snapshot().await.unwrap();
    let cited = &snap.manifest.entries["a.txt"];
    assert_eq!(cited.etag, etag);
    assert!(w.config().handle_parts(&cited.key).is_some(), "not a handle: {}", cited.key);
    assert!(s.head(&cited.key).await.is_ok());
    assert!(handles_of(&s, &w, "a.txt").await == vec![cited.key.clone()], "one handle, the write's");
    assert_eq!(snap.manifest.seq, 1);
    assert!(snap.manifest_etag.is_some());

    let st = w.status().await.unwrap();
    assert_eq!(st.seq, Some(1));
    assert_eq!(st.epoch, None, "no syncer holds this workspace, and none is needed");
}

#[tokio::test]
async fn an_overwrite_must_name_what_it_read() {
    let s = store();
    let w = ws(&s);
    let v1 = w.put_file("a.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();

    // No If-Match at all: refused, nothing written.
    let err = w.put_file("a.txt", Bytes::from("two"), &PutFile::default()).await.unwrap_err();
    assert!(matches!(err, VerbError::PreconditionRequired), "{err}");
    assert_eq!((err.status(), err.code()), (428, "precondition-required"));
    assert!(!err.is_retryable());
    assert_eq!(w.get_file("a.txt").await.unwrap().body, Bytes::from("one"));

    // A stale one: refused, and the refusal names the current tag.
    let stale = PutFile { if_match: Some("\"not-it\"".into()), ..Default::default() };
    let err = w.put_file("a.txt", Bytes::from("two"), &stale).await.unwrap_err();
    match &err {
        VerbError::FileChanged { current } => assert_eq!(current.as_deref(), Some(v1.as_str())),
        e => panic!("expected FileChanged, got {e}"),
    }
    assert_eq!(err.current_etag(), Some(v1.as_str()));
    assert_eq!((err.status(), err.code()), (412, "file-changed"));

    // The right one, quoted as S3 hands it out: accepted.
    let ok = PutFile { if_match: Some(v1.clone()), author: Some("dilip".into()), ..Default::default() };
    let v2 = w.put_file("a.txt", Bytes::from("two"), &ok).await.unwrap();
    assert_ne!(v2, v1);
    assert_eq!(w.get_file("a.txt").await.unwrap().body, Bytes::from("two"));

    // The bare form of the tag is judged the same as the quoted one.
    let bare = PutFile { if_match: Some(v2.trim_matches('"').to_string()), ..Default::default() };
    w.put_file("a.txt", Bytes::from("three"), &bare).await.unwrap();

    // `*` demands existence: a create over an existing file is refused.
    let create = PutFile { if_none_match: Some("*".into()), ..Default::default() };
    let err = w.put_file("a.txt", Bytes::from("four"), &create).await.unwrap_err();
    assert!(matches!(err, VerbError::FileChanged { current: Some(_) }), "{err}");
    // ...and a create where nothing exists succeeds.
    w.put_file("b.txt", Bytes::from("new"), &create).await.unwrap();

    // If-Match on a path that is not there is a stale caller.
    let err = w.put_file("c.txt", Bytes::from("x"), &ok).await.unwrap_err();
    assert!(matches!(err, VerbError::FileChanged { current: None }), "{err}");

    // Precondition shapes the verb refuses to guess at.
    let odd = PutFile { if_none_match: Some("\"e\"".into()), ..Default::default() };
    let err = w.put_file("d.txt", Bytes::from("x"), &odd).await.unwrap_err();
    assert!(matches!(err, VerbError::BadPrecondition(_)), "{err}");
    let both = PutFile { if_match: Some("*".into()), if_none_match: Some("*".into()), ..Default::default() };
    let err = w.put_file("d.txt", Bytes::from("x"), &both).await.unwrap_err();
    assert_eq!(err.code(), "bad-precondition");
}

#[tokio::test]
async fn path_hygiene_the_size_cap_and_a_missing_file() {
    let s = store();
    let w = ws(&s).with_max_put_bytes(4);
    // The last two are the consume's temp-sibling name (review 2026-09-18,
    // C1): the syncer's walk skips it at every depth, so an acked write
    // under it was cited once and then collected as a delete.
    for bad in ["../x", "/abs", "a//b", ".flint/x", ".flint", "a/./b", ".flint-sync/state",
                "notes/report.flint-sync-tmp", "a.flint-sync-tmp/b"] {
        let err = w.put_file(bad, Bytes::from("x"), &PutFile::default()).await.unwrap_err();
        assert!(matches!(err, VerbError::BadPath(_)), "{bad}: {err}");
        assert_eq!(err.status(), 400);
        let err = w.get_file(bad).await.unwrap_err();
        assert!(matches!(err, VerbError::BadPath(_)), "{bad}: {err}");
    }
    let err = w.put_file("ok.txt", Bytes::from("12345"), &PutFile::default()).await.unwrap_err();
    assert!(matches!(err, VerbError::TooLarge { size: 5, max: 4 }), "{err}");
    assert_eq!((err.status(), err.code()), (413, "payload-too-large"));
    assert!(
        s.head(&w.config().file_key("ok.txt")).await.is_err(),
        "a refused body must not have landed"
    );
    w.put_file("ok.txt", Bytes::from("1234"), &PutFile::default()).await.unwrap();

    let err = w.get_file("nope.txt").await.unwrap_err();
    assert!(matches!(err, VerbError::NoSuchFile(_)), "{err}");
    assert_eq!((err.status(), err.code()), (404, "no-such-file"));
}

/// A save is cited by the time it is acknowledged (P2), so a caller that
/// waits for the citation is answered at once, with the seq that cites it.
#[tokio::test]
async fn a_save_is_cited_when_acknowledged_and_wait_cited_answers_at_once() {
    let s = store();
    let w = ws(&s);
    let etag = w.put_file("a.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();
    let t0 = std::time::Instant::now();
    let seq = w
        .wait_cited("a.txt", &etag, Duration::from_secs(5), Duration::from_millis(50))
        .await
        .unwrap();
    assert!(t0.elapsed() < Duration::from_secs(1), "the wait polled for a citation that was already there");
    let snap = w.snapshot().await.unwrap();
    assert_eq!(seq, snap.manifest.seq);
    assert_eq!(snap.manifest.entries["a.txt"].etag, etag);
    assert_eq!(w.status().await.unwrap().seq, Some(seq));
    // The read resolves through the citation.
    assert_eq!(w.get_file("a.txt").await.unwrap().etag, etag);
}

/// A version nothing cites — here one that never existed — runs the
/// caller's clock down and answers 202 `citation-pending`: the bucket
/// cannot tell a citation that is coming from one that never will, so the
/// caller's timeout is the only bound.
#[tokio::test]
async fn wait_cited_for_a_version_nothing_cites_waits_out_its_timeout_and_answers_pending() {
    let s = store();
    let w = ws(&s);
    w.put_file("other.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();
    let t0 = std::time::Instant::now();
    let err = w
        .wait_cited("a.txt", "\"never-written\"", Duration::from_millis(400), Duration::from_millis(50))
        .await
        .unwrap_err();
    assert!(t0.elapsed() >= Duration::from_millis(400), "refused before the caller's timeout");
    assert_eq!((err.status(), err.code()), (202, "citation-pending"));
}

#[tokio::test]
async fn a_superseded_write_is_named_not_waited_for() {
    let s = store();
    let w = ws(&s);
    let epoch = hold_lease(&s, &w).await;
    let v1 = w.put_file("a.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();
    let v2 = w
        .put_file("a.txt", Bytes::from("two"), &PutFile { if_match: Some(v1.clone()), ..Default::default() })
        .await
        .unwrap();
    // The syncer consumed both entries and cited the later bytes.
    cite(&w, epoch, 1, "a.txt", &v2, b"two").await;

    let err = w
        .wait_cited("a.txt", &v1, Duration::from_secs(5), Duration::from_millis(50))
        .await
        .unwrap_err();
    match err {
        VerbError::Superseded { cited_etag, .. } => assert_eq!(cited_etag, v2),
        e => panic!("{e}"),
    }
    assert_eq!(
        w.wait_cited("a.txt", &v2, Duration::from_secs(5), Duration::from_millis(50)).await.unwrap(),
        1
    );
}


#[tokio::test]
async fn the_syncer_facing_verbs_are_epoch_validated() {
    let s = store();
    let w = ws(&s);

    // No cell at all.
    let err = w.cas_manifest(&LeanManifest::default(), None, 1, "u").await.unwrap_err();
    assert!(matches!(err, VerbError::NoHolder), "{err}");
    assert_eq!((err.status(), err.code()), (403, "no-holder"));

    let epoch = hold_lease(&s, &w).await;
    // A stale claim, and a claim from the future, both die here.
    for claimed in [epoch.wrapping_sub(1), epoch + 1] {
        let err = w.cas_manifest(&LeanManifest::default(), None, claimed, "u").await.unwrap_err();
        match &err {
            VerbError::StaleEpoch { cell_epoch, holder_id, claimed: c } => {
                assert_eq!(*cell_epoch, epoch);
                assert_eq!(holder_id, "syncer-1");
                assert_eq!(*c, claimed);
            }
            e => panic!("{e}"),
        }
        assert_eq!((err.status(), err.code()), (403, "stale-epoch"));
    }

    // The manifest CAS: a first write, then a miss against a stale handle.
    let etag = w.put_file("a.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();
    let h1 = cite(&w, epoch, 1, "a.txt", &etag, b"one").await;
    let mut m = LeanManifest { seq: 2, ..Default::default() };
    m.entries.insert("a.txt".into(), w.snapshot().await.unwrap().manifest.entries["a.txt"].clone());
    let err = w.cas_manifest(&m, Some("\"stale\""), epoch, "u2").await.unwrap_err();
    match &err {
        VerbError::CasMiss { current } => assert_eq!(current.as_deref(), Some(h1.as_str())),
        e => panic!("{e}"),
    }
    assert_eq!((err.status(), err.code()), (409, "cas-miss"));
    assert_eq!(w.snapshot().await.unwrap().manifest.seq, 1, "a miss changes nothing");
}

#[tokio::test]
async fn a_boundary_request_is_recorded_never_performed() {
    let s = store();
    let w = ws(&s);
    let a = w.request_boundary(Some("dilip")).await.unwrap();
    assert_eq!(a.status, "recorded");
    assert_eq!(a.verb, "boundary");
    assert_eq!(a.requestor, "dilip");
    let st = w.status().await.unwrap();
    assert_eq!(st.boundary_request.as_ref().map(|r| r.requestor.as_str()), Some("dilip"));
    assert!(st.sync_request.is_none());
    assert_eq!(st.seq, None, "nothing was published by asking");

    let a = w.request_sync(None).await.unwrap();
    assert_eq!(a.verb, "sync");
    assert_eq!(a.requestor, "gateway");
    assert!(a.note.contains("CARRIED"));
    assert!(w.status().await.unwrap().sync_request.is_some());
}

#[tokio::test]
async fn a_draft_is_private_until_promoted_and_promote_holds_the_recorded_base() {
    let s = store();
    let w = ws(&s);
    let v1 = w.put_file("doc.md", Bytes::from("v1"), &PutFile::default()).await.unwrap();

    // Save against v1. Nothing about the file changes.
    let body_etag = w
        .put_draft("alice", "doc.md", Bytes::from("alice's edit"), None, Some(&v1))
        .await
        .unwrap();
    assert_eq!(w.get_file("doc.md").await.unwrap().body, Bytes::from("v1"));
    assert_eq!(w.snapshot().await.unwrap().manifest.entries["doc.md"].etag, v1, "a draft is not published");

    let d = w.get_draft("alice", "doc.md").await.unwrap();
    assert_eq!(d.body, Bytes::from("alice's edit"));
    assert_eq!(d.etag, body_etag);
    assert_eq!(d.base_etag.as_deref(), Some(v1.as_str()));
    assert!(!d.incomplete);

    // Per user: bob has none.
    assert!(w.list_drafts("bob").await.unwrap().is_empty());
    assert!(matches!(w.get_draft("bob", "doc.md").await.unwrap_err(), VerbError::NoDraft(_)));

    let rows = w.list_drafts("alice").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, "doc.md");
    assert_eq!(rows[0].author, "alice", "no author ⇒ the user");
    assert!(!rows[0].stale);

    // A sibling publishes: the resume view says stale, promote refuses
    // and KEEPS the draft.
    let v2 = w
        .put_file("doc.md", Bytes::from("v2"), &PutFile { if_match: Some(v1.clone()), ..Default::default() })
        .await
        .unwrap();
    assert!(w.list_drafts("alice").await.unwrap()[0].stale);
    let err = w.promote_draft("alice", "doc.md", None).await.unwrap_err();
    match &err {
        VerbError::DraftStale { current, message } => {
            assert_eq!(current.as_deref(), Some(v2.as_str()));
            assert!(message.contains("KEPT"), "{message}");
        }
        e => panic!("{e}"),
    }
    assert_eq!((err.status(), err.code()), (409, "draft-stale"));
    assert_eq!(err.current_etag(), Some(v2.as_str()));
    assert!(w.get_draft("alice", "doc.md").await.is_ok(), "the draft survives the refusal");
    assert_eq!(w.get_file("doc.md").await.unwrap().body, Bytes::from("v2"));

    // Re-saved against v2, the promote publishes and the draft is gone.
    w.put_draft("alice", "doc.md", Bytes::from("alice's edit 2"), Some("alice@x"), Some(&v2))
        .await
        .unwrap();
    let v3 = w.promote_draft("alice", "doc.md", None).await.unwrap();
    assert_ne!(v3, v2);
    assert_eq!(w.get_file("doc.md").await.unwrap().body, Bytes::from("alice's edit 2"));
    // A promote COMMITS, as a save does (P2). The author is not recorded:
    // a manifest entry has no author, and the cell that carried one is not
    // written any more (open item, 2026-09-25).
    let snap = w.snapshot().await.unwrap();
    assert_eq!(snap.manifest.entries["doc.md"].etag, v3, "a promote is cited when it returns");
    assert!(w.list_drafts("alice").await.unwrap().is_empty());
    assert!(matches!(w.get_draft("alice", "doc.md").await.unwrap_err(), VerbError::NoDraft(_)));

    // A draft with no base creates, and refuses to clobber a file that
    // appeared meanwhile.
    w.put_draft("alice", "new.md", Bytes::from("fresh"), None, None).await.unwrap();
    w.put_file("new.md", Bytes::from("someone else"), &PutFile::default()).await.unwrap();
    let err = w.promote_draft("alice", "new.md", None).await.unwrap_err();
    assert!(matches!(err, VerbError::DraftStale { current: Some(_), .. }), "{err}");
    w.delete_draft("alice", "new.md").await.unwrap();
    w.delete_draft("alice", "new.md").await.unwrap();
    assert!(matches!(w.get_draft("alice", "new.md").await.unwrap_err(), VerbError::NoDraft(_)));

    // Hygiene on the user segment.
    for bad in ["", "a/b", "..", "a\\b"] {
        let err = w.put_draft(bad, "x.md", Bytes::from("x"), None, None).await.unwrap_err();
        assert!(matches!(err, VerbError::BadUser(_)), "{bad:?}: {err}");
        assert_eq!((err.status(), err.code()), (400, "bad-user"));
    }
}

#[tokio::test]
async fn ten_workspaces_share_one_store_and_never_see_each_other() {
    let s = store();
    let all: Vec<Workspace> =
        (0..10).map(|i| Workspace::new(s.clone(), &format!("teams/t{i}"))).collect();
    for (i, w) in all.iter().enumerate() {
        w.put_file("shared.txt", Bytes::from(format!("ws {i}")), &PutFile::default()).await.unwrap();
    }
    for (i, w) in all.iter().enumerate() {
        assert_eq!(w.get_file("shared.txt").await.unwrap().body, Bytes::from(format!("ws {i}")));
        let snap = w.snapshot().await.unwrap();
        assert_eq!(snap.manifest.entries.len(), 1, "a workspace's document cites only its own save");
        assert_eq!(snap.manifest.seq, 1);
        assert_eq!(w.prefix(), format!("teams/t{i}"));
    }
    // A trailing slash on the prefix is dropped, so `teams/t0/` IS `teams/t0`.
    let alias = Workspace::new(s.clone(), "teams/t0/");
    assert_eq!(alias.get_file("shared.txt").await.unwrap().body, Bytes::from("ws 0"));
}

// ── delete and rename (docs/plans/flint-lean-delete-rename-design.md) ──

/// P2, slice 2: a delete COMMITS — one manifest CAS, no cell, no syncer.
/// The document stops citing the path and its tombstone names what was
/// deleted; a batch is one CAS and refused whole; a stale precondition or
/// an unknown path changes nothing.
#[tokio::test]
async fn a_delete_commits_in_one_cas_and_a_batch_is_refused_whole() {
    let s = store();
    let w = ws(&s);
    let a = w.put_file("a.txt", Bytes::from("aaa"), &PutFile::default()).await.unwrap();
    let b = w.put_file("b.txt", Bytes::from("bbb"), &PutFile::default()).await.unwrap();
    let seq = w.snapshot().await.unwrap().manifest.seq;

    let err = w.remove_file("a.txt", Some("dilip"), Some("\"stale\"")).await.unwrap_err();
    assert!(matches!(&err, VerbError::FileChanged { current: Some(c) } if c == &a), "{err}");
    let err = w.remove_files(&[("a.txt", None), ("nope.txt", None)], None).await.unwrap_err();
    assert!(matches!(err, VerbError::NoSuchFile(_)), "{err}");
    assert_eq!(w.snapshot().await.unwrap().manifest.seq, seq, "a refused delete committed");

    w.remove_files(&[("a.txt", Some(a.as_str())), ("b.txt", None)], Some("dilip")).await.unwrap();
    let snap = w.snapshot().await.unwrap();
    assert_eq!(snap.manifest.seq, seq + 1, "a batch is ONE commit");
    assert!(snap.manifest.entries.is_empty());
    assert_eq!(snap.manifest.tombstones["a.txt"].etag, a);
    assert_eq!(snap.manifest.tombstones["b.txt"].etag, b);
    assert!(matches!(w.get_file("a.txt").await.unwrap_err(), VerbError::NoSuchFile(_)));
}

/// P2, slice 2: a rename COMMITS — one manifest CAS moves the citation, so
/// the destination names the source's handle and no bytes move. It never
/// waits on a barrier window (G1). A destination that exists refuses.
#[tokio::test]
async fn a_rename_commits_in_one_cas_even_while_a_writer_holds_the_lease() {
    let s = store();
    let w = ws(&s);
    hold_lease(&s, &w).await;
    let a = w.put_file("a.txt", Bytes::from("aaa"), &PutFile::default()).await.unwrap();
    w.put_file("taken.txt", Bytes::from("t"), &PutFile::default()).await.unwrap();
    let before = w.snapshot().await.unwrap().manifest;

    let err = w.rename_file("a.txt", "taken.txt", None).await.unwrap_err();
    assert!(matches!(err, VerbError::DestinationExists { .. }), "{err}");

    let etag = w.rename_file("a.txt", "dir/b.txt", Some("dilip")).await.unwrap();
    assert_eq!(etag, a);
    let snap = w.snapshot().await.unwrap();
    assert_eq!(snap.manifest.seq, before.seq + 1, "a rename is ONE commit");
    assert!(!snap.manifest.entries.contains_key("a.txt"));
    assert_eq!(snap.manifest.entries["dir/b.txt"].key, before.entries["a.txt"].key, "the handle moved");
    assert_eq!(snap.manifest.tombstones["a.txt"].etag, a);
    assert_eq!(w.get_file("dir/b.txt").await.unwrap().body, Bytes::from("aaa"));
}


/// A rename is refused whole before anything changes, and otherwise moves
/// the citation in ONE commit (P2): the destination names the source's
/// handle, nothing is copied, the source reads as gone at once — {source}
/// or {destination}, never both. A batch is one commit too.
#[tokio::test]
async fn a_rename_moves_the_citation_in_one_commit_and_refuses_what_it_must() {
    let s = store();
    let w = ws(&s);
    let body = Bytes::from("the same bytes");
    let etag_a = w.put_file("notes/a.txt", body.clone(), &PutFile::default()).await.unwrap();
    let etag_taken = w.put_file("taken.txt", Bytes::from("t"), &PutFile::default()).await.unwrap();

    assert!(matches!(w.rename_file("notes/a.txt", "notes/a.txt", None).await.unwrap_err(), VerbError::BadPath(_)));
    assert!(matches!(w.rename_file("nope.txt", "x.txt", None).await.unwrap_err(), VerbError::NoSuchFile(_)));
    let err = w.rename_file("notes/a.txt", "taken.txt", None).await.unwrap_err();
    match &err {
        VerbError::DestinationExists { path, current } => {
            assert_eq!(path, "taken.txt");
            assert_eq!(current.as_deref(), Some(etag_taken.as_str()));
        }
        e => panic!("{e}"),
    }
    assert_eq!((err.status(), err.code()), (409, "destination-exists"));

    let src_key = tracked_key(&w, "notes/a.txt").await;
    let etag_b = w.rename_file("notes/a.txt", "docs/b.txt", Some("dilip")).await.unwrap();
    assert_eq!(etag_b, etag_a);
    let blob = w.get_file("docs/b.txt").await.unwrap();
    assert_eq!((blob.body, blob.etag), (body.clone(), etag_b));
    let snap = w.snapshot().await.unwrap();
    let listed: Vec<String> = snap.listing().into_iter().map(|l| l.path).collect();
    assert_eq!(listed, vec!["docs/b.txt".to_string(), "taken.txt".to_string()]);
    let cited = &snap.manifest.entries["docs/b.txt"];
    assert_eq!(cited.key, src_key, "the destination does not name the source's handle");
    assert_eq!(cited.crc64_b64, crc64_to_b64(crc64_nvme(&body)), "the bytes' own CRC");
    assert!(handles_of(&s, &w, "docs/b.txt").await.is_empty(), "a rename moved bytes");
    assert!(matches!(w.get_file("notes/a.txt").await.unwrap_err(), VerbError::NoSuchFile(_)), "the source still reads");

    // A batch: a duplicate destination refuses the whole batch; two pairs
    // are ONE commit.
    w.put_file("c1", Bytes::from("1"), &PutFile::default()).await.unwrap();
    w.put_file("c2", Bytes::from("2"), &PutFile::default()).await.unwrap();
    assert!(matches!(w.rename_files(&[("c1", "d1"), ("c2", "d1")], None).await.unwrap_err(), VerbError::BadPath(_)));
    let seq = w.snapshot().await.unwrap().manifest.seq;
    let etags = w.rename_files(&[("c1", "d1"), ("c2", "d2")], None).await.unwrap();
    assert_eq!(etags.len(), 2);
    let snap = w.snapshot().await.unwrap();
    assert_eq!(snap.manifest.seq, seq + 1, "a batch is ONE commit");
    assert!(!snap.manifest.entries.contains_key("c1") && !snap.manifest.entries.contains_key("c2"));
    assert_eq!(w.get_file("d2").await.unwrap().body, Bytes::from("2"));
}

/// A rename is a CITATION MOVE (design 2026-09-19, R6): the destination's
/// entry names the source's handle and nothing is copied or minted. The
/// copy's 412 arms — its own orphan overwritten, a stranger's object
/// refused — went with the copy. What a rename still refuses is a
/// destination the workspace TRACKS, naming the version; an outside
/// writer's object at the destination's bare path is ingress, judged by
/// the sweep in its own time, and is neither in the way nor touched.
#[tokio::test]
async fn a_rename_moves_no_bytes_and_a_tracked_destination_refuses() {
    use flint_lean_gateway::crc64_nvme;
    use flint_store::{GenerationStamps, PutCondition};
    let s = store();
    let w = ws(&s);
    let body = Bytes::from("moved bytes");
    w.put_file("a.txt", body.clone(), &PutFile::default()).await.unwrap();
    w.put_file("s.txt", Bytes::from("second source"), &PutFile::default()).await.unwrap();
    let b = Bytes::from_static(b"an outside writer's bytes");
    let crc = crc64_nvme(&b);
    let stamps = GenerationStamps { generation: 1, epoch: 0, flush_uuid: "aws-cli".into(), boundary_source: None, posix: None };
    let ingress = s
        .put_whole(&w.config().file_key("b.txt"), b, &PutCondition::Unconditional, &stamps, crc)
        .await
        .unwrap()
        .etag;
    let src = tracked_key(&w, "a.txt").await;
    w.rename_file("a.txt", "b.txt", None).await.unwrap();
    assert_eq!(w.get_file("b.txt").await.unwrap().body, body);
    assert_eq!(tracked_key(&w, "b.txt").await, src, "the destination does not name the source's handle");
    assert!(handles_of(&s, &w, "b.txt").await.is_empty(), "a rename minted or copied a handle");
    let (at_bare, _) = s.get_whole(&w.config().file_key("b.txt"), None).await.unwrap();
    assert_eq!(at_bare.etag, ingress, "the ingress object was touched");

    let theirs = w.put_file("c.txt", Bytes::from("someone else's"), &PutFile::default()).await.unwrap();
    let err = w.rename_file("s.txt", "c.txt", None).await.unwrap_err();
    match &err {
        VerbError::DestinationExists { current, .. } => assert_eq!(current.as_deref(), Some(theirs.as_str())),
        e => panic!("{e}"),
    }
    assert_eq!(w.get_file("c.txt").await.unwrap().body, Bytes::from("someone else's"));
}


/// An overwrite of a cited file is CITED when it returns (P2): every reader
/// sees it, the listing agrees, and a writer's later publish over it reads
/// at once too.
#[tokio::test]
async fn an_overwrite_of_a_cited_file_is_cited_at_once_and_a_later_publish_reads_at_once() {
    use flint_store::{GenerationStamps, PutCondition};
    let s = store();
    let w = ws(&s);
    let epoch = hold_lease(&s, &w).await;
    let v1 = w.put_file("a.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();
    let v2 = w
        .put_file("a.txt", Bytes::from("two"), &PutFile { if_match: Some(v1.clone()), ..Default::default() })
        .await
        .unwrap();
    let blob = w.get_file("a.txt").await.unwrap();
    assert_eq!((blob.etag.as_str(), &blob.body[..]), (v2.as_str(), &b"two"[..]));
    let snap = w.snapshot().await.unwrap();
    assert_eq!(snap.listing().iter().find(|l| l.path == "a.txt").unwrap().etag, v2);
    assert_eq!(snap.manifest.entries["a.txt"].etag, v2, "the save is the citation");

    let key = w.config().handle_key("a.txt", "agent-publish");
    let three = Bytes::from("three");
    let crc = crc64_nvme(&three);
    let stamps = GenerationStamps {
        generation: 2,
        epoch,
        flush_uuid: "agent-publish".into(),
        boundary_source: None,
        posix: None,
    };
    let v3 = s.put_whole(&key, three, &PutCondition::IfNoneMatchAny, &stamps, crc).await.unwrap().etag;
    let mut m = w.snapshot().await.unwrap().manifest;
    m.seq += 1;
    let e = m.entries.get_mut("a.txt").unwrap();
    e.key = key;
    e.etag = v3.clone();
    e.crc64_b64 = crc64_to_b64(crc);
    e.size = 5;
    let current = w.snapshot().await.unwrap().manifest_etag;
    w.cas_manifest(&m, current.as_deref(), epoch, "agent-publish").await.unwrap();
    let blob = w.get_file("a.txt").await.unwrap();
    assert_eq!((blob.etag.as_str(), &blob.body[..]), (v3.as_str(), &b"three"[..]));
}

/// A store that remembers every `get_whole` key, so a test can say
/// what a read COST in requests rather than time it.
struct CountGets {
    inner: Arc<MemoryStore>,
    gets: std::sync::Mutex<Vec<String>>,
}

impl CountGets {
    fn new(inner: Arc<MemoryStore>) -> Self {
        Self { inner, gets: Default::default() }
    }
    /// Every key fetched since the last drain or take.
    fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *self.gets.lock().unwrap())
    }
    /// The keys fetched since the last take: (inbox cell, file objects).
    fn take(&self) -> (usize, usize) {
        let keys = std::mem::take(&mut *self.gets.lock().unwrap());
        let cell = keys.iter().filter(|k| k.ends_with("/inbox")).count();
        let files = keys.iter().filter(|k| k.contains("/files/")).count();
        (cell, files)
    }
}

#[async_trait::async_trait]
impl ObjectStore for CountGets {
    async fn copy_object(
        &self,
        src_key: &str,
        src_if_match: Option<&str>,
        dst_key: &str,
        condition: &flint_store::PutCondition,
        stamps: &flint_store::GenerationStamps,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.copy_object(src_key, src_if_match, dst_key, condition, stamps).await
    }
    async fn put_whole(
        &self,
        key: &str,
        body: Bytes,
        cond: &flint_store::PutCondition,
        stamps: &flint_store::GenerationStamps,
        crc: u64,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.put_whole(key, body, cond, stamps, crc).await
    }
    async fn compose_generation(
        &self,
        spec: &flint_store::ComposeSpec<'_>,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.compose_generation(spec).await
    }
    async fn head(&self, key: &str) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.head(key).await
    }
    async fn get_whole(
        &self,
        key: &str,
        if_match: Option<&str>,
    ) -> flint_store::StoreResult<(flint_store::ObjectMeta, Bytes)> {
        self.gets.lock().unwrap().push(key.to_string());
        self.inner.get_whole(key, if_match).await
    }
    async fn get_range(
        &self,
        key: &str,
        off: u64,
        len: u64,
        if_match: &str,
    ) -> flint_store::StoreResult<Bytes> {
        self.inner.get_range(key, off, len, if_match).await
    }
    fn min_part_size(&self) -> u64 {
        self.inner.min_part_size()
    }
    fn max_parts(&self) -> usize {
        self.inner.max_parts()
    }
    async fn list(&self, prefix: &str) -> flint_store::StoreResult<Vec<flint_store::ListedObject>> {
        self.inner.list(prefix).await
    }
    async fn delete(&self, key: &str) -> flint_store::StoreResult<()> {
        self.inner.delete(key).await
    }
    async fn delete_if_match(&self, key: &str, etag: &str) -> flint_store::StoreResult<()> {
        self.inner.delete_if_match(key, etag).await
    }
    async fn head_version(
        &self,
        key: &str,
        v: &str,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.head_version(key, v).await
    }
    async fn get_version(
        &self,
        key: &str,
        v: &str,
    ) -> flint_store::StoreResult<(flint_store::ObjectMeta, Bytes)> {
        self.inner.get_version(key, v).await
    }
    async fn delete_version(&self, key: &str, v: &str) -> flint_store::StoreResult<()> {
        self.inner.delete_version(key, v).await
    }
    async fn list_versions(
        &self,
        prefix: &str,
    ) -> flint_store::StoreResult<Vec<flint_store::ListedVersion>> {
        self.inner.list_versions(prefix).await
    }
    async fn list_uploads(
        &self,
        prefix: &str,
    ) -> flint_store::StoreResult<Vec<flint_store::PendingUpload>> {
        self.inner.list_uploads(prefix).await
    }
    async fn abort_upload(&self, key: &str, id: &str) -> flint_store::StoreResult<()> {
        self.inner.abort_upload(key, id).await
    }
    async fn bootstrap(
        &self,
        prefix: &str,
    ) -> flint_store::StoreResult<flint_store::BootstrapReport> {
        self.inner.bootstrap(prefix).await
    }
    async fn epoch_read(
        &self,
        key: &str,
    ) -> flint_store::StoreResult<Option<flint_store::EpochState>> {
        self.inner.epoch_read(key).await
    }
    async fn epoch_acquire(
        &self,
        key: &str,
        holder: &str,
        observed: Option<&flint_store::EpochState>,
    ) -> flint_store::StoreResult<flint_store::EpochLease> {
        self.inner.epoch_acquire(key, holder, observed).await
    }
    async fn epoch_renew(
        &self,
        key: &str,
        lease: &flint_store::EpochLease,
        echo: Option<&str>,
    ) -> flint_store::StoreResult<flint_store::EpochLease> {
        self.inner.epoch_renew(key, lease, echo).await
    }
    async fn epoch_release(
        &self,
        key: &str,
        lease: &flint_store::EpochLease,
    ) -> flint_store::StoreResult<()> {
        self.inner.epoch_release(key, lease).await
    }
}

/// WHAT THE COMMON READ COSTS: one fetch of the bytes, by their handle, and
/// no fetch of the cell — for a saved file, an overwritten one, and one a
/// writer published. Every UI verb commits to the manifest when it returns
/// (P2), so the cell holds nothing a read must overlay.
#[tokio::test]
async fn a_read_costs_one_object_fetch_and_no_cell_fetch() {
    use flint_store::{GenerationStamps, PutCondition};
    let counting = Arc::new(CountGets::new(Arc::new(MemoryStore::new())));
    let s: Arc<dyn ObjectStore> = counting.clone();
    let w = ws(&s);
    let epoch = hold_lease(&s, &w).await;
    let v1 = w.put_file("a.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();
    counting.take();
    assert_eq!(w.get_file("a.txt").await.unwrap().etag, v1);
    assert_eq!(counting.take(), (0, 1), "(inbox fetches, object fetches) for the common read");

    let v2 = w
        .put_file("a.txt", Bytes::from("two"), &PutFile { if_match: Some(v1.clone()), ..Default::default() })
        .await
        .unwrap();
    counting.take();
    assert_eq!(w.get_file("a.txt").await.unwrap().etag, v2);
    assert_eq!(counting.take(), (0, 1), "(inbox fetches, object fetches) for an overwritten read");

    // A writer publishes over it, at a handle of its own.
    let key = w.config().handle_key("a.txt", "agent-publish");
    let three = Bytes::from("three");
    let crc = crc64_nvme(&three);
    let stamps = GenerationStamps {
        generation: 2,
        epoch,
        flush_uuid: "agent-publish".into(),
        boundary_source: None,
        posix: None,
    };
    let v3 = s.put_whole(&key, three, &PutCondition::IfNoneMatchAny, &stamps, crc).await.unwrap().etag;
    let mut m = w.snapshot().await.unwrap().manifest;
    m.seq += 1;
    let e = m.entries.get_mut("a.txt").unwrap();
    e.key = key;
    e.etag = v3.clone();
    e.crc64_b64 = crc64_to_b64(crc);
    e.size = 5;
    let current = w.snapshot().await.unwrap().manifest_etag;
    w.cas_manifest(&m, current.as_deref(), epoch, "agent-publish").await.unwrap();
    counting.take();
    assert_eq!(w.get_file("a.txt").await.unwrap().etag, v3);
    assert_eq!(counting.take(), (0, 1), "(inbox fetches, object fetches) for a writer's publish");
}

/// Retired class F8/L-16 (`LeanImmutableHitlOverAny`): a UI write beside a
/// writer's upload its commit has not cited yet. Under the slot the write
/// was refused 409 `concurrent-write` until the commit cited the upload,
/// and a dead writer's orphan blocked the path for the grace. Under
/// handles (design 2026-09-19, R1) the two never meet: the upload sits at
/// a handle of its own, the UI's write lands at another with no
/// condition, the cell tracks it, and the writer's next consume judges it
/// — nothing refused, nothing overwritten, no grace to wait out. The
/// caller's precondition is judged against what the workspace TRACKS.
#[tokio::test]
async fn a_ui_write_beside_an_uncited_upload_lands_at_its_own_handle_and_is_tracked() {
    let s = store();
    let w = ws(&s);
    let epoch = hold_lease(&s, &w).await;
    let seed = w.put_file("a.txt", Bytes::from("seed"), &PutFile::default()).await.unwrap();

    // A syncer's upload lands at a handle of its own and is not cited yet.
    let flush = "0f8a4c2e-3b6d-4a1e-9c7b-5d2e8f1a6b3c"; // a barrier's flush id
    let key = w.config().handle_key("a.txt", flush);
    let body = Bytes::from("agent upload, not yet cited");
    let uploaded = s
        .put_whole(
            &key,
            body.clone(),
            &flint_store::PutCondition::IfNoneMatchAny,
            &flint_store::GenerationStamps { generation: 2, epoch, flush_uuid: flush.into(), boundary_source: None, posix: None },
            crc64_nvme(&body),
        )
        .await
        .unwrap();

    // The UI overwrites the version it READ — the cited one — with force
    // or by etag: each lands at a handle of the write's own and is cited
    // (P2), and the upload is untouched. The upload's etag names nothing
    // the workspace cites, so a caller naming it is told what the file is.
    let mut read = seed.clone();
    for if_match in ["*".to_string(), "read".to_string()] {
        let if_match = if if_match == "read" { read.clone() } else { if_match };
        let opts = PutFile { if_match: Some(if_match.clone()), ..Default::default() };
        let etag = w.put_file("a.txt", Bytes::from("from the UI"), &opts).await.unwrap_or_else(|e| panic!("If-Match {if_match}: {e}"));
        assert_eq!(s.head(&key).await.unwrap().etag, uploaded.etag, "If-Match {if_match} touched the upload");
        assert_eq!(w.get_file("a.txt").await.unwrap().etag, etag);
        read = etag;
    }
    let opts = PutFile { if_match: Some(uploaded.etag.clone()), ..Default::default() };
    let err = w.put_file("a.txt", Bytes::from("from the UI"), &opts).await.unwrap_err();
    assert!(matches!(err, VerbError::FileChanged { .. }), "an etag the workspace does not track: {err}");
    let handles = handles_of(&s, &w, "a.txt").await;
    assert!(handles.contains(&key), "the upload is gone: {handles:?}");
    assert!(handles.len() >= 3, "the UI's writes did not land at handles of their own: {handles:?}");
}


/// A barrier that opens its window the moment a PUT LANDS on `trigger`:
/// after the gateway's admission check and its object PUT, before its
/// inbox append. The order is the hook's, never a timer's.
struct AfterPut {
    inner: Arc<MemoryStore>,
    trigger: String,
    cfg: flint_lean::LeanConfig,
    epoch: u64,
    armed: std::sync::atomic::AtomicBool,
    fired: std::sync::atomic::AtomicBool,
    /// Park the save after its PUT instead of publishing (the caller
    /// then goes away).
    hold: std::sync::atomic::AtomicBool,
    /// The workspace path the fixture's writer publishes.
    path: String,
    /// Publish AFTER the save's next read of the document returns (between
    /// its read and its CAS), not right after its PUT.
    after_read: std::sync::atomic::AtomicBool,
    race_pending: std::sync::atomic::AtomicBool,
}

/// A writer's publish of `path`: bytes at a handle of its own, one CAS.
async fn publish_as_writer(store: &dyn ObjectStore, cfg: &flint_lean::LeanConfig, epoch: u64, path: &str, body: &[u8]) -> String {
    let key = cfg.handle_key(path, "writer-flush");
    let bytes = Bytes::copy_from_slice(body);
    let crc = crc64_nvme(&bytes);
    let stamps = flint_store::GenerationStamps { generation: 9, epoch, flush_uuid: "writer-flush".into(), boundary_source: None, posix: None };
    let meta = store.put_whole(&key, bytes, &flint_store::PutCondition::IfNoneMatchAny, &stamps, crc).await.unwrap();
    let current = flint_lean::manifest::load(store, cfg).await.unwrap();
    let mut doc = current.as_ref().map(|l| l.manifest.clone()).unwrap_or_default();
    doc.seq += 1;
    doc.entries.insert(path.to_string(), LeanEntry {
        key, etag: meta.etag.clone(), crc64_b64: crc64_to_b64(crc), size: body.len() as u64,
        mode: 0o644, mtime_unix: 0, generation: 9, epoch,
    });
    flint_lean::manifest::cas_write(store, cfg, &doc, current.as_ref().map(|l| l.handle()).as_ref(), epoch, "writer-flush")
        .await
        .expect("the fixture's writer publishes");
    meta.etag
}

#[async_trait::async_trait]
impl ObjectStore for AfterPut {
    async fn copy_object(
        &self,
        src_key: &str,
        src_if_match: Option<&str>,
        dst_key: &str,
        condition: &flint_store::PutCondition,
        stamps: &flint_store::GenerationStamps,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.copy_object(src_key, src_if_match, dst_key, condition, stamps).await
    }
    async fn put_whole(
        &self,
        key: &str,
        body: Bytes,
        cond: &flint_store::PutCondition,
        stamps: &flint_store::GenerationStamps,
        crc: u64,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        let landed = self.inner.put_whole(key, body, cond, stamps, crc).await?;
        // The trigger names the path; a PUT lands at a HANDLE of it.
        let hit = key == self.trigger || key.rsplit_once('@').map(|(p, _)| p == self.trigger).unwrap_or(false);
        if hit && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.fired.store(true, std::sync::atomic::Ordering::SeqCst);
            if self.hold.load(std::sync::atomic::Ordering::SeqCst) {
                // The save is parked after its PUT for good: the only way
                // out is for its caller to go away.
                std::future::pending::<()>().await;
            }
            if self.after_read.load(std::sync::atomic::Ordering::SeqCst) {
                self.race_pending.store(true, std::sync::atomic::Ordering::SeqCst);
            } else {
                // A writer publishes between the save's PUT and its CAS.
                publish_as_writer(self.inner.as_ref(), &self.cfg, self.epoch, &self.path, b"the writer's").await;
            }
        }
        Ok(landed)
    }
    async fn compose_generation(
        &self,
        spec: &flint_store::ComposeSpec<'_>,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.compose_generation(spec).await
    }
    async fn head(&self, key: &str) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.head(key).await
    }
    async fn get_whole(
        &self,
        key: &str,
        if_match: Option<&str>,
    ) -> flint_store::StoreResult<(flint_store::ObjectMeta, Bytes)> {
        let got = self.inner.get_whole(key, if_match).await;
        if key == self.cfg.current_key() && self.race_pending.swap(false, std::sync::atomic::Ordering::SeqCst) {
            // The save has read the pointer; a writer's CAS lands before its own.
            publish_as_writer(self.inner.as_ref(), &self.cfg, self.epoch, &self.path, b"the writer's").await;
        }
        got
    }
    async fn get_range(
        &self,
        key: &str,
        off: u64,
        len: u64,
        if_match: &str,
    ) -> flint_store::StoreResult<Bytes> {
        self.inner.get_range(key, off, len, if_match).await
    }
    fn min_part_size(&self) -> u64 {
        self.inner.min_part_size()
    }
    fn max_parts(&self) -> usize {
        self.inner.max_parts()
    }
    async fn list(&self, prefix: &str) -> flint_store::StoreResult<Vec<flint_store::ListedObject>> {
        self.inner.list(prefix).await
    }
    async fn delete(&self, key: &str) -> flint_store::StoreResult<()> {
        self.inner.delete(key).await
    }
    async fn delete_if_match(&self, key: &str, etag: &str) -> flint_store::StoreResult<()> {
        self.inner.delete_if_match(key, etag).await
    }
    async fn head_version(
        &self,
        key: &str,
        v: &str,
    ) -> flint_store::StoreResult<flint_store::ObjectMeta> {
        self.inner.head_version(key, v).await
    }
    async fn get_version(
        &self,
        key: &str,
        v: &str,
    ) -> flint_store::StoreResult<(flint_store::ObjectMeta, Bytes)> {
        self.inner.get_version(key, v).await
    }
    async fn delete_version(&self, key: &str, v: &str) -> flint_store::StoreResult<()> {
        self.inner.delete_version(key, v).await
    }
    async fn list_versions(
        &self,
        prefix: &str,
    ) -> flint_store::StoreResult<Vec<flint_store::ListedVersion>> {
        self.inner.list_versions(prefix).await
    }
    async fn list_uploads(
        &self,
        prefix: &str,
    ) -> flint_store::StoreResult<Vec<flint_store::PendingUpload>> {
        self.inner.list_uploads(prefix).await
    }
    async fn abort_upload(&self, key: &str, id: &str) -> flint_store::StoreResult<()> {
        self.inner.abort_upload(key, id).await
    }
    async fn bootstrap(
        &self,
        prefix: &str,
    ) -> flint_store::StoreResult<flint_store::BootstrapReport> {
        self.inner.bootstrap(prefix).await
    }
    async fn epoch_read(
        &self,
        key: &str,
    ) -> flint_store::StoreResult<Option<flint_store::EpochState>> {
        self.inner.epoch_read(key).await
    }
    async fn epoch_acquire(
        &self,
        key: &str,
        holder: &str,
        observed: Option<&flint_store::EpochState>,
    ) -> flint_store::StoreResult<flint_store::EpochLease> {
        self.inner.epoch_acquire(key, holder, observed).await
    }
    async fn epoch_renew(
        &self,
        key: &str,
        lease: &flint_store::EpochLease,
        echo: Option<&str>,
    ) -> flint_store::StoreResult<flint_store::EpochLease> {
        self.inner.epoch_renew(key, lease, echo).await
    }
    async fn epoch_release(
        &self,
        key: &str,
        lease: &flint_store::EpochLease,
    ) -> flint_store::StoreResult<()> {
        self.inner.epoch_release(key, lease).await
    }
}


/// FINDING 12's guarantee under P2: a save refused AFTER its PUT never
/// destroys what the workspace cites. A writer publishes the path between
/// the save's PUT and its CAS; the save's precondition no longer holds at
/// the CAS, so it is refused `FileChanged` naming the writer's version,
/// which stays cited and readable. The save's handle is an orphan.
/// (Before P2 this was a window opening after the PUT and the inbox append
/// refused; a save no longer appends, and a window no longer gates it.)
#[tokio::test]
async fn a_save_refused_after_its_put_never_destroys_what_the_workspace_cites() {
    const HOLD: bool = false;
    let mem = Arc::new(MemoryStore::new());
    let plain: Arc<dyn ObjectStore> = mem.clone();
    let w0 = ws(&plain);
    let epoch = hold_lease(&plain, &w0).await;
    let hook = Arc::new(AfterPut {
        inner: mem.clone(),
        trigger: w0.config().file_key("a.txt"),
        cfg: w0.config().clone(),
        epoch,
        armed: false.into(),
        fired: false.into(),
        hold: HOLD.into(),
        path: "a.txt".into(),
        after_read: false.into(),
        race_pending: false.into(),
    });
    let s: Arc<dyn ObjectStore> = hook.clone();
    let w = ws(&s);
    let acked = w.put_file("a.txt", Bytes::from("acked"), &PutFile::default()).await.unwrap();
    hook.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let edit = PutFile { if_match: Some(acked.clone()), ..Default::default() };
    let err = w.put_file("a.txt", Bytes::from("the next edit"), &edit).await.unwrap_err();
    assert!(hook.fired.load(std::sync::atomic::Ordering::SeqCst), "the fixture never reached the race");
    let writers = w.snapshot().await.unwrap().manifest.entries["a.txt"].etag.clone();
    assert_ne!(writers, acked, "the fixture's writer did not publish");
    assert!(matches!(&err, VerbError::FileChanged { current: Some(c) } if c == &writers), "{err}");
    assert_eq!(&w.get_file("a.txt").await.unwrap().body[..], b"the writer's");
    let cited: Vec<String> = w.snapshot().await.unwrap().manifest.entries.values().map(|e| e.key.clone()).collect();
    let orphan = handles_of(&plain, &w0, "a.txt").await.into_iter().find(|k| !cited.contains(k) && !k.contains("writer-flush"));
    assert!(orphan.is_some(), "the refused save's PUT did not land at a handle of its own");
}



/// Finding 12's second half under P2: a caller that goes away after the
/// save's PUT and before its CAS leaves nothing half-done. Nothing is
/// cited from it, the version the workspace cites is untouched, and the
/// next save naming that version lands. (A caller that goes away AFTER the
/// CAS has a cited save and no answer — a lost ack, never a lost write.)
#[tokio::test]
async fn a_save_whose_caller_goes_away_after_its_put_leaves_the_citation_untouched() {
    use std::sync::atomic::Ordering::SeqCst;
    const HOLD: bool = true;
    let mem = Arc::new(MemoryStore::new());
    let plain: Arc<dyn ObjectStore> = mem.clone();
    let w0 = ws(&plain);
    let epoch = hold_lease(&plain, &w0).await;
    let hook = Arc::new(AfterPut {
        inner: mem.clone(),
        trigger: w0.config().file_key("a.txt"),
        cfg: w0.config().clone(),
        epoch,
        armed: false.into(),
        fired: false.into(),
        hold: HOLD.into(),
        path: "a.txt".into(),
        after_read: false.into(),
        race_pending: false.into(),
    });
    let s: Arc<dyn ObjectStore> = hook.clone();
    let w = ws(&s);
    let acked = w.put_file("a.txt", Bytes::from("acked"), &PutFile::default()).await.unwrap();
    hook.armed.store(true, SeqCst);
    let edit = PutFile { if_match: Some(acked.clone()), ..Default::default() };
    tokio::select! {
        got = w.put_file("a.txt", Bytes::from("the next edit"), &edit) => {
            panic!("the fixture: the parked save returned: {got:?}")
        }
        _ = async {
            while !hook.fired.load(SeqCst) {
                tokio::task::yield_now().await;
            }
        } => {}
    }
    // The caller is gone, its PUT landed.
    let snap = w.snapshot().await.unwrap();
    assert_eq!(snap.manifest.entries["a.txt"].etag, acked, "an unfinished save moved the citation");
    assert_eq!(&w.get_file("a.txt").await.unwrap().body[..], b"acked");
    assert_eq!(handles_of(&plain, &w0, "a.txt").await.len(), 2, "the fixture: the parked PUT landed");
    let next = w.put_file("a.txt", Bytes::from("again"), &edit).await.unwrap();
    assert_eq!(w.snapshot().await.unwrap().manifest.entries["a.txt"].etag, next);
}


// ---------------------------------------------------------------------
// Read-only workspaces (per-user access design §4.6, phase E; F10): the
// typed refusal, the store wrapper behind it, and the control that every
// verb filed as a writer really writes.
// ---------------------------------------------------------------------

/// Every public verb that writes, in the order `each_writer` calls them.
const WRITERS: &[&str] = &[
    "put_file", "rename_file", "rename_files", "remove_file", "remove_files",
    "request_boundary", "request_sync", "put_draft", "promote_draft", "delete_draft",
    "cas_manifest",
];

/// Every public verb that only reads.
const READERS: &[&str] = &["get_file", "snapshot", "status", "wait_cited", "get_draft", "list_drafts"];

/// The requests a read-only credential allows, by `MemoryStore` op name
/// (the syncer battery's `READ_OPS`). Anything else is a write.
const READ_OPS: &[&str] = &[
    "get_whole", "get_range", "get_version", "head", "head_version", "list", "list_versions",
    "list_uploads", "lifecycle_rules", "epoch_read", "presign_get",
];

fn store_writes(mem: &MemoryStore) -> Vec<(&'static str, u64)> {
    mem.op_counts().into_iter().filter(|(op, _)| !READ_OPS.contains(op)).collect()
}

/// The census behind `WRITERS` and `READERS`, and its control: every
/// `pub async fn` in the verb modules is filed exactly once, so a verb
/// added later cannot skip both the guard and these tests.
#[test]
fn every_public_verb_is_filed_as_a_reader_or_a_writer() {
    let mut verbs: Vec<String> = [include_str!("../src/workspace.rs"), include_str!("../src/drafts.rs")]
        .iter()
        .flat_map(|src| src.lines())
        .filter_map(|l| l.trim_start().strip_prefix("pub async fn "))
        .map(|rest| rest[..rest.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap()].to_string())
        .collect();
    verbs.sort();
    assert!(
        verbs.contains(&"put_file".to_string()) && verbs.contains(&"promote_draft".to_string()),
        "the parse did not find the verbs: {verbs:?}"
    );
    let mut filed: Vec<String> = WRITERS.iter().chain(READERS).map(|s| s.to_string()).collect();
    filed.sort();
    assert_eq!(verbs, filed, "a public verb is not filed as a reader or a writer, or is filed twice");
}

/// What `each_writer` needs already in the bucket, put there through a
/// WRITABLE workspace: a tracked write for `drop_inbox` to drop, a draft
/// for `delete_draft` to discard, a lease for the epoch-validated verbs.
async fn seed_for_writers(w: &Workspace, store: &Arc<dyn ObjectStore>) -> u64 {
    w.put_file("seed.txt", Bytes::from("seed"), &PutFile::default()).await.unwrap();
    w.put_draft("u", "keep.txt", Bytes::from("kept"), None, None).await.unwrap();
    hold_lease(store, w).await
}

/// Every writer once, in an order in which each SUCCEEDS on a writable
/// workspace, so the control can say each one wrote. `after` gets each
/// verb's name and result as it returns.
async fn each_writer(
    w: &Workspace,
    epoch: u64,
    reset: &dyn Fn(),
    mut after: impl FnMut(&'static str, Result<(), VerbError>),
) {
    after("put_file", w.put_file("w.txt", Bytes::from("one"), &PutFile::default()).await.map(drop));
    after("rename_file", w.rename_file("w.txt", "r1.txt", None).await.map(drop));
    after("rename_files", w.rename_files(&[("r1.txt", "r2.txt")], None).await.map(drop));
    after("remove_file", w.remove_file("r2.txt", None, None).await);
    after("remove_files", w.remove_files(&[("seed.txt", None)], None).await);
    after("request_boundary", w.request_boundary(None).await.map(drop));
    after("request_sync", w.request_sync(None).await.map(drop));
    after("put_draft", w.put_draft("u", "d.txt", Bytes::from("draft"), None, None).await.map(drop));
    after("promote_draft", w.promote_draft("u", "d.txt", None).await.map(drop));
    after("delete_draft", w.delete_draft("u", "keep.txt").await);
    // Over whatever the saves above committed (P2: they commit).
    let snap = w.snapshot().await;
    let (seq, current) = snap.map(|s| (s.manifest.seq, s.manifest_etag)).unwrap_or((0, None));
    reset(); // the fixture's read is not the verb's
    let m = LeanManifest { seq: seq + 1, ..Default::default() };
    after("cas_manifest", w.cas_manifest(&m, current.as_deref(), epoch, "writers-census").await.map(drop));
}

/// F10, the typed layer: on `Workspace::read_only` every writer answers
/// `ReadOnly` (403 `read-only`, not retryable) and the store saw NO
/// request — not a write, not even the window read. The backstop is
/// asserted first, so with a verb's guard deleted this fails on the
/// typed answer while showing the store refused the write anyway.
#[tokio::test]
async fn a_read_only_workspace_refuses_every_writer_before_it_touches_the_store() {
    let mem = Arc::new(MemoryStore::new());
    let plain: Arc<dyn ObjectStore> = mem.clone();
    let epoch = seed_for_writers(&ws(&plain), &plain).await;
    let before = plain.list(PREFIX).await.unwrap();

    let ro = Workspace::read_only(plain.clone(), PREFIX);
    assert!(ro.is_read_only());
    mem.reset_op_counts();
    let mut seen = vec![];
    each_writer(&ro, epoch, &|| mem.reset_op_counts(), |verb, r| {
        assert_eq!(store_writes(&mem), vec![], "{verb} on a read-only workspace reached a store writer ({r:?})");
        match &r {
            Err(e @ VerbError::ReadOnly) => {
                assert_eq!((e.status(), e.code(), e.is_retryable()), (403, "read-only", false))
            }
            other => panic!("{verb} on a read-only workspace answered {other:?}, not ReadOnly"),
        }
        assert_eq!(mem.total_ops(), 0, "{verb} sent {:?} before refusing", mem.op_counts());
        seen.push(verb);
    })
    .await;
    assert_eq!(seen, WRITERS);
    assert_eq!(plain.list(PREFIX).await.unwrap(), before, "the bucket moved");
}

/// F10, the store layer: `store()` is public, so an embedder can write
/// around the typed check. The store a read-only workspace hands out
/// refuses that too, and sends nothing. Control: the same PUT through a
/// writable workspace's store lands.
#[tokio::test]
async fn a_read_only_workspace_store_refuses_a_write_that_goes_around_the_verbs() {
    use flint_store::{GenerationStamps, PutCondition};
    let mem = Arc::new(MemoryStore::new());
    let plain: Arc<dyn ObjectStore> = mem.clone();
    let body = Bytes::from("around the verbs");
    let stamps = GenerationStamps {
        generation: 1,
        epoch: 0,
        flush_uuid: "around".into(),
        boundary_source: None,
        posix: None,
    };

    let ro = Workspace::read_only(plain.clone(), PREFIX);
    let key = ro.config().file_key("around.txt");
    let err = ro
        .store()
        .put_whole(&key, body.clone(), &PutCondition::IfNoneMatchAny, &stamps, crc64_nvme(&body))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Auth(_)), "{err}");
    let err = ro.store().delete(&key).await.unwrap_err();
    assert!(matches!(err, StoreError::Auth(_)), "{err}");
    assert_eq!(mem.total_ops(), 0, "the refused writes sent {:?}", mem.op_counts());

    let w = ws(&plain);
    w.store()
        .put_whole(&key, body.clone(), &PutCondition::IfNoneMatchAny, &stamps, crc64_nvme(&body))
        .await
        .unwrap();
    assert_eq!(store_writes(&mem), vec![("put_whole", 1)]);
}

/// F10's control: the same calls on `Workspace::new` land, and each one
/// sends at least one write. A verb filed as a writer that writes nothing
/// would make the read-only test above vacuous for it.
#[tokio::test]
async fn every_writer_writes_on_a_writable_workspace() {
    let mem = Arc::new(MemoryStore::new());
    let plain: Arc<dyn ObjectStore> = mem.clone();
    let w = ws(&plain);
    assert!(!w.is_read_only());
    let epoch = seed_for_writers(&w, &plain).await;
    mem.reset_op_counts();
    let mut seen = vec![];
    each_writer(&w, epoch, &|| mem.reset_op_counts(), |verb, r| {
        if let Err(e) = r {
            panic!("{verb} failed on a writable workspace: {e:?}");
        }
        assert_ne!(store_writes(&mem), vec![], "{verb} sent no write on a writable workspace");
        mem.reset_op_counts();
        seen.push(verb);
    })
    .await;
    assert_eq!(seen, WRITERS);
    assert!(w.snapshot().await.unwrap().manifest.entries.is_empty(), "the last writer, the manifest CAS, landed");
}

/// A read-only workspace reads what writers wrote — a cited file, a
/// tracked one, a draft — answers as a writable one does, and sends
/// only reads.
#[tokio::test]
async fn a_read_only_workspace_reads_what_writers_wrote_and_sends_only_reads() {
    let mem = Arc::new(MemoryStore::new());
    let plain: Arc<dyn ObjectStore> = mem.clone();
    let w = ws(&plain);
    let epoch = hold_lease(&plain, &w).await;
    let cited = w.put_file("cited.txt", Bytes::from("cited"), &PutFile::default()).await.unwrap();
    let tracked = w.put_file("tracked.txt", Bytes::from("tracked"), &PutFile::default()).await.unwrap();
    w.put_draft("u", "d.txt", Bytes::from("draft"), None, None).await.unwrap();
    let listing = w.snapshot().await.unwrap().listing();

    let ro = Workspace::read_only(plain.clone(), PREFIX);
    mem.reset_op_counts();
    let mut seen = vec![];

    let blob = ro.get_file("cited.txt").await.unwrap();
    assert_eq!((blob.etag.as_str(), blob.body), (cited.as_str(), Bytes::from("cited")));
    let blob = ro.get_file("tracked.txt").await.unwrap();
    assert_eq!((blob.etag.as_str(), blob.body), (tracked.as_str(), Bytes::from("tracked")));
    seen.push("get_file");
    let snap = ro.snapshot().await.unwrap();
    assert_eq!(snap.listing(), listing);
    assert_eq!(snap.listing().iter().map(|l| l.path.as_str()).collect::<Vec<_>>(), ["cited.txt", "tracked.txt"]);
    seen.push("snapshot");
    let st = ro.status().await.unwrap();
    assert_eq!((st.seq, st.epoch), (Some(2), Some(epoch)), "two saves, two commits");
    seen.push("status");
    let seq = ro.wait_cited("cited.txt", &cited, Duration::from_secs(5), Duration::from_millis(10)).await.unwrap();
    assert_eq!(seq, 2);
    seen.push("wait_cited");
    assert_eq!(ro.get_draft("u", "d.txt").await.unwrap().body, Bytes::from("draft"));
    seen.push("get_draft");
    let rows = ro.list_drafts("u").await.unwrap();
    assert_eq!(rows.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["d.txt"]);
    seen.push("list_drafts");
    assert_eq!(seen, READERS);

    assert_eq!(store_writes(&mem), vec![], "a read-only workspace's reads sent a write");
    assert!(mem.total_ops() > 0, "the control: the reads reached the store");
}

/// M8 (2026-09-24 simplification analysis): a warm read costs the POINTER
/// and the object — never the manifest's entries again. The gateway is a
/// publisher under P2 and every read went through a full manifest load; the
/// document is immutable per pointer etag, so it is cached by that etag.
/// Both on one `Workspace` (an embedder's) and across requests, through the
/// server's shared core.
#[tokio::test]
async fn a_warm_read_fetches_the_pointer_and_the_object_and_no_manifest_entries() {
    let counting = Arc::new(CountGets::new(Arc::new(MemoryStore::new())));
    let s: Arc<dyn ObjectStore> = counting.clone();
    let w = ws(&s);
    for i in 0..20 {
        w.put_file(&format!("d/f{i:02}.txt"), Bytes::from(format!("v{i}")), &PutFile::default()).await.unwrap();
    }
    let entries = |keys: &[String]| keys.iter().filter(|k| k.contains("/manifests/") || k.contains("/chunks/")).count();

    counting.drain();
    w.get_file("d/f03.txt").await.unwrap();
    assert!(entries(&counting.drain()) > 0, "PRECONDITION: a cold read did not load the manifest");
    w.get_file("d/f04.txt").await.unwrap();
    let warm = counting.drain();
    assert_eq!(entries(&warm), 0, "a warm read loaded the manifest's entries again: {warm:?}");

    let mut workspaces = std::collections::BTreeMap::new();
    workspaces.insert("proj1".to_string(), PREFIX.to_string());
    let core = flint_lean_gateway::http::GatewayCore {
        store: s.clone(),
        workspaces,
        token: "t".into(),
        max_put_bytes: 1 << 20,
        manifests: Default::default(),
    };
    core.workspace("proj1").unwrap().get_file("d/f05.txt").await.unwrap();
    counting.drain();
    core.workspace("proj1").unwrap().get_file("d/f06.txt").await.unwrap();
    let warm = counting.drain();
    assert_eq!(entries(&warm), 0, "the second request loaded the manifest's entries again: {warm:?}");

    // A save moves the pointer: the next read sees it.
    w.put_file("d/f07.txt", Bytes::from("new"), &PutFile { if_match: Some("*".into()), ..Default::default() }).await.unwrap();
    assert_eq!(core.workspace("proj1").unwrap().get_file("d/f07.txt").await.unwrap().body, Bytes::from("new"));
}

/// M8: the read door verifies the bytes against the CRC the citation
/// carries, as checkout and the consume do. A corrupted object is refused
/// with `corrupt`, never served.
#[tokio::test]
async fn a_read_whose_bytes_do_not_match_the_citation_is_refused() {
    let mem = Arc::new(MemoryStore::new());
    let s: Arc<dyn ObjectStore> = mem.clone();
    let w = ws(&s);
    w.put_file("a.txt", Bytes::from("the bytes the user saved"), &PutFile::default()).await.unwrap();
    let key = w.snapshot().await.unwrap().manifest.entries["a.txt"].key.clone();
    mem.inject_corrupt_body(&key, |b| b[3] ^= 0x40);
    let err = w.get_file("a.txt").await.unwrap_err();
    assert_eq!((err.status(), err.code()), (502, "corrupt"), "{err}");
}
