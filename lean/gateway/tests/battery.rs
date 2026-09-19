//! The gateway's battery: the HTTP door driven end to end against the
//! in-memory store, with a real syncer running real barriers beside
//! it. These tests lived in `flint-lean`'s battery until the gateway
//! became its own crate; the fixtures below are copies of that
//! battery's, and `flint_lean` is used exactly as an embedder uses it.

use std::sync::Arc;

use bytes::Bytes;
use flint_store::memory::MemoryStore;
use flint_store::{crc64_nvme, GenerationStamps, ObjectStore, PutCondition};

use flint_lean::inbox::{self, InboxEntry};
use flint_lean::lease::{self, ClaimOutcome};
use flint_lean::manifest;
use flint_lean::state::SyncerState;
use flint_lean::{now_unix, LeanConfig, LeanError, Syncer, LEAN_DIR};
use flint_lean_gateway::http::{routes, GatewayCore};

const PREFIX: &str = "tenant/proj1";

fn cfg_for(root: &std::path::Path) -> LeanConfig {
    LeanConfig::new(PREFIX, root)
}

async fn syncer(store: &Arc<MemoryStore>, root: &std::path::Path) -> Syncer {
    let cfg = cfg_for(root);
    let state = SyncerState::open(cfg.state_dir()).unwrap();
    Syncer {
        store: store.clone() as Arc<dyn ObjectStore>,
        cfg,
        state,
        lease: None,
        cell_written_at: None,
        noted_not_regular: Default::default(),
    }
}

/// Claim, looping claim_step (a fresh or released cell claims on the
/// first step; a foreign one needs the quiet polls).
async fn claim_until_held(sc: &mut Syncer, max_steps: u32) -> bool {
    for _ in 0..max_steps {
        match lease::claim_step(sc, true).await.unwrap() {
            ClaimOutcome::Claimed(_) => return true,
            ClaimOutcome::Waiting { .. } => {}
        }
    }
    false
}

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn read(root: &std::path::Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(root.join(rel)).ok()
}

/// Simulate the GATEWAY's HITL write: object PUT first (fresh read →
/// If-Match current / If-None-Match for a create), then the inbox entry.
async fn hitl_write(
    store: &Arc<MemoryStore>,
    cfg: &LeanConfig,
    path: &str,
    content: &str,
    author: &str,
) -> Result<String, LeanError> {
    let key = cfg.file_key(path);
    let cond = match store.head(&key).await {
        Ok(meta) => PutCondition::IfMatch(meta.etag),
        Err(_) => PutCondition::IfNoneMatchAny,
    };
    let body = Bytes::from(content.to_string());
    let crc = crc64_nvme(&body);
    let stamps = GenerationStamps {
        generation: 0,
        epoch: 0,
        flush_uuid: format!("gateway-{author}"),
        boundary_source: None,
        posix: None,
    };
    let meta = store.put_whole(&key, body, &cond, &stamps, crc).await?;
    inbox::gateway_append(
        store.as_ref(),
        cfg,
        InboxEntry {
            path: path.to_string(),
            etag: meta.etag.clone(),
            author: author.to_string(),
            added_unix: now_unix(),
            crc64_b64: Some(flint_store::crc64_to_b64(crc)),
            cited: None,
        },
    )
    .await?;
    Ok(meta.etag)
}

async fn current_etag(store: &Arc<MemoryStore>, cfg: &LeanConfig, path: &str) -> String {
    store.head(&cfg.file_key(path)).await.unwrap().etag
}

/// Publishes a tree with a clear in-scope / out-of-scope split: two
/// small files under `inputs/` and six 4 KiB files under `outputs/`.
/// The size asymmetry is deliberate — it is what lets the budget arms
/// below move exactly one dimension.
async fn scoped_fixture(store: &Arc<MemoryStore>) -> tempfile::TempDir {
    let dir_a = tempfile::tempdir().unwrap();
    let mut a = syncer(store, dir_a.path()).await;
    assert!(claim_until_held(&mut a, 3).await);
    a.checkout().await.unwrap();
    write(dir_a.path(), "inputs/wanted.txt", "the file the agent edits");
    write(dir_a.path(), "inputs/also.txt", "and this one");
    for i in 0..6 {
        write(dir_a.path(), &format!("outputs/big-{i}.bin"), &"x".repeat(4096));
    }
    a.run_barrier().await.unwrap();
    dir_a
}

// ── the gateway verbs ────────────────────────────────────────────────

const GW_TOKEN: &str = "test-bearer-0123456789abcdef";

fn gw_core(store: &Arc<MemoryStore>) -> Arc<GatewayCore> {
    let mut workspaces = std::collections::BTreeMap::new();
    workspaces.insert("proj1".to_string(), PREFIX.to_string());
    Arc::new(GatewayCore {
        store: store.clone() as Arc<dyn ObjectStore>,
        workspaces,
        token: GW_TOKEN.to_string(),
        max_put_bytes: 8 * 1024 * 1024,
    })
}

fn gw_req() -> warp::test::RequestBuilder {
    warp::test::request().header("authorization", format!("Bearer {GW_TOKEN}"))
}

/// Auth + tenancy: wrong bearer 401; unknown workspace 404; reserved
/// and traversal paths refused.
#[tokio::test]
async fn gateway_auth_tenancy_and_path_hygiene() {
    let store = Arc::new(MemoryStore::new());
    let routes = routes(gw_core(&store));

    let res = warp::test::request()
        .method("GET")
        .path("/lean/v1/proj1/status")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 401);

    let res = gw_req().method("GET").path("/lean/v1/nope/status").reply(&routes).await;
    assert_eq!(res.status(), 404);

    for bad in ["../../etc/passwd", ".flint/lean/manifest", ".flint-sync/baseline.json"] {
        let res = gw_req()
            .method("PUT")
            .path(&format!("/lean/v1/proj1/files/{bad}"))
            .body("x")
            .reply(&routes)
            .await;
        assert_eq!(res.status(), 400, "path {bad:?} must be refused");
    }
}

