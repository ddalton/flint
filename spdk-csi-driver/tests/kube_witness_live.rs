//! THE WITNESS AGAINST A REAL API SERVER.
//!
//! `witness_kube.rs`'s unit tests prove the SEMANTICS: given a store
//! that conflicts, the promotion re-reads and refuses; given a store
//! that returns junk, the record refuses rather than reading as absent.
//! Every one of them runs against an in-memory store I wrote, which
//! means every one of them assumes the answer to the question the whole
//! design rests on:
//!
//!   **does a Kubernetes merge-patch carrying `metadata.resourceVersion`
//!   actually get refused when that version has moved?**
//!
//! If it does not — if the API server ignores the field on a patch, or
//! our patch is shaped so the check never runs — then every write lands,
//! the CAS is decoration, and `FlintCompositionWitness.cfg` is green
//! about a system we did not build. No unit test can settle that. This
//! drill settles it, and the rest of what only a live server knows:
//! that our derived object names are legal, that the label selector
//! `list` uses agrees with the label `put` writes, and that the Role in
//! the chart grants exactly the verbs the store calls.
//!
//! # It runs AS THE SERVICE ACCOUNT, on purpose
//!
//! `tests/regression/kind-witness-pass.sh` mints a token for
//! `flint-pnfs-mds` and points `KUBECONFIG` at it before running these.
//! A verb the chart forgot to grant then fails HERE, in the same call
//! production would make, instead of at a failover six months from now.
//! Run as cluster-admin these still pass except the namespacing leg,
//! which is the leg that checks the credential is small.
//!
//! # Running it
//!
//!   make test-kind-witness            # creates/uses a kind cluster
//!
//! or by hand against any cluster you do not mind writing ConfigMaps in:
//!
//!   FLINT_WITNESS_NS=flint-system cargo test --test kube_witness_live \
//!     -- --ignored --test-threads=1
//!
//! Every test is `#[ignore]`d: they need a cluster, so they must never
//! run in the ordinary gate.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use spdk_csi_driver::pnfs::mds::witness::CompositionWitness;
use spdk_csi_driver::pnfs::mds::witness_kube::{
    object_name, ConfigMapStore, DocStore, KubeWitness, PutOutcome, KIND_COMPOSITION, KIND_TARGET,
};
use spdk_csi_driver::state_backend::extent_alloc::{ExtentAllocError, LEG_INSYNC};

/// The namespace under test. Required rather than defaulted: this drill
/// WRITES, and a default would eventually write somewhere unintended.
fn ns() -> String {
    std::env::var("FLINT_WITNESS_NS")
        .expect("FLINT_WITNESS_NS must name the namespace to arbitrate in")
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// A per-run suffix so two runs against the same cluster cannot collide
/// — and so a failed run's wreckage is attributable.
fn tag() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
    format!("{}-{:x}", now() % 100_000, nanos)
}

async fn store() -> ConfigMapStore {
    ConfigMapStore::new(&ns()).await.expect("kube client")
}

async fn witness() -> KubeWitness {
    KubeWitness::new(Arc::new(store().await) as Arc<dyn DocStore>)
}

/// Remove what a leg made. Called on the way out of every test; a leg
/// that fails early leaves its objects behind ON PURPOSE, labelled and
/// findable with `kubectl get cm -l chert.us/kind=composition`.
async fn sweep(vols: &[String], targets: &[String]) {
    let s = store().await;
    for v in vols {
        let _ = s.delete(&object_name(KIND_COMPOSITION, v)).await;
    }
    for t in targets {
        let _ = s.delete(&object_name(KIND_TARGET, t)).await;
    }
}

// ── leg 1: THE CAS IS REAL ───────────────────────────────────────────

