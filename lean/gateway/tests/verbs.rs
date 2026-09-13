//! The library API, verb by verb, on the in-memory store — what an
//! embedder's backend sees, with no HTTP anywhere. The gateway's own
//! battery (`battery.rs`) drives the same verbs through the wire; this
//! file pins what the typed surface promises: the refusals, what they
//! carry, and the order of writes.

use std::sync::Arc;
use std::time::Duration;

use flint_lean_gateway::{
    crc64_nvme, crc64_to_b64, Bytes, LeanEntry, LeanManifest, MemoryStore, ObjectStore, PutFile,
    VerbError, Workspace,
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

/// What the syncer's barrier does after consuming the inbox, in one
/// CAS: cite `path` at `etag` with the CRC of `body`.
async fn cite(w: &Workspace, epoch: u64, seq: u64, path: &str, etag: &str, body: &[u8]) -> String {
    let mut m = LeanManifest { seq, ..Default::default() };
    m.entries.insert(
        path.to_string(),
        LeanEntry {
            key: w.config().file_key(path),
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

#[tokio::test]
async fn a_write_is_durable_and_readable_at_once_and_tracked_until_cited() {
    let s = store();
    let w = ws(&s);
    let etag = w.put_file("a.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();

    let blob = w.get_file("a.txt").await.unwrap();
    assert_eq!(blob.body, Bytes::from("one"));
    assert_eq!(blob.etag, etag);

    // Object at the real key, entry in the inbox, nothing in the manifest.
    assert!(s.head(&w.config().file_key("a.txt")).await.is_ok());
    let snap = w.snapshot().await.unwrap();
    assert_eq!(snap.inbox.entries.len(), 1);
    assert_eq!(snap.inbox.entries[0].etag, etag);
    assert_eq!(snap.inbox.entries[0].author, "ui", "no author ⇒ `ui`");
    assert!(snap.manifest.entries.is_empty());
    assert!(snap.manifest_etag.is_none());

    let st = w.status().await.unwrap();
    assert_eq!(st.inbox_depth, 1);
    assert_eq!(st.seq, None);
    assert_eq!(st.epoch, None, "no syncer holds this workspace");
    assert!(st.window.is_none());
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
    for bad in ["../x", "/abs", "a//b", ".flint/x", ".flint", "a/./b", ".flint-sync/state"] {
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

#[tokio::test]
async fn a_cited_file_reads_through_the_manifest_and_wait_cited_sees_the_citation() {
    let s = store();
    let w = ws(&s);
    let epoch = hold_lease(&s, &w).await;
    let etag = w.put_file("a.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();

    // Nobody has cited it: the wait says so within the bound, and the
    // write is still readable meanwhile.
    let err = w
        .wait_cited("a.txt", &etag, Duration::from_millis(400), Duration::from_millis(50))
        .await
        .unwrap_err();
    assert!(matches!(err, VerbError::CitationPending { .. }), "{err}");
    assert_eq!((err.status(), err.code()), (202, "citation-pending"));
    assert_eq!(w.get_file("a.txt").await.unwrap().body, Bytes::from("one"));

    // A syncer cites it while a caller waits.
    let w2 = w.clone();
    let etag2 = etag.clone();
    let citer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cite(&w2, epoch, 1, "a.txt", &etag2, b"one").await
    });
    let seq = w
        .wait_cited("a.txt", &etag, Duration::from_secs(5), Duration::from_millis(50))
        .await
        .unwrap();
    assert_eq!(seq, 1);
    citer.await.unwrap();

    let snap = w.snapshot().await.unwrap();
    assert_eq!(snap.manifest.seq, 1);
    assert_eq!(snap.manifest.entries["a.txt"].etag, etag);
    assert!(snap.manifest_etag.is_some());
    assert_eq!(w.status().await.unwrap().seq, Some(1));

    // The read now resolves through the citation.
    assert_eq!(w.get_file("a.txt").await.unwrap().etag, etag);
}

#[tokio::test]
async fn wait_cited_refuses_at_once_when_no_syncer_holds_the_lease() {
    let s = store();
    let w = ws(&s);
    let etag = w.put_file("a.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();
    let t0 = std::time::Instant::now();
    let err = w
        .wait_cited("a.txt", &etag, Duration::from_secs(30), Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(t0.elapsed() < Duration::from_secs(2), "must not run the 30 s clock down");
    match err {
        VerbError::CitationPending { reason, .. } => assert!(reason.contains("no syncer"), "{reason}"),
        e => panic!("{e}"),
    }
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
    let entries = w.snapshot().await.unwrap().inbox.entries;
    w.drop_inbox(epoch, &entries).await.unwrap();
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
async fn an_open_barrier_window_refuses_hitl_writes_until_it_closes() {
    let s = store();
    let w = ws(&s);
    let epoch = hold_lease(&s, &w).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    w.open_window(epoch, now + 60).await.unwrap();
    assert!(w.status().await.unwrap().window.is_some());

    // Refused at once, with the window's remaining time as the hint.
    let err = w.put_file("a.txt", Bytes::from("x"), &PutFile::default()).await.unwrap_err();
    match &err {
        VerbError::WindowOpen { retry_after_secs, .. } => {
            assert!((55..=60).contains(retry_after_secs), "{retry_after_secs}")
        }
        e => panic!("{e}"),
    }
    assert_eq!((err.status(), err.code()), (409, "barrier-window-open"));
    assert!(err.is_retryable());
    assert!(s.head(&w.config().file_key("a.txt")).await.is_err(), "nothing was written");

    // A bounded wait that runs out is the same refusal, later.
    let patient = w.clone().with_window_wait(Some(Duration::from_millis(300)));
    let t0 = std::time::Instant::now();
    let err = patient.put_file("a.txt", Bytes::from("x"), &PutFile::default()).await.unwrap_err();
    assert!(matches!(err, VerbError::WindowOpen { .. }), "{err}");
    assert!(t0.elapsed() >= Duration::from_millis(300));

    // ...and one that outlasts the window sees the write through.
    let w2 = w.clone();
    let closer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        w2.clear_window(epoch, &[]).await.unwrap();
    });
    let patient = w.clone().with_window_wait(Some(Duration::from_secs(5)));
    patient.put_file("a.txt", Bytes::from("x"), &PutFile::default()).await.unwrap();
    closer.await.unwrap();
    assert!(w.status().await.unwrap().window.is_none());
    assert_eq!(w.get_file("a.txt").await.unwrap().body, Bytes::from("x"));
}

#[tokio::test]
async fn the_syncer_facing_verbs_are_epoch_validated() {
    let s = store();
    let w = ws(&s);

    // No cell at all.
    let err = w.open_window(1, 10).await.unwrap_err();
    assert!(matches!(err, VerbError::NoHolder), "{err}");
    assert_eq!((err.status(), err.code()), (403, "no-holder"));

    let epoch = hold_lease(&s, &w).await;
    // A stale claim, and a claim from the future, both die here.
    for claimed in [epoch.wrapping_sub(1), epoch + 1] {
        let err = w.open_window(claimed, 10).await.unwrap_err();
        match &err {
            VerbError::StaleEpoch { cell_epoch, holder_id, claimed: c } => {
                assert_eq!(*cell_epoch, epoch);
                assert_eq!(holder_id, "syncer-1");
                assert_eq!(*c, claimed);
            }
            e => panic!("{e}"),
        }
        assert_eq!((err.status(), err.code()), (403, "stale-epoch"));
        assert!(matches!(w.clear_window(claimed, &[]).await.unwrap_err(), VerbError::StaleEpoch { .. }));
        assert!(matches!(w.drop_inbox(claimed, &[]).await.unwrap_err(), VerbError::StaleEpoch { .. }));
        let m = LeanManifest::default();
        assert!(matches!(
            w.cas_manifest(&m, None, claimed, "u").await.unwrap_err(),
            VerbError::StaleEpoch { .. }
        ));
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
    assert_eq!(w.snapshot().await.unwrap().inbox.entries.len(), 1, "a draft is not tracked");

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
    let inbox = w.snapshot().await.unwrap().inbox.entries;
    assert_eq!(inbox.last().unwrap().etag, v3, "a promote is a tracked HITL write");
    assert_eq!(inbox.last().unwrap().author, "alice@x");
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
        assert_eq!(w.status().await.unwrap().inbox_depth, 1);
        assert_eq!(w.prefix(), format!("teams/t{i}"));
    }
    // A trailing slash on the prefix is dropped, so `teams/t0/` IS `teams/t0`.
    let alias = Workspace::new(s.clone(), "teams/t0/");
    assert_eq!(alias.get_file("shared.txt").await.unwrap().body, Bytes::from("ws 0"));
}

// ── delete and rename (docs/plans/flint-lean-delete-rename-design.md) ──

#[tokio::test]
async fn a_delete_is_recorded_hides_the_path_and_is_withdrawable() {
    let s = store();
    let w = ws(&s);
    let etag_a = w.put_file("a.txt", Bytes::from("aaa"), &PutFile::default()).await.unwrap();
    w.put_file("b.txt", Bytes::from("bbb"), &PutFile::default()).await.unwrap();

    // A stale precondition refuses and records nothing.
    let err = w.remove_file("a.txt", Some("dilip"), Some("\"stale\"")).await.unwrap_err();
    assert!(matches!(&err, VerbError::FileChanged { current: Some(c) } if c == &etag_a), "{err}");
    assert_eq!(w.snapshot().await.unwrap().pending_removals().count(), 0);
    let err = w.remove_file("nope.txt", None, None).await.unwrap_err();
    assert!(matches!(err, VerbError::NoSuchFile(_)), "{err}");

    // Recorded: the listing hides the path at once, the bytes stay put
    // until the syncer performs it, and the path reads as gone to a
    // second removal.
    w.remove_file("a.txt", Some("dilip"), Some(&etag_a)).await.unwrap();
    let snap = w.snapshot().await.unwrap();
    let pending: Vec<_> = snap.pending_removals().collect();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].path, "a.txt");
    assert_eq!(pending[0].author, "dilip");
    assert!(pending[0].moved_to.is_none());
    let listed: Vec<String> = snap.listing().into_iter().map(|l| l.path).collect();
    assert_eq!(listed, vec!["b.txt".to_string()]);
    assert_eq!(w.status().await.unwrap().removals_pending, 1);
    assert!(s.head(&w.config().file_key("a.txt")).await.is_ok(), "the object is the syncer's to delete");
    assert!(w.get_file("a.txt").await.is_ok(), "bytes stay readable until then");
    assert!(matches!(w.remove_file("a.txt", None, None).await.unwrap_err(), VerbError::NoSuchFile(_)));

    // Withdrawn: back in the listing; a second withdraw has nothing.
    w.withdraw_removal("a.txt").await.unwrap();
    assert_eq!(w.snapshot().await.unwrap().listing().len(), 2);
    let err = w.withdraw_removal("a.txt").await.unwrap_err();
    assert!(matches!(err, VerbError::NoRemoval(_)), "{err}");
    assert_eq!((err.status(), err.code()), (404, "no-removal"));
    assert!(matches!(w.withdraw_removal("../x").await.unwrap_err(), VerbError::BadPath(_)));

    // A batch is one transaction: all recorded, or none.
    let err = w.remove_files(&[("b.txt", None), ("zzz", None)], None).await.unwrap_err();
    assert!(matches!(err, VerbError::NoSuchFile(_)), "{err}");
    assert_eq!(w.snapshot().await.unwrap().pending_removals().count(), 0, "none recorded");
    w.remove_files(&[("a.txt", None), ("b.txt", None)], None).await.unwrap();
    let snap = w.snapshot().await.unwrap();
    assert_eq!(snap.pending_removals().count(), 2);
    assert!(snap.pending_removals().all(|r| r.author == "ui"));
    assert!(snap.listing().is_empty());
}

#[tokio::test]
async fn a_rename_copies_then_records_both_halves_in_one_cas_and_refuses_what_it_must() {
    let s = store();
    let w = ws(&s);
    let body = Bytes::from("the same bytes");
    w.put_file("notes/a.txt", body.clone(), &PutFile::default()).await.unwrap();
    let etag_taken = w.put_file("taken.txt", Bytes::from("t"), &PutFile::default()).await.unwrap();

    // Refused before anything is written.
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
    assert!(s.head(&w.config().file_key("docs/b.txt")).await.is_err(), "no copy landed");

    // The rename: destination readable at once with the same bytes,
    // source out of the listing at once, one entry + one removal.
    let etag_b = w.rename_file("notes/a.txt", "docs/b.txt", Some("dilip")).await.unwrap();
    let blob = w.get_file("docs/b.txt").await.unwrap();
    assert_eq!(blob.body, body);
    assert_eq!(blob.etag, etag_b);
    let snap = w.snapshot().await.unwrap();
    let listed: Vec<String> = snap.listing().into_iter().map(|l| l.path).collect();
    assert_eq!(listed, vec!["docs/b.txt".to_string(), "taken.txt".to_string()]);
    let entry = snap.inbox.entries.iter().find(|e| e.path == "docs/b.txt").expect("tracked");
    assert_eq!(entry.author, "dilip");
    assert_eq!(entry.crc64_b64.as_deref(), Some(crc64_to_b64(crc64_nvme(&body)).as_str()), "the bytes' own CRC");
    let removal = snap.pending_removals().find(|r| r.path == "notes/a.txt").expect("recorded");
    assert_eq!(removal.moved_to.as_deref(), Some("docs/b.txt"));
    assert_eq!(removal.author, "dilip");
    assert!(w.get_file("notes/a.txt").await.is_ok(), "bytes stay until the syncer performs it");

    // A batch: two pairs, one CAS; a duplicate destination refuses the whole batch.
    w.put_file("c1", Bytes::from("1"), &PutFile::default()).await.unwrap();
    w.put_file("c2", Bytes::from("2"), &PutFile::default()).await.unwrap();
    assert!(matches!(w.rename_files(&[("c1", "d1"), ("c2", "d1")], None).await.unwrap_err(), VerbError::BadPath(_)));
    assert!(s.head(&w.config().file_key("d1")).await.is_err(), "nothing copied");
    let etags = w.rename_files(&[("c1", "d1"), ("c2", "d2")], None).await.unwrap();
    assert_eq!(etags.len(), 2);
    let snap = w.snapshot().await.unwrap();
    assert_eq!(snap.pending_removals().count(), 3);
    assert_eq!(w.get_file("d2").await.unwrap().body, Bytes::from("2"));
}

#[tokio::test]
async fn a_rename_overwrites_its_own_orphan_but_never_a_strangers_object() {
    use flint_lean_gateway::{crc64_nvme, StoreError};
    use flint_store::{GenerationStamps, PutCondition};
    let s = store();
    let w = ws(&s);
    let body = Bytes::from("moved bytes");
    w.put_file("a.txt", body.clone(), &PutFile::default()).await.unwrap();
    w.put_file("s.txt", Bytes::from("second source"), &PutFile::default()).await.unwrap();
    let plant = |key: String, bytes: &'static str, uuid: &'static str| {
        let s = s.clone();
        async move {
            let b = Bytes::from(bytes);
            let crc = crc64_nvme(&b);
            let stamps = GenerationStamps {
                generation: 1,
                epoch: 0,
                flush_uuid: uuid.into(),
                boundary_source: None,
                posix: None,
            };
            s.put_whole(&key, b, &PutCondition::Unconditional, &stamps, crc).await.unwrap().etag
        }
    };
    // An orphan of an earlier attempt of THIS verb: untracked, uncited,
    // stamped as a rename copy. Overwritten.
    plant(w.config().file_key("b.txt"), "stale copy", "gateway-rename-earlier").await;
    w.rename_file("a.txt", "b.txt", None).await.unwrap();
    assert_eq!(w.get_file("b.txt").await.unwrap().body, body);

    // A stranger's object at the destination: refused, and intact.
    let theirs = plant(w.config().file_key("c.txt"), "someone else's", "other-writer").await;
    let err = w.rename_file("s.txt", "c.txt", None).await.unwrap_err();
    match &err {
        VerbError::DestinationExists { current, .. } => assert_eq!(current.as_deref(), Some(theirs.as_str())),
        e => panic!("{e}"),
    }
    let (_, kept) = s.get_whole(&w.config().file_key("c.txt"), None).await.unwrap();
    assert_eq!(&kept[..], b"someone else's");
    assert!(matches!(s.head("nothing").await, Err(StoreError::NotFound(_))));
}

#[tokio::test]
async fn a_rename_is_refused_while_a_window_is_open_and_copies_nothing() {
    let s = store();
    let w = ws(&s);
    w.put_file("a.txt", Bytes::from("x"), &PutFile::default()).await.unwrap();
    let epoch = hold_lease(&s, &w).await;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    w.open_window(epoch, now + 60).await.unwrap();
    let err = w.rename_file("a.txt", "b.txt", None).await.unwrap_err();
    assert!(matches!(err, VerbError::WindowOpen { .. }), "{err}");
    assert!(s.head(&w.config().file_key("b.txt")).await.is_err(), "refused BEFORE the copy");
    // A removal is not window-gated: it touches nothing when recorded.
    w.remove_file("a.txt", None, None).await.unwrap();
    w.withdraw_removal("a.txt").await.unwrap();
    w.clear_window(epoch, &[]).await.unwrap();
    w.rename_file("a.txt", "b.txt", None).await.unwrap();
    assert_eq!(w.get_file("b.txt").await.unwrap().body, Bytes::from("x"));
}

/// THE READ DOOR OVERLAYS THE INBOX. An overwrite of a file the
/// manifest already cites is readable at once, by every reader — not
/// 409 `moved` until the syncer re-cites it, which is what preferring
/// the citation answered before 0.2.1, and forever in a workspace no
/// syncer runs on. And the overlay never answers for an entry the
/// bucket has outrun: in the barrier's window after the manifest CAS
/// (the syncer published the agent's newer bytes and cited them; the
/// consumed entry leaves the cell only with the window) the read
/// yields to the citation. Mutation: answer `moved` on the citation's
/// precondition failure instead of consulting the cell ⇒ the first
/// read fails (and the battery leg that reads a promote).
#[tokio::test]
async fn an_overwrite_of_a_cited_file_reads_at_once_and_an_outrun_entry_yields_to_the_citation() {
    use flint_store::{GenerationStamps, PutCondition};
    let s = store();
    let w = ws(&s);
    let epoch = hold_lease(&s, &w).await;
    let v1 = w.put_file("a.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();
    cite(&w, epoch, 1, "a.txt", &v1, b"one").await;
    assert_eq!(w.get_file("a.txt").await.unwrap().etag, v1);

    // A browser overwrites the cited file: every reader sees it now,
    // and the read agrees with the listing.
    let v2 = w
        .put_file(
            "a.txt",
            Bytes::from("two"),
            &PutFile { if_match: Some(v1.clone()), ..Default::default() },
        )
        .await
        .unwrap();
    let blob = w.get_file("a.txt").await.unwrap();
    assert_eq!((blob.etag.as_str(), &blob.body[..]), (v2.as_str(), &b"two"[..]));
    let snap = w.snapshot().await.unwrap();
    assert_eq!(snap.listing().iter().find(|l| l.path == "a.txt").unwrap().etag, v2);
    assert_eq!(snap.manifest.entries["a.txt"].etag, v1, "the library never edits the manifest");

    // The barrier consumed v2, the agent changed the file again, the
    // barrier published and cited THAT — and v2's entry is still in
    // the cell. The read serves what the manifest cites, never `moved`.
    let key = w.config().file_key("a.txt");
    let three = Bytes::from("three");
    let crc = crc64_nvme(&three);
    let stamps = GenerationStamps {
        generation: 2,
        epoch,
        flush_uuid: "agent-publish".into(),
        boundary_source: None,
        posix: None,
    };
    let v3 = s.put_whole(&key, three, &PutCondition::Unconditional, &stamps, crc).await.unwrap().etag;
    cite(&w, epoch, 2, "a.txt", &v3, b"three").await;
    let tracked = w.snapshot().await.unwrap().inbox.entries.iter().filter(|e| e.path == "a.txt").count();
    assert_eq!(tracked, 1, "the fixture: the outrun entry is still in the cell");
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

/// WHAT THE COMMON READ COSTS. A cited path nobody has overwritten is
/// read guarded on its citation and the inbox cell is never fetched —
/// even while a consumed entry for it still lingers in the cell. Only
/// an overwrite, the citation's precondition failure, fetches the cell
/// and reads the entry; once the barrier cites past the entry the read
/// is back to one object fetch. Mutation: the 0.2.1 order (the cell
/// first) fetches it on every read ⇒ the first count fails.
#[tokio::test]
async fn a_read_of_an_unmodified_cited_file_never_fetches_the_inbox() {
    use flint_store::{GenerationStamps, PutCondition};
    let counting = Arc::new(CountGets::new(Arc::new(MemoryStore::new())));
    let s: Arc<dyn ObjectStore> = counting.clone();
    let w = ws(&s);
    let epoch = hold_lease(&s, &w).await;
    let v1 = w.put_file("a.txt", Bytes::from("one"), &PutFile::default()).await.unwrap();
    cite(&w, epoch, 1, "a.txt", &v1, b"one").await;
    assert_eq!(w.snapshot().await.unwrap().inbox.entries.len(), 1, "the fixture: the entry lingers");

    counting.take();
    assert_eq!(w.get_file("a.txt").await.unwrap().etag, v1);
    assert_eq!(counting.take(), (0, 1), "(inbox fetches, object fetches) for the common read");

    // An overwrite: the cited fetch fails its precondition, the cell
    // is fetched once, the entry's bytes are fetched once.
    let v2 = w
        .put_file("a.txt", Bytes::from("two"), &PutFile { if_match: Some(v1.clone()), ..Default::default() })
        .await
        .unwrap();
    counting.take();
    assert_eq!(w.get_file("a.txt").await.unwrap().etag, v2);
    assert_eq!(counting.take(), (1, 2), "(inbox fetches, object fetches) for an overwritten read");

    // The barrier cites past the entry: one object fetch again, the
    // cell untouched, the lingering entry never consulted.
    let key = w.config().file_key("a.txt");
    let three = Bytes::from("three");
    let crc = crc64_nvme(&three);
    let stamps = GenerationStamps {
        generation: 2,
        epoch,
        flush_uuid: "agent-publish".into(),
        boundary_source: None,
        posix: None,
    };
    let v3 = s.put_whole(&key, three, &PutCondition::Unconditional, &stamps, crc).await.unwrap().etag;
    cite(&w, epoch, 2, "a.txt", &v3, b"three").await;
    counting.take();
    assert_eq!(w.get_file("a.txt").await.unwrap().etag, v3);
    assert_eq!(counting.take(), (0, 1), "(inbox fetches, object fetches) once cited past the entry");
}