/// The full HITL flow THROUGH the gateway: PUT lands object + inbox
/// entry, the syncer's next barrier consumes and cites it, and the
/// gateway serves it back — first from the inbox fallback, then from
/// the manifest citation.
#[tokio::test]
async fn gateway_hitl_put_consumed_and_cited_by_barrier() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();

    let routes = routes(gw_core(&store));
    let res = gw_req()
        .method("PUT")
        .path("/lean/v1/proj1/files/docs/spec.md")
        .header("x-flint-author", "dilip")
        .body("user upload via gateway")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200, "{:?}", res.body());

    // Readable immediately via the inbox fallback (uncited yet).
    let res = gw_req().method("GET").path("/lean/v1/proj1/files/docs/spec.md").reply(&routes).await;
    assert_eq!(res.status(), 200);
    assert_eq!(&res.body()[..], b"user upload via gateway");

    // The barrier consumes + cites; the file lands in the agent tree.
    sc.run_barrier().await.unwrap();
    assert_eq!(read(dir.path(), "docs/spec.md").unwrap(), "user upload via gateway");
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert!(m.manifest.entries.contains_key("docs/spec.md"));

    // Still readable — now via the citation.
    let res = gw_req().method("GET").path("/lean/v1/proj1/files/docs/spec.md").reply(&routes).await;
    assert_eq!(res.status(), 200);
}

/// A consume whose WRITE fails is transient, and must not be recorded as
/// a containment refusal nor dropped from the cell.
///
/// Containment is already decided before this point (`check_contained`),
/// so the error reaching the write is an I/O one — ENOSPC, EACCES, EIO.
/// The old arm labelled it `consume-refused-containment` and pushed the
/// entry to `consumed`, so a full disk silently and permanently lost a
/// foreign write: the bytes stay in the bucket, the workspace never
/// adopts them, and nothing re-offers the entry.
///
/// A pre-existing DIRECTORY at the target path induces exactly that
/// split — the parent resolves, so containment passes, and the rename
/// then fails. The load-bearing assertion is the RETRY: remove the
/// obstruction, run one more barrier, and the file must land. That is
/// what proves the entry survived, and it is the half a label check
/// alone would miss.
#[tokio::test]
async fn a_failed_consume_write_is_retried_not_silently_dropped() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();

    // The obstruction must leave the path locally ABSENT, or consume
    // takes the locally-dirty branch and never reaches the write at all.
    // A directory AT the path looks present and yields `consume-dirty` —
    // that was this test's first draft, and it exercised nothing. A
    // read-only PARENT keeps `ro/file.txt` absent while the write fails.
    if unsafe { libc::geteuid() } == 0 {
        // root ignores the mode bits, so the write would succeed and the
        // test would pass having tested nothing. Skipping loudly beats
        // a green run that proves the opposite of what it claims.
        eprintln!("SKIPPED: running as root, a read-only dir cannot induce EACCES");
        return;
    }
    let ro = dir.path().join("ro");
    std::fs::create_dir(&ro).unwrap();
    std::fs::set_permissions(&ro, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();

    let routes = routes(gw_core(&store));
    let res = gw_req()
        .method("PUT")
        .path("/lean/v1/proj1/files/ro/file.txt")
        .header("x-flint-author", "dilip")
        .body("foreign bytes that must not be lost")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200, "{:?}", res.body());

    // The barrier must not wedge on it.
    sc.run_barrier().await.unwrap();

    let conflicts = sc.state.load_conflicts().unwrap();
    assert!(
        conflicts.iter().any(|c| c.path == "ro/file.txt" && c.kind.starts_with("consume-write-failed")),
        "a write failure must be surfaced as itself, not as containment: {conflicts:?}"
    );
    assert!(
        !conflicts.iter().any(|c| c.path == "ro/file.txt" && c.kind.starts_with("consume-refused-containment")),
        "containment was already checked upstream; a full disk is not a planted symlink"
    );

    // THE CONTROL: clear the obstruction, and the retained entry must be
    // re-offered and land. If the entry had been consumed, this barrier
    // would do nothing and the file would never appear.
    std::fs::set_permissions(&ro, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    sc.run_barrier().await.unwrap();
    assert_eq!(
        read(dir.path(), "ro/file.txt").unwrap(),
        "foreign bytes that must not be lost",
        "the entry must have survived in the cell and been retried"
    );
}

/// The window gate: a PUT during a live barrier window is refused with
/// Retry-After; an expired window admits (the dead-syncer unwedge).
#[tokio::test]
async fn gateway_put_refused_while_window_open() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let sc = syncer(&store, dir.path()).await;
    let routes = routes(gw_core(&store));

    inbox::open_window(store.as_ref(), &sc.cfg, 1, now_unix() + 120).await.unwrap();
    let res = gw_req()
        .method("PUT")
        .path("/lean/v1/proj1/files/a.txt")
        .body("x")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 409);
    assert!(res.headers().contains_key("retry-after"));

    inbox::clear_window(store.as_ref(), &sc.cfg, 1, &[]).await.unwrap();
    let res = gw_req()
        .method("PUT")
        .path("/lean/v1/proj1/files/a.txt")
        .body("x")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200);

    inbox::open_window(store.as_ref(), &sc.cfg, 1, now_unix() - 5).await.unwrap();
    let res = gw_req()
        .method("PUT")
        .path("/lean/v1/proj1/files/b.txt")
        .body("y")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200, "an expired window must not wedge HITL");
}