/// The one fact the whole design rests on, asked of the server directly.
///
/// Two writes are decided against the SAME resourceVersion. The first
/// must land and the second must be refused — not merged, not last-write-
/// wins. Everything else in this file is detail next to this assertion:
/// if it fails, the witness serializes nothing and two targets can both
/// believe they compose the volume.
#[tokio::test]
#[ignore = "needs a Kubernetes API server (make test-kind-witness)"]
async fn a_stale_write_is_refused_by_the_api_server() {
    let vol = format!("cas-{}", tag());
    let s = store().await;
    let name = object_name(KIND_COMPOSITION, &vol);
    let _ = s.delete(&name).await;

    assert_eq!(
        s.put(&name, r#"{"volume":"one"}"#, None).await.unwrap(),
        PutOutcome::Wrote,
        "create-if-absent did not land on a name nothing holds"
    );
    // Creating it again is the same conflict wearing a 409 AlreadyExists:
    // seating a volume is insert-if-absent, and two shards seating at
    // once must not both succeed.
    assert_eq!(
        s.put(&name, r#"{"volume":"two"}"#, None).await.unwrap(),
        PutOutcome::Conflict,
        "the API server accepted a second CREATE of the same object"
    );

    let (body, rv) = s.get(&name).await.unwrap().expect("the record we just wrote");
    assert!(body.contains("one"), "the losing create overwrote the record: {body}");

    assert_eq!(
        s.put(&name, r#"{"volume":"three"}"#, Some(&rv)).await.unwrap(),
        PutOutcome::Wrote,
        "a write against the CURRENT version was refused"
    );
    // `rv` is now stale by exactly one write — the shape of every lost
    // race in the model.
    assert_eq!(
        s.put(&name, r#"{"volume":"four"}"#, Some(&rv)).await.unwrap(),
        PutOutcome::Conflict,
        "THE CAS DOES NOT CAS: the API server accepted a write decided \
         against a resourceVersion that had already moved. Every failover \
         property in FlintComposition.tla assumes this write is refused."
    );
    let (body, _) = s.get(&name).await.unwrap().unwrap();
    assert!(
        body.contains("three"),
        "the refused write still changed the record: {body}"
    );

    sweep(&[vol], &[]).await;
}

// ── leg 2: the credential is small ───────────────────────────────────

/// The Role is namespaced on purpose, and this is the leg that knows
/// whether it is. Run as the ServiceAccount, a read in another namespace
/// must be REFUSED BY THE SERVER.
///
/// It must also be refused as UNREACHABLE and never as a record-level
/// refusal: a refusal is a decision, and a 403 mistaken for one would
/// let an RBAC mistake read as "this volume has no seat" — the exact
/// misreading that mints a second composition.
#[tokio::test]
#[ignore = "needs a Kubernetes API server (make test-kind-witness)"]
async fn the_witness_credential_cannot_leave_its_namespace() {
    let other = if ns() == "default" { "kube-public" } else { "default" };
    let s = ConfigMapStore::new(other).await.expect("kube client");
    match s.list(KIND_COMPOSITION).await {
        Ok(_) => panic!(
            "listing composition records in '{other}' SUCCEEDED — the witness \
             credential is not confined to its namespace (are you running as \
             cluster-admin instead of the flint-pnfs-mds ServiceAccount?)"
        ),
        Err(e) => {
            assert!(
                e.is_unreachable(),
                "a 403 came back as a record-level REFUSAL ({e}) — an RBAC \
                 mistake must never read as a fact about the volume"
            );
            let msg = e.to_string().to_lowercase();
            assert!(
                msg.contains("forbidden") || msg.contains("403"),
                "expected the server's forbidden, got: {e}"
            );
        }
    }
}

// ── leg 3: the label we write is the label we select by ──────────────

/// The live half of the naming fix. `put` labels an object by kind and
/// `list` fetches it with a label selector; if those two rules disagree,
/// a target disappears from the registry and nothing can dial it.
///
/// `mds-c-2` is the id that used to vanish — the store decided the label
/// by looking for `-c-` anywhere in the object name.
#[tokio::test]
#[ignore = "needs a Kubernetes API server (make test-kind-witness)"]
async fn a_target_named_like_a_composition_is_listed_as_a_target() {
    let t = tag();
    let ids: Vec<String> = ["mds-c-2", "gke-c-1-pool", "plain"]
        .iter()
        .map(|n| format!("{n}-{t}"))
        .collect();
    let w = witness().await;
    for (i, id) in ids.iter().enumerate() {
        w.target_register(id, "10.0.0.9", 4420 + i as u16, now()).await.unwrap();
    }

    let listed: Vec<String> =
        w.target_list().await.unwrap().into_iter().map(|r| r.target_id).collect();
    for id in &ids {
        assert!(
            listed.contains(id),
            "target '{id}' is registered but absent from target_list — the \
             label selector and the label disagree, so this target cannot be \
             probed, placed against, or promoted to"
        );
    }

    sweep(&[], &ids).await;
}

// ── leg 4: our derived names are legal names ─────────────────────────

/// Volume ids come from CSI and are not ours to choose. `object_name`
/// sanitizes them; only the API server can say whether the result is a
/// name it will accept.
#[tokio::test]
#[ignore = "needs a Kubernetes API server (make test-kind-witness)"]
async fn the_api_server_accepts_every_name_we_derive() {
    let t = tag();
    let vols: Vec<String> = vec![
        format!("pvc-Weird_Name/1-{t}"),
        format!("PVC-{t}-UPPER"),
        format!("--leading-and-trailing---{t}--"),
        format!("{}-{t}", "x".repeat(200)),
        format!("::::{t}"),
    ];
    let s = store().await;
    for v in &vols {
        let name = object_name(KIND_COMPOSITION, v);
        let _ = s.delete(&name).await;
        assert_eq!(
            s.put(&name, r#"{"volume":"x"}"#, None).await.unwrap(),
            PutOutcome::Wrote,
            "the API server refused the name we derived for volume '{v}': {name}"
        );
    }
    sweep(&vols, &[]).await;
}

// ── leg 5: two clients, one seat ─────────────────────────────────────

/// Two witnesses on two connections race one promotion. Exactly one seat
/// may move, and the loser must come back with the record's own refusal
/// rather than a second composition.
///
/// This is the unit test's interposed race run for real: no seam, no
/// injected writer, two kube clients and whatever ordering the server
/// picks.
#[tokio::test]
#[ignore = "needs a Kubernetes API server (make test-kind-witness)"]
async fn two_clients_racing_a_promotion_produce_one_composition() {
    let t = tag();
    let vol = format!("race-{t}");
    let (a, b, c) = (format!("tgt-a-{t}"), format!("tgt-b-{t}"), format!("tgt-c-{t}"));

    let w1 = witness().await;
    let w2 = witness().await;
    for id in [&a, &b, &c] {
        w1.target_register(id, "10.0.0.9", 4420, now()).await.unwrap();
    }
    let seat = w1.seat_volume(&vol, &a, now(), now() + 30).await.unwrap();
    assert_eq!(seat.epoch, 1);
    assert_eq!(seat.composer, a);

    // Both candidates are eligible: the election gate must be decided by
    // the CAS, not by one of them failing ElectInSync.
    w1.leg_mark(&vol, &b, LEG_INSYNC, now()).await.unwrap();
    w1.leg_mark(&vol, &c, LEG_INSYNC, now()).await.unwrap();

    // The second witness reads the seat the first one wrote — the whole
    // point of a witness, asserted before the race that depends on it.
    let seen = w2.volume_seat(&vol).await.unwrap().expect("w2 cannot see the seat w1 wrote");
    assert_eq!(seen.composer, a, "the two clients disagree about who composes");

    let (r1, r2) = tokio::join!(
        w1.promote(&vol, 1, &a, &b, now()),
        w2.promote(&vol, 1, &a, &c, now()),
    );
    let winners = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        winners, 1,
        "expected exactly one promotion to land, got {winners} (r1={r1:?} r2={r2:?}) \
         — two winners is a split composition"
    );
    let loser = if r1.is_err() { &r1 } else { &r2 };
    let err = loser.as_ref().unwrap_err();
    assert!(
        matches!(err.refusal(), Some(ExtentAllocError::PromotionRaced { .. })),
        "the losing promotion did not refuse in the record's own words: {err}"
    );

    let final_seat = w2.volume_seat(&vol).await.unwrap().unwrap();
    assert_eq!(final_seat.epoch, 2, "the epoch advanced by more than one promotion");
    assert!(
        final_seat.composer == b || final_seat.composer == c,
        "the seat moved to nobody who asked for it: {}",
        final_seat.composer
    );

    sweep(&[vol], &[a, b, c]).await;
}

// ── leg 6: what a steady state costs, measured ───────────────────────

/// Reconciliation is level-triggered: every shard re-asserts the same
/// facts every pass. Against sqlite a redundant re-assert was a local
/// UPDATE nobody noticed; against the witness it is a write to the one
/// object failover is decided in, and a fleet at rest would contend on
/// it forever — with the write that MATTERS the one that keeps losing.
///
/// So this leg asks the server for the whole bill, both halves:
///
///   * the two facts a pass really does re-assert every time — a
///     target's coordinates and a client's admission — must write
///     NOTHING when they have not changed;
///   * the lease renewal must write, because it is the dead-man and its
///     expiry is the fact that moves. That is the standing API cost of a
///     replicated volume: one write per renewal per volume, and it is
///     recorded here rather than assumed away.
///
/// What is NOT asserted, and was in the first draft of this file: that
/// `leg_mark` is a no-op when re-marked. It is not, and it should not
/// be — `marked_unix` is when a transition happened, which is data. All
/// five production callers are transition-guarded (degrade needs an
/// unreachability verdict AND an in-sync leg, the rebuild needs a
/// completed copy, eviction needs a deposal), so no pass ever re-marks
/// an unchanged leg. Testing it as a no-op tested a call production
/// does not make.
#[tokio::test]
#[ignore = "needs a Kubernetes API server (make test-kind-witness)"]
async fn a_pass_that_changes_nothing_writes_nothing_and_the_lease_is_the_bill() {
    let t = tag();
    let vol = format!("noop-{t}");
    let tgt = format!("tgt-{t}");
    let nqn = "nqn.2014-08.org.nvmexpress:uuid:node-a";
    let w = witness().await;
    w.target_register(&tgt, "10.0.0.9", 4420, now()).await.unwrap();
    w.seat_volume(&vol, &tgt, now(), now() + 30).await.unwrap();
    w.host_admit(&vol, 7, nqn, now()).await.unwrap();

    let s = store().await;
    let comp = object_name(KIND_COMPOSITION, &vol);
    let target = object_name(KIND_TARGET, &tgt);
    let (_, comp_rv) = s.get(&comp).await.unwrap().unwrap();
    let (_, target_rv) = s.get(&target).await.unwrap().unwrap();

    // Three reconcile passes' worth of re-assertion, with the clock
    // moved on each time — a timestamp must not be enough to make a
    // rewrite, or "unchanged" would never happen in production.
    for i in 1..=3 {
        w.target_register(&tgt, "10.0.0.9", 4420, now() + i * 10).await.unwrap();
        w.host_admit(&vol, 7, nqn, now() + i * 10).await.unwrap();
    }

    let (_, comp_after) = s.get(&comp).await.unwrap().unwrap();
    let (_, target_after) = s.get(&target).await.unwrap().unwrap();
    assert_eq!(
        comp_rv, comp_after,
        "three no-op passes moved the composition's resourceVersion — a fleet \
         at rest would contend forever on the object failover is decided in"
    );
    assert_eq!(
        target_rv, target_after,
        "re-registering unchanged coordinates rewrote the target row — every \
         target would write every pass, for nothing"
    );

    // And the half that is not free. The lease is the dead-man: its
    // expiry moves, so the object moves with it.
    w.lease_renew(&vol, &tgt, now() + 120).await.unwrap();
    let (_, comp_renewed) = s.get(&comp).await.unwrap().unwrap();
    assert_ne!(
        comp_rv, comp_renewed,
        "a lease renewal did NOT write — then the expiry the dead-man reads is \
         not the one that was renewed"
    );

    sweep(&[vol], &[tgt]).await;
}

// ── leg 7: unreadable is not absent ──────────────────────────────────

/// A record we cannot parse must refuse. Reading it as empty would seat
/// a live volume a second time and mint a parallel composition — the
/// worst outcome this file can produce, so it is asserted against the
/// real read path and not only against the fake.
#[tokio::test]
#[ignore = "needs a Kubernetes API server (make test-kind-witness)"]
async fn an_unreadable_record_refuses_rather_than_reading_as_absent() {
    let t = tag();
    let vol = format!("corrupt-{t}");
    let s = store().await;
    let name = object_name(KIND_COMPOSITION, &vol);
    let _ = s.delete(&name).await;
    s.put(&name, "{ this is not json", None).await.unwrap();

    let w = witness().await;
    let err = w
        .volume_seat(&vol)
        .await
        .expect_err("an unparseable record read as a volume with no seat");
    assert!(
        matches!(err.refusal(), Some(ExtentAllocError::Corruption(_))),
        "expected a corruption refusal, got: {err}"
    );

    sweep(&[vol], &[]).await;
}

// ── leg 8: the sweep ─────────────────────────────────────────────────

/// DeleteVolume's act, and its second call. One ConfigMap per volume
/// means a sweep that does not work is a leak that outlives the cluster's
/// volumes.
#[tokio::test]
#[ignore = "needs a Kubernetes API server (make test-kind-witness)"]
async fn the_sweep_removes_the_record_and_is_idempotent() {
    let t = tag();
    let vol = format!("sweep-{t}");
    let tgt = format!("tgt-{t}");
    let w = witness().await;
    w.target_register(&tgt, "10.0.0.9", 4420, now()).await.unwrap();
    w.seat_volume(&vol, &tgt, now(), now() + 30).await.unwrap();

    let s = store().await;
    let name = object_name(KIND_COMPOSITION, &vol);
    assert!(s.get(&name).await.unwrap().is_some(), "the seat wrote no object");

    w.drop_volume(&vol).await.unwrap();
    assert!(
        s.get(&name).await.unwrap().is_none(),
        "the record survived its own sweep"
    );
    // A second sweep is the ordinary case: CSI retries DeleteVolume.
    w.drop_volume(&vol).await.expect("sweeping an already-swept record failed");

    sweep(&[], &[tgt]).await;
}