/// P5's teeth: the manifest CAS verb validates the claimed epoch
/// against the cell PER REQUEST — a deposed epoch is 403 even with a
/// THE LOST UPDATE, and the arm that now refuses it.
///
/// Two browsers each read v1 and then write. Before preconditions were
/// accepted, both PUTs succeeded — the gateway took a FRESH head of its
/// own immediately before writing, so the second one's `If-Match` was
/// against v2 and matched, and it silently overwrote the first. That is
/// the whole reason this arm exists, and `b_overwrites_a` is the leg
/// that fails if `judge_preconditions` is removed.
#[tokio::test]
async fn gateway_put_refuses_an_unconditioned_or_stale_overwrite() {
    let store = Arc::new(MemoryStore::new());
    let routes = routes(gw_core(&store));
    let path = "/lean/v1/proj1/files/shared.md";

    // Create: no precondition needed for a path that is not there.
    let res = gw_req().method("PUT").path(path).body("v1").reply(&routes).await;
    assert_eq!(res.status(), 200, "{:?}", res.body());
    let v1: String = serde_json::from_slice::<serde_json::Value>(res.body())
        .unwrap()["etag"]
        .as_str()
        .unwrap()
        .to_string();

    // An overwrite that names nothing is REFUSED, not performed.
    let res = gw_req().method("PUT").path(path).body("blind").reply(&routes).await;
    assert_eq!(res.status(), 428, "an unconditioned overwrite must be refused");

    // Browser A writes from v1 and wins.
    let res =
        gw_req().method("PUT").path(path).header("if-match", &v1).body("from-A").reply(&routes).await;
    assert_eq!(res.status(), 200);
    let v2: String = serde_json::from_slice::<serde_json::Value>(res.body())
        .unwrap()["etag"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(v1, v2);

    // Browser B still holds v1. THE LOST UPDATE: this used to be a 200.
    let b_overwrites_a =
        gw_req().method("PUT").path(path).header("if-match", &v1).body("from-B").reply(&routes).await;
    assert_eq!(b_overwrites_a.status(), 412, "a stale writer must be told, not obeyed");
    assert_eq!(
        b_overwrites_a.headers().get("etag").unwrap().to_str().unwrap(),
        v2,
        "412 carries the etag the caller should have sent"
    );

    // A's bytes are still there — B did not win.
    let res = gw_req().method("GET").path(path).reply(&routes).await;
    assert_eq!(&res.body()[..], b"from-A");

    // `*` means "whatever is there now", and is honoured.
    let res =
        gw_req().method("PUT").path(path).header("if-match", "*").body("forced").reply(&routes).await;
    assert_eq!(res.status(), 200);

    // create-if-absent on a path that exists is a 412, not a clobber.
    let res = gw_req()
        .method("PUT")
        .path(path)
        .header("if-none-match", "*")
        .body("x")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 412);

    // A stale caller writing a path that has since gone.
    let res = gw_req()
        .method("PUT")
        .path("/lean/v1/proj1/files/never.md")
        .header("if-match", "\"deadbeef\"")
        .body("x")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 412);

    // create-if-absent on an absent path creates.
    let res = gw_req()
        .method("PUT")
        .path("/lean/v1/proj1/files/fresh.md")
        .header("if-none-match", "*")
        .body("x")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200);

    // Both preconditions at once is a caller error, not a guess.
    let res = gw_req()
        .method("PUT")
        .path(path)
        .header("if-match", "*")
        .header("if-none-match", "*")
        .body("x")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 400);
}

/// correct CAS token (the LeanEpochOnlyHolds arm, now enforced).
#[tokio::test]
async fn gateway_manifest_cas_rejects_stale_epoch() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await); // epoch 1
    sc.checkout().await.unwrap();
    write(dir.path(), "f.txt", "v1");
    sc.run_barrier().await.unwrap();
    let loaded = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();

    // A successor deposes the cell to epoch 2.
    let state = store.epoch_read(&sc.cfg.epoch_key()).await.unwrap().unwrap();
    store.epoch_acquire(&sc.cfg.epoch_key(), "successor", Some(&state)).await.unwrap();

    let routes = routes(gw_core(&store));
    let mut doc = loaded.manifest.clone();
    doc.seq += 1;
    let body = serde_json::json!({
        "manifest": doc,
        "expected_etag": loaded.etag,
        "epoch": 1u64, // the deposed writer's claim
        "flush_uuid": "straggler",
    });
    let res = gw_req().method("POST").path("/lean/v1/proj1/manifest").json(&body).reply(&routes).await;
    assert_eq!(res.status(), 403, "stale epoch must be refused: {:?}", res.body());
    let after = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert_eq!(after.manifest.seq, loaded.manifest.seq, "the straggler CAS landed!");

    // The CURRENT epoch with the right token succeeds.
    let body = serde_json::json!({
        "manifest": doc,
        "expected_etag": loaded.etag,
        "epoch": 2u64,
        "flush_uuid": "successor",
    });
    let res = gw_req().method("POST").path("/lean/v1/proj1/manifest").json(&body).reply(&routes).await;
    assert_eq!(res.status(), 200, "{:?}", res.body());

    // And a CAS miss reports 409 with the current etag, never blind
    // re-seed semantics.
    let res = gw_req().method("POST").path("/lean/v1/proj1/manifest").json(&body).reply(&routes).await;
    assert_eq!(res.status(), 409);
}

/// status + snapshot surface the RPO/observability facts.
#[tokio::test]
async fn gateway_status_and_snapshot() {
    let store = Arc::new(MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "f.txt", "v1");
    sc.run_barrier().await.unwrap();
    hitl_write(&store, &sc.cfg, "pending.txt", "queued", "dilip").await.unwrap();

    let routes = routes(gw_core(&store));
    let res = gw_req().method("GET").path("/lean/v1/proj1/status").reply(&routes).await;
    assert_eq!(res.status(), 200);
    let v: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(v["seq"], 1);
    assert_eq!(v["inbox_depth"], 1);
    assert_eq!(v["epoch"], 1);
    assert!(v["window"].is_null());

    let res = gw_req().method("GET").path("/lean/v1/proj1/snapshot").reply(&routes).await;
    assert_eq!(res.status(), 200);
    let v: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert!(v["manifest"]["entries"]["f.txt"].is_object());
    assert_eq!(v["inbox"]["entries"][0]["path"], "pending.txt");
}

/// A published tree plus a live gateway. Returns (dir, routes) — the
/// dir must be kept alive or the tempdir unlinks under the syncer.
async fn draft_fixture(
    store: &Arc<MemoryStore>,
) -> (tempfile::TempDir, Syncer, warp::filters::BoxedFilter<(warp::reply::Response,)>) {
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(store, dir.path()).await;
    assert!(claim_until_held(&mut sc, 3).await);
    sc.checkout().await.unwrap();
    write(dir.path(), "inputs/wanted.txt", "published v1");
    write(dir.path(), "outputs/report.bin", "out v1");
    sc.run_barrier().await.unwrap();
    let routes = routes(gw_core(store));
    (dir, sc, routes)
}

async fn save_draft(
    routes: &warp::filters::BoxedFilter<(warp::reply::Response,)>,
    user: &str,
    path: &str,
    base: Option<&str>,
    body: &str,
) -> warp::http::Response<bytes::Bytes> {
    let mut req = gw_req()
        .method("PUT")
        .path(&format!("/lean/v1/proj1/drafts/{user}/{path}"))
        .body(body);
    if let Some(b) = base {
        req = req.header("x-flint-base-etag", b);
    }
    req.reply(routes).await
}

/// The whole promise, in one test: a saved draft is durable in the
/// BUCKET and invisible everywhere else. Not in the agent's tree, not
/// in the manifest, and not in the published object — which is what
/// separates a draft from `PUT /files/{path}`, whose bytes are live the
/// instant it returns.
#[tokio::test]
async fn a_saved_draft_is_durable_in_s3_and_invisible_until_promoted() {
    let store = Arc::new(MemoryStore::new());
    let (dir, mut sc, routes) = draft_fixture(&store).await;
    let base = current_etag(&store, &sc.cfg, "inputs/wanted.txt").await;

    let res = save_draft(&routes, "alice", "inputs/wanted.txt", Some(&base), "alice's edit").await;
    assert_eq!(res.status(), 200, "{:?}", res.body());

    // Durable: both objects are in the bucket.
    let body_key = sc.cfg.draft_body_key("alice", "inputs/wanted.txt");
    let meta_key = sc.cfg.draft_meta_key("alice", "inputs/wanted.txt");
    assert!(store.head(&body_key).await.is_ok(), "the draft body must be in the bucket");
    assert!(store.head(&meta_key).await.is_ok(), "the draft meta must be in the bucket");

    // Invisible: two barriers (the deletion rule needs two scans, so one
    // could pass for the wrong reason) change nothing about the file.
    sc.run_barrier().await.unwrap();
    sc.run_barrier().await.unwrap();
    assert_eq!(read(dir.path(), "inputs/wanted.txt").as_deref(), Some("published v1"));
    assert_eq!(
        current_etag(&store, &sc.cfg, "inputs/wanted.txt").await,
        base,
        "a draft must not move the published object"
    );
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;
    assert_eq!(m.entries["inputs/wanted.txt"].etag, base, "a draft must not move the citation");
    assert!(
        !m.entries.keys().any(|k| k.contains("drafts")),
        "a draft must never be cited: {:?}",
        m.entries.keys().collect::<Vec<_>>()
    );

    // And it reads back, with the base it was taken against.
    let res = gw_req()
        .method("GET")
        .path("/lean/v1/proj1/drafts/alice/inputs/wanted.txt")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200);
    assert_eq!(&res.body()[..], b"alice's edit");
    assert_eq!(res.headers()["x-flint-base-etag"].to_str().unwrap(), base);
}

/// Promote is the publish: it lands at the live key, the inbox tracks
/// it, the barrier cites it and the agent gets the bytes — and the
/// draft is gone afterwards, so a second promote cannot republish it.
#[tokio::test]
async fn promote_publishes_the_draft_and_the_barrier_cites_it() {
    let store = Arc::new(MemoryStore::new());
    let (dir, mut sc, routes) = draft_fixture(&store).await;
    let base = current_etag(&store, &sc.cfg, "inputs/wanted.txt").await;
    save_draft(&routes, "alice", "inputs/wanted.txt", Some(&base), "alice's edit").await;

    let res = gw_req()
        .method("POST")
        .path("/lean/v1/proj1/drafts/alice/inputs/wanted.txt")
        .header("x-flint-author", "alice")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200, "{:?}", res.body());

    // The bytes are LIVE at the real key immediately: read the object
    // directly, below any door.
    let (_, live) = store.get_whole(&sc.cfg.file_key("inputs/wanted.txt"), None).await.unwrap();
    assert_eq!(&live[..], b"alice's edit", "promote must publish at the live key at once");

    // `GET /files/{path}` serves the promoted bytes at once: the read
    // door overlays the inbox entry on the manifest citation, as the
    // listing and the sync verb do. (Before 0.2.1 it preferred the
    // citation and answered 409 `moved` until the barrier re-cited.)
    //
    // THE CONTROL, and the reason a defect here would not be a drafts
    // defect: the ordinary HITL door reads a second path in the same
    // fixture through the same door.
    let res = gw_req().method("GET").path("/lean/v1/proj1/files/inputs/wanted.txt").reply(&routes).await;
    assert_eq!(res.status(), 200, "{:?}", res.body());
    assert_eq!(&res.body()[..], b"alice's edit", "the promoted bytes, through the read door, before any barrier");

    let b = current_etag(&store, &sc.cfg, "outputs/report.bin").await;
    let res = gw_req()
        .method("PUT")
        .path("/lean/v1/proj1/files/outputs/report.bin")
        .header("if-match", &b)
        .body("plain HITL overwrite")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200, "{:?}", res.body());
    let res = gw_req().method("GET").path("/lean/v1/proj1/files/outputs/report.bin").reply(&routes).await;
    assert_eq!(res.status(), 200, "the plain HITL door must answer the same as promote's: {:?}", res.body());
    assert_eq!(&res.body()[..], b"plain HITL overwrite");

    // The barrier consumes both entries and cites them; the read is the
    // same bytes, now through the citation.
    let r = sc.run_barrier().await.unwrap();
    assert_eq!(r.consumed, 2, "promote must land an inbox entry like any HITL write");
    let res = gw_req().method("GET").path("/lean/v1/proj1/files/inputs/wanted.txt").reply(&routes).await;
    assert_eq!(res.status(), 200, "{:?}", res.body());
    assert_eq!(&res.body()[..], b"alice's edit");
    assert_eq!(read(dir.path(), "inputs/wanted.txt").as_deref(), Some("alice's edit"));
    let m = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest;
    assert_ne!(m.entries["inputs/wanted.txt"].etag, base);

    // The draft is consumed: nothing left to promote twice.
    let res = gw_req().method("GET").path("/lean/v1/proj1/drafts/alice").reply(&routes).await;
    let list: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(list["drafts"].as_array().unwrap().len(), 0, "{list}");
}

/// THE TWO-USER CASE, and the reason the base etag is recorded rather
/// than enforced at save time.
///
/// Alice opens v1 and drafts. Bob publishes v2. Alice comes back —
/// possibly days later, from a browser that has forgotten everything —
/// and promotes. That must refuse, must name the version it found, and
/// must KEEP alice's work: a refusal that discarded the draft would
/// destroy exactly what the feature exists to protect.
#[tokio::test]
async fn a_sibling_publish_refuses_the_promote_and_keeps_the_draft() {
    let store = Arc::new(MemoryStore::new());
    let (_dir, sc, routes) = draft_fixture(&store).await;
    let base = current_etag(&store, &sc.cfg, "inputs/wanted.txt").await;
    save_draft(&routes, "alice", "inputs/wanted.txt", Some(&base), "alice's edit").await;

    // Bob publishes through the ordinary HITL door.
    let res = gw_req()
        .method("PUT")
        .path("/lean/v1/proj1/files/inputs/wanted.txt")
        .header("x-flint-author", "bob")
        .header("if-match", &base)
        .body("bob's edit")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200, "{:?}", res.body());
    let bob = current_etag(&store, &sc.cfg, "inputs/wanted.txt").await;
    assert_ne!(bob, base);

    let res = gw_req()
        .method("POST")
        .path("/lean/v1/proj1/drafts/alice/inputs/wanted.txt")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 409, "{:?}", res.body());
    let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(body["error"], "draft-stale", "{body}");
    assert_eq!(
        res.headers()["x-flint-current-etag"].to_str().unwrap(),
        bob,
        "the refusal must name what it found, or the caller cannot reconcile"
    );

    // Bob's bytes stand.
    assert_eq!(current_etag(&store, &sc.cfg, "inputs/wanted.txt").await, bob);
    // And alice's work is STILL THERE.
    let res = gw_req()
        .method("GET")
        .path("/lean/v1/proj1/drafts/alice/inputs/wanted.txt")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200, "a refused promote must never discard the draft");
    assert_eq!(&res.body()[..], b"alice's edit");
}

/// A draft whose base says "this file did not exist" promotes as a
/// CREATE, and is refused if the file appeared meanwhile. This is also
/// the fail-closed arm for a caller that simply forgot the header:
/// absent base ⇒ create-if-absent ⇒ an existing file refuses, rather
/// than being clobbered by a write that named nothing.
#[tokio::test]
async fn a_draft_with_no_base_creates_and_refuses_to_clobber() {
    let store = Arc::new(MemoryStore::new());
    let (_dir, sc, routes) = draft_fixture(&store).await;

    save_draft(&routes, "alice", "inputs/brand-new.txt", None, "new file").await;
    let res = gw_req()
        .method("POST")
        .path("/lean/v1/proj1/drafts/alice/inputs/brand-new.txt")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200, "a create-shaped draft must publish: {:?}", res.body());

    // The mutation arm: the same shape over a file that DOES exist.
    save_draft(&routes, "alice", "inputs/wanted.txt", None, "clobber attempt").await;
    let before = current_etag(&store, &sc.cfg, "inputs/wanted.txt").await;
    let res = gw_req()
        .method("POST")
        .path("/lean/v1/proj1/drafts/alice/inputs/wanted.txt")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 409, "{:?}", res.body());
    assert_eq!(
        current_etag(&store, &sc.cfg, "inputs/wanted.txt").await,
        before,
        "a baseless draft must never overwrite an existing file"
    );
}

/// The crash residue between the two PUTs is a body with no meta. It is
/// READABLE — the bytes are the user's work — but it must not promote,
/// because promoting it would have to guess a base, and each guess is
/// wrong in exactly the case the other is right.
#[tokio::test]
async fn an_incomplete_draft_reads_but_refuses_to_promote() {
    let store = Arc::new(MemoryStore::new());
    let (_dir, sc, routes) = draft_fixture(&store).await;
    let base = current_etag(&store, &sc.cfg, "inputs/wanted.txt").await;
    save_draft(&routes, "alice", "inputs/wanted.txt", Some(&base), "alice's edit").await;

    // Simulate the crash: the meta never landed.
    store.delete(&sc.cfg.draft_meta_key("alice", "inputs/wanted.txt")).await.unwrap();

    let res = gw_req()
        .method("GET")
        .path("/lean/v1/proj1/drafts/alice/inputs/wanted.txt")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200, "the bytes are still the user's work");
    assert!(res.headers().contains_key("x-flint-draft-incomplete"), "and the caller is told");

    let res = gw_req()
        .method("POST")
        .path("/lean/v1/proj1/drafts/alice/inputs/wanted.txt")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 404, "{:?}", res.body());
    assert_eq!(
        current_etag(&store, &sc.cfg, "inputs/wanted.txt").await,
        base,
        "an incomplete draft must publish nothing"
    );
}

/// Promote IS a HITL write, so it takes the HITL discipline: refused
/// while a barrier window is live, admitted once the window clears.
/// Without the arm that clears it, a test asserting only the 409 would
/// pass against a promote that ALWAYS refuses.
#[tokio::test]
async fn a_promote_is_refused_while_the_barrier_window_is_open() {
    let store = Arc::new(MemoryStore::new());
    let (_dir, sc, routes) = draft_fixture(&store).await;
    let base = current_etag(&store, &sc.cfg, "inputs/wanted.txt").await;
    save_draft(&routes, "alice", "inputs/wanted.txt", Some(&base), "alice's edit").await;

    inbox::open_window(store.as_ref(), &sc.cfg, 1, now_unix() + 120).await.unwrap();
    let res = gw_req()
        .method("POST")
        .path("/lean/v1/proj1/drafts/alice/inputs/wanted.txt")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 409);
    assert!(res.headers().contains_key("retry-after"));
    assert_eq!(
        current_etag(&store, &sc.cfg, "inputs/wanted.txt").await,
        base,
        "a windowed refusal must publish nothing"
    );

    inbox::clear_window(store.as_ref(), &sc.cfg, 1, &[]).await.unwrap();
    let res = gw_req()
        .method("POST")
        .path("/lean/v1/proj1/drafts/alice/inputs/wanted.txt")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200, "the window must not wedge promote forever: {:?}", res.body());
}

/// The arm that justifies the UNCONDITIONAL save. A second tab re-saves
/// between the first tab's read of the meta and its copy; the first
/// tab's promote must not publish bytes it never saw. It fails on the
/// COPY SOURCE guard — `draft-moved`, distinct from `draft-stale`,
/// because the fix is different: retry, rather than reconcile.
#[tokio::test]
async fn a_promote_racing_a_re_save_publishes_neither_silently() {
    let store = Arc::new(MemoryStore::new());
    let (_dir, sc, routes) = draft_fixture(&store).await;
    let base = current_etag(&store, &sc.cfg, "inputs/wanted.txt").await;
    save_draft(&routes, "alice", "inputs/wanted.txt", Some(&base), "tab one").await;

    // The meta as tab one read it, then tab two re-saves under it.
    let meta_key = sc.cfg.draft_meta_key("alice", "inputs/wanted.txt");
    let (_, stale_meta) = store.get_whole(&meta_key, None).await.unwrap();
    save_draft(&routes, "alice", "inputs/wanted.txt", Some(&base), "tab two").await;

    // Put tab one's meta back: now the recorded body_etag names bytes
    // the body no longer has — exactly the race, deterministically.
    let crc = crc64_nvme(&stale_meta);
    store
        .put_whole(
            &meta_key,
            stale_meta,
            &PutCondition::Unconditional,
            &GenerationStamps {
                generation: 0,
                epoch: 0,
                flush_uuid: "test-restage".into(),
                boundary_source: None,
                posix: None,
            },
            crc,
        )
        .await
        .unwrap();

    let res = gw_req()
        .method("POST")
        .path("/lean/v1/proj1/drafts/alice/inputs/wanted.txt")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 409, "{:?}", res.body());
    let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(body["error"], "draft-moved", "{body}");
    assert_eq!(
        current_etag(&store, &sc.cfg, "inputs/wanted.txt").await,
        base,
        "a racing promote must publish nothing at all"
    );
}

/// Two users, one path: separate drafts, separate listings. And the
/// control for the DISJOINT-SUBTREE key layout — a file legally named
/// `notes.meta` must not collide with `notes`'s metadata, which is
/// exactly what a `<path>.meta` suffix scheme would have done.
#[tokio::test]
async fn drafts_are_per_user_and_a_dotmeta_filename_does_not_collide() {
    let store = Arc::new(MemoryStore::new());
    let (_dir, _sc, routes) = draft_fixture(&store).await;

    save_draft(&routes, "alice", "notes", None, "alice on notes").await;
    save_draft(&routes, "bob", "notes", None, "bob on notes").await;
    save_draft(&routes, "alice", "notes.meta", None, "a file that is NOT metadata").await;

    for (user, path, want) in [
        ("alice", "notes", "alice on notes"),
        ("bob", "notes", "bob on notes"),
        ("alice", "notes.meta", "a file that is NOT metadata"),
    ] {
        let res = gw_req()
            .method("GET")
            .path(&format!("/lean/v1/proj1/drafts/{user}/{path}"))
            .reply(&routes)
            .await;
        assert_eq!(res.status(), 200, "{user}/{path}");
        assert_eq!(&res.body()[..], want.as_bytes(), "{user}/{path}");
    }

    let res = gw_req().method("GET").path("/lean/v1/proj1/drafts/bob").reply(&routes).await;
    let list: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    let paths: Vec<&str> =
        list["drafts"].as_array().unwrap().iter().map(|d| d["path"].as_str().unwrap()).collect();
    assert_eq!(paths, vec!["notes"], "one user's listing must not carry another's: {list}");
}

/// The resume view a user comes back to: which of these can still be
/// published? Both arms, so a `stale` hardwired either way fails.
#[tokio::test]
async fn the_resume_view_marks_exactly_the_stale_draft() {
    let store = Arc::new(MemoryStore::new());
    let (_dir, sc, routes) = draft_fixture(&store).await;
    let a = current_etag(&store, &sc.cfg, "inputs/wanted.txt").await;
    let b = current_etag(&store, &sc.cfg, "outputs/report.bin").await;
    save_draft(&routes, "alice", "inputs/wanted.txt", Some(&a), "edit a").await;
    save_draft(&routes, "alice", "outputs/report.bin", Some(&b), "edit b").await;

    // Only ONE of the two moves underneath.
    gw_req()
        .method("PUT")
        .path("/lean/v1/proj1/files/inputs/wanted.txt")
        .header("if-match", &a)
        .body("sibling wrote")
        .reply(&routes)
        .await;

    let res = gw_req().method("GET").path("/lean/v1/proj1/drafts/alice").reply(&routes).await;
    let list: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    let rows = list["drafts"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{list}");
    for r in rows {
        let stale = r["stale"].as_bool().unwrap();
        match r["path"].as_str().unwrap() {
            "inputs/wanted.txt" => assert!(stale, "the moved one must read stale: {list}"),
            "outputs/report.bin" => assert!(!stale, "the untouched one must not: {list}"),
            p => panic!("unexpected {p}"),
        }
    }
}

/// Drafts sit under `LEAN_DIR`, and the ONLY reason that is safe is
/// that both sweeps are prefix-scoped. If either ever widens to the
/// namespace root, every unpublished draft in the fleet is collected
/// silently.
///
/// The control is an object the sweep DOES take, planted in the same
/// namespace: an unreferenced chunk, with the grace set to zero so it
/// is collectable now. One dimension moves — which subtree the object
/// sits in — and the sweep must take one and leave the other. Without
/// that arm a sweep that collected NOTHING would pass this test.
#[tokio::test]
async fn neither_sweep_collects_a_draft() {
    let store = Arc::new(MemoryStore::new());
    let (dir, mut sc, routes) = draft_fixture(&store).await;
    save_draft(&routes, "alice", "inputs/wanted.txt", None, "alice's edit").await;
    let body_key = sc.cfg.draft_body_key("alice", "inputs/wanted.txt");
    let meta_key = sc.cfg.draft_meta_key("alice", "inputs/wanted.txt");

    // Churn, so the sweeps have a real workspace to reason about.
    for i in 0..3 {
        write(dir.path(), &format!("churn-{i}.txt"), &format!("v{i}"));
        sc.run_barrier().await.unwrap();
    }

    // The control: an orphan chunk, immediately collectable.
    sc.cfg.orphan_grace_secs = 0;
    let orphan = format!("{}/{}/chunks/deadbeefdeadbeef", sc.cfg.prefix, LEAN_DIR);
    let bytes = Bytes::from_static(b"not referenced by any pointer");
    let crc = crc64_nvme(&bytes);
    store
        .put_whole(
            &orphan,
            bytes,
            &PutCondition::Unconditional,
            &GenerationStamps {
                generation: 0,
                epoch: 0,
                flush_uuid: "test-orphan".into(),
                boundary_source: None,
                posix: None,
            },
            crc,
        )
        .await
        .unwrap();

    manifest::sweep_generations(store.as_ref(), &sc.cfg).await.unwrap();
    let taken = manifest::sweep_chunks(store.as_ref(), &sc.cfg).await.unwrap();

    assert!(taken > 0, "the control was not collected — this test proves nothing");
    assert!(store.head(&orphan).await.is_err(), "the control must be gone");
    assert!(store.head(&body_key).await.is_ok(), "a sweep collected the draft BODY");
    assert!(store.head(&meta_key).await.is_ok(), "a sweep collected the draft META");
}

/// Discard. Meta first, so the window leaves the one incomplete shape
/// the rest of the module already handles.
#[tokio::test]
async fn a_discarded_draft_leaves_nothing_behind() {
    let store = Arc::new(MemoryStore::new());
    let (_dir, sc, routes) = draft_fixture(&store).await;
    save_draft(&routes, "alice", "inputs/wanted.txt", None, "alice's edit").await;

    let res = gw_req()
        .method("DELETE")
        .path("/lean/v1/proj1/drafts/alice/inputs/wanted.txt")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 204);
    assert!(store.head(&sc.cfg.draft_body_key("alice", "inputs/wanted.txt")).await.is_err());
    assert!(store.head(&sc.cfg.draft_meta_key("alice", "inputs/wanted.txt")).await.is_err());

    let res = gw_req()
        .method("POST")
        .path("/lean/v1/proj1/drafts/alice/inputs/wanted.txt")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 404, "a discarded draft must not promote");
}

/// Path and user hygiene: the draft door reserves exactly what the
/// files door reserves, and a user id may not be a path segment of its
/// own — `drafts/../..` must not address another workspace's keys.
#[tokio::test]
async fn the_draft_door_refuses_reserved_paths_and_bad_users() {
    let store = Arc::new(MemoryStore::new());
    let (_dir, _sc, routes) = draft_fixture(&store).await;

    for bad in ["../../etc/passwd", ".flint/lean/manifest", ".flint-sync/baseline.json"] {
        let res = save_draft(&routes, "alice", bad, None, "x").await;
        assert_eq!(res.status(), 400, "draft path {bad:?} must be refused");
    }
    for bad in ["..", "."] {
        let res = save_draft(&routes, bad, "a.txt", None, "x").await;
        assert_eq!(res.status(), 400, "user {bad:?} must be refused");
    }
    let res = warp::test::request()
        .method("PUT")
        .path("/lean/v1/proj1/drafts/alice/a.txt")
        .body("x")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 401, "the draft door is behind the same bearer");
}

/// An unpromoted draft of an OUT-OF-SCOPE path changes nothing about a
/// scoped workspace. This is the property that actually holds, and the
/// one worth guarding: a draft is not a citation, so it cannot widen an
/// admitted set.
#[tokio::test]
async fn an_unpromoted_draft_never_widens_a_scoped_workspace() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let routes = routes(gw_core(&store));

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    b.checkout_scoped(Some(vec!["inputs".into()])).await.unwrap();
    assert!(claim_until_held(&mut b, 12).await);
    let admitted: Vec<String> = b.state.load_baseline().unwrap().entries.keys().cloned().collect();

    save_draft(&routes, "alice", "outputs/big-0.bin", None, "drafted out of scope").await;

    // Two barriers: absence must survive two scans, so one could pass
    // for the wrong reason.
    let r1 = b.run_barrier().await.unwrap();
    let r2 = b.run_barrier().await.unwrap();
    assert!(r1.deleted.is_empty() && r2.deleted.is_empty(), "{:?} {:?}", r1.deleted, r2.deleted);
    assert_eq!(r1.consumed, 0, "an unpromoted draft is not an inbox entry");
    assert!(read(dir_b.path(), "outputs/big-0.bin").is_none(), "a draft must not materialise");
    assert_eq!(
        b.state.load_baseline().unwrap().entries.keys().cloned().collect::<Vec<_>>(),
        admitted,
        "a draft must not widen the admitted set"
    );
    assert_eq!(b.state.load_scope().unwrap().as_deref(), Some(&["inputs".to_string()][..]));
}

/// The complement, and an HONEST one: PROMOTING an out-of-scope draft
/// DOES widen a scoped workspace's held set.
///
/// Not a defect of drafts — promote lands an ordinary inbox entry, and
/// merge → inbox → consume is the DESIGNED destination for out-of-scope
/// foreign changes (`sync.rs:13-22`). The gateway is stateless and
/// reads only the bucket, while the scope is LOCAL to the pod
/// (`scope.json`), so the gateway could not filter on it even if that
/// were wanted. Promote is a fourth mouth on the door the inbox already
/// is, and the consequence is the one probe P3 confirmed: once a path
/// is in the baseline, removing it locally publishes the object DELETE.
///
/// This test exists so the behaviour is PINNED rather than rediscovered.
#[tokio::test]
async fn promoting_an_out_of_scope_draft_widens_the_held_set() {
    let store = Arc::new(MemoryStore::new());
    let _keep = scoped_fixture(&store).await;
    let routes = routes(gw_core(&store));

    let dir_b = tempfile::tempdir().unwrap();
    let mut b = syncer(&store, dir_b.path()).await;
    b.checkout_scoped(Some(vec!["inputs".into()])).await.unwrap();
    assert!(claim_until_held(&mut b, 12).await);
    assert!(!b.state.load_baseline().unwrap().entries.contains_key("outputs/big-0.bin"));

    let base = current_etag(&store, &b.cfg, "outputs/big-0.bin").await;
    save_draft(&routes, "alice", "outputs/big-0.bin", Some(&base), "promoted out of scope").await;
    let res = gw_req()
        .method("POST")
        .path("/lean/v1/proj1/drafts/alice/outputs/big-0.bin")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200, "{:?}", res.body());

    let r = b.run_barrier().await.unwrap();
    assert_eq!(r.consumed, 1);
    assert_eq!(
        read(dir_b.path(), "outputs/big-0.bin").as_deref(),
        Some("promoted out of scope"),
        "consume materialises an out-of-scope promote — the documented widening"
    );
    assert!(
        b.state.load_baseline().unwrap().entries.contains_key("outputs/big-0.bin"),
        "and it is now CITED, which is what makes the widening load-bearing"
    );
    // The scope RECORD is untouched: what drifts is the held set, not
    // the declaration. That asymmetry is the design's, not a bug here.
    assert_eq!(b.state.load_scope().unwrap().as_deref(), Some(&["inputs".to_string()][..]));
}

// ── delete and rename through the wire, performed by a real barrier ──

/// DELETE records a removal, POST /rename copies and records both
/// halves, and ONE barrier cites the delete out and the rename across
/// in ONE manifest generation. The HTTP reads agree at every step:
/// the destination reads before the barrier, the old names 404 after.
#[tokio::test]
async fn a_ui_delete_and_a_rename_ride_one_barrier_through_the_wire() {
    let store = Arc::new(MemoryStore::new());
    let (_dir, mut sc, routes) = draft_fixture(&store).await;
    let before = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap().manifest.seq;

    // A stale If-Match: 412 with the current tag; nothing recorded.
    let res = gw_req()
        .method("DELETE")
        .path("/lean/v1/proj1/files/outputs/report.bin")
        .header("if-match", "\"nope\"")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 412);
    assert!(res.headers().get("etag").is_some());
    // The delete: 204 as soon as the intent is durable.
    let res = gw_req()
        .method("DELETE")
        .path("/lean/v1/proj1/files/outputs/report.bin")
        .header("x-flint-author", "dilip")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 204, "{:?}", res.body());
    // The rename: 200 {etag}; the destination reads at once.
    let res = gw_req()
        .method("POST")
        .path("/lean/v1/proj1/rename")
        .header("content-type", "application/json")
        .body(r#"{"from":"inputs/wanted.txt","to":"inputs/renamed.txt"}"#)
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 200, "{:?}", res.body());
    let res = gw_req().method("GET").path("/lean/v1/proj1/files/inputs/renamed.txt").reply(&routes).await;
    assert_eq!(res.status(), 200);
    assert_eq!(&res.body()[..], b"published v1");
    // A source under a pending removal is gone to a second rename.
    let res = gw_req()
        .method("POST")
        .path("/lean/v1/proj1/rename")
        .header("content-type", "application/json")
        .body(r#"{"from":"outputs/report.bin","to":"outputs/again.bin"}"#)
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 404);
    let res = gw_req().method("GET").path("/lean/v1/proj1/status").reply(&routes).await;
    let v: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(v["removals_pending"], 2);
    assert_eq!(v["removals_refused"], 0);

    // ONE barrier, ONE generation, both halves.
    let r = sc.run_barrier().await.unwrap();
    assert_eq!(r.consumed, 1);
    assert_eq!(r.removed.len(), 2, "{r:?}");
    let after = manifest::load(store.as_ref(), &sc.cfg).await.unwrap().unwrap();
    assert_eq!(after.manifest.seq, before + 1);
    assert!(after.manifest.entries.contains_key("inputs/renamed.txt"));
    assert!(!after.manifest.entries.contains_key("inputs/wanted.txt"));
    assert!(!after.manifest.entries.contains_key("outputs/report.bin"));
    for gone in ["inputs/wanted.txt", "outputs/report.bin"] {
        let res = gw_req().method("GET").path(&format!("/lean/v1/proj1/files/{gone}")).reply(&routes).await;
        assert_eq!(res.status(), 404, "{gone}");
        assert!(store.head(&sc.cfg.file_key(gone)).await.is_err(), "{gone} GC'd");
    }
    let res = gw_req().method("GET").path("/lean/v1/proj1/files/inputs/renamed.txt").reply(&routes).await;
    assert_eq!(res.status(), 200);
    // Settled: nothing left to withdraw.
    let res = gw_req().method("DELETE").path("/lean/v1/proj1/removals/outputs/report.bin").reply(&routes).await;
    assert_eq!(res.status(), 404);
    let v: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(v["error"], "no-removal");
}

/// A removal the syncer REFUSES — the agent has unpublished edits on
/// the path — is answered in the cell, where `/snapshot` shows it with
/// the reason and the author, and `/status` counts it.
#[tokio::test]
async fn a_refused_removal_is_readable_through_the_wire() {
    let store = Arc::new(MemoryStore::new());
    let (dir, mut sc, routes) = draft_fixture(&store).await;
    write(dir.path(), "inputs/wanted.txt", "the agent's unpublished edit");
    let res = gw_req()
        .method("DELETE")
        .path("/lean/v1/proj1/files/inputs/wanted.txt")
        .header("x-flint-author", "dilip")
        .reply(&routes)
        .await;
    assert_eq!(res.status(), 204);
    let r = sc.run_barrier().await.unwrap();
    assert_eq!(r.removals_refused, 1);
    assert!(r.removed.is_empty());
    assert_eq!(read(dir.path(), "inputs/wanted.txt").unwrap(), "the agent's unpublished edit");
    let res = gw_req().method("GET").path("/lean/v1/proj1/snapshot").reply(&routes).await;
    let v: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    let removals = v["inbox"]["removals"].as_array().unwrap();
    assert_eq!(removals.len(), 1);
    assert_eq!(removals[0]["path"], "inputs/wanted.txt");
    assert_eq!(removals[0]["author"], "dilip");
    assert_eq!(removals[0]["refused"]["kind"], "removal-refused-dirty");
    let res = gw_req().method("GET").path("/lean/v1/proj1/status").reply(&routes).await;
    let v: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(v["removals_pending"], 0);
    assert_eq!(v["removals_refused"], 1);
    // Withdrawing a refused record clears it.
    let res = gw_req().method("DELETE").path("/lean/v1/proj1/removals/inputs/wanted.txt").reply(&routes).await;
    assert_eq!(res.status(), 204);
    let res = gw_req().method("GET").path("/lean/v1/proj1/status").reply(&routes).await;
    let v: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(v["removals_refused"], 0);
}

/// Phase D control: the path rule is ONE predicate. Every traversal or
/// reserved path in the table is refused identically through the HTTP
/// route and the library verb, and a clean path reaches the verb on
/// both sides (404, not 400) — a second copy of the rule would let the
/// two drift and still pass.
#[tokio::test]
async fn a_bad_path_is_refused_identically_by_the_wire_and_the_library() {
    use flint_lean_gateway::{VerbError, Workspace};
    let store = Arc::new(MemoryStore::new());
    let routes = routes(gw_core(&store));
    let ws = Workspace::new(store.clone() as Arc<dyn ObjectStore>, PREFIX);
    let table = ["../x", "a/../b", "a/./b", ".flint/x", ".flint", ".flint-sync/state"];
    for bad in table {
        let res = gw_req().method("DELETE").path(&format!("/lean/v1/proj1/files/{bad}")).reply(&routes).await;
        assert_eq!(res.status(), 400, "wire DELETE {bad}");
        let v: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        assert_eq!(v["error"], "bad-path", "{bad}");
        let err = ws.remove_file(bad, None, None).await.unwrap_err();
        assert!(matches!(err, VerbError::BadPath(_)), "library remove {bad}: {err}");

        let res = gw_req()
            .method("POST")
            .path("/lean/v1/proj1/rename")
            .header("content-type", "application/json")
            .body(format!(r#"{{"from":"ok.txt","to":"{bad}"}}"#))
            .reply(&routes)
            .await;
        assert_eq!(res.status(), 400, "wire rename to {bad}");
        let err = ws.rename_file("ok.txt", bad, None).await.unwrap_err();
        assert!(matches!(err, VerbError::BadPath(_)), "library rename to {bad}: {err}");
    }
    // The positive control: a clean path gets past the rule on both sides.
    let res = gw_req().method("DELETE").path("/lean/v1/proj1/files/clean/ok.txt").reply(&routes).await;
    assert_eq!(res.status(), 404);
    assert!(matches!(ws.remove_file("clean/ok.txt", None, None).await.unwrap_err(), VerbError::NoSuchFile(_)));
}
