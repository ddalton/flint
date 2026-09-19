//! The upload bytes-in-flight bound, seen from the barrier.
//!
//! `flint_store::gate::ByteGate` is the store's; the barrier meets it at
//! two points — every local part of a multipart compose, charged inside
//! the store, and every whole body `upload_one` reads before
//! `put_whole`, charged through `ObjectStore::upload_gate`. These legs
//! drive a tree wide enough to blow far past a small budget and read the
//! bound off the gate's high-water mark. Each has its CONTROL: the same
//! tree through a gate too large to bind must report a far higher mark,
//! or the assertion under the budget proves nothing (a control moves one
//! dimension — the budget — and nothing else).
//!
//! The double mirrors the S3 window shape (`part_parallelism` parts of
//! one object staged concurrently, `upload_fanout` objects at once) and
//! has an injectable PUT latency so whole bodies genuinely overlap; on
//! the S3 backend the same charge sits in `compose_one_part`, and its
//! mutation check is the loopback measurement, not this file.

use std::sync::Arc;

use flint_store::memory::MemoryStore;
use flint_store::ObjectStore;

use super::state::SyncerState;
use super::{LeanConfig, Syncer};

const PREFIX: &str = "tenant/gate";
const MIB: u64 = 1 << 20;

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

fn write_bytes(root: &std::path::Path, rel: &str, len: u64, fill: u8) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    // Not one repeated byte: a part staged at the wrong offset must
    // change the checksum the store validates at Complete.
    let body: Vec<u8> = (0..len).map(|i| fill ^ (i as u8).wrapping_mul(31)).collect();
    std::fs::write(p, body).unwrap();
}

/// The shape of one leg: a tree of `files` x `each` bytes published in
/// one barrier through a store built with `gate_bytes` of budget.
struct Leg {
    gate_bytes: u64,
    part_parallelism: usize,
    files: usize,
    each: u64,
    whole_put_max: u64,
    put_delay_ms: u64,
}

/// Run one leg; the gate's high-water mark and the store, for the
/// caller's own oracles.
async fn publish(leg: &Leg) -> (u64, Arc<MemoryStore>) {
    let store = MemoryStore::new()
        .with_part_parallelism(leg.part_parallelism)
        .with_upload_inflight_max_bytes(leg.gate_bytes);
    store.inject_put_whole_delay_ms(leg.put_delay_ms);
    let store = Arc::new(store);
    let gate = store.upload_gate().expect("the double was built with a gate");
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    sc.cfg.whole_put_max = leg.whole_put_max;
    // No claim here: a barrier claims its own fence inside the commit
    // section (the per-barrier lease), so a fresh syncer publishes as is.
    sc.checkout().await.unwrap();
    for i in 0..leg.files {
        write_bytes(dir.path(), &format!("ckpt/shard-{i:02}.bin"), leg.each, i as u8);
    }
    let r = sc.run_barrier().await.unwrap();
    assert_eq!(r.uploaded.len(), leg.files, "every file published: {:?}", r.deferred);
    assert!(r.deferred.is_empty());
    assert_eq!(gate.held(), 0, "every permit released once the barrier is over");
    (gate.high_water(), store)
}

/// (a) Large objects, composed 8-wide: the parts of twelve 4 MiB objects
/// (four 1 MiB parts each, all forty-eight eligible to be in flight at
/// once under `upload_fanout` 32 x parallelism 8) never hold more than
/// the 3 MiB budget; the same tree through a 1 GiB gate holds many
/// times that, which is what the budget is bounding.
#[tokio::test]
async fn compose_parts_never_exceed_the_budget_and_the_control_blows_past_it() {
    let budget = 3 * MIB;
    let leg = |gate_bytes| Leg {
        gate_bytes,
        part_parallelism: 8,
        files: 12,
        each: 4 * MIB,
        whole_put_max: MIB, // everything composes, in 1 MiB parts
        put_delay_ms: 0,
    };
    let (bounded, _) = publish(&leg(budget)).await;
    assert!(bounded <= budget, "high-water {bounded} exceeded the {budget} budget");
    assert!(bounded > 0, "nothing was charged: the gate is not on this path");

    let (control, _) = publish(&leg(1 << 30)).await;
    assert!(
        control >= 4 * budget,
        "the control's high-water ({control}) is not far above the budget ({budget}): \
         the tree is too narrow for the bounded leg to have proved anything"
    );
}

/// (b) Whole bodies, `upload_fanout` wide: twenty-four 512 KiB files,
/// each read whole by `upload_one` and charged through the store's
/// gate, never hold more than the 2 MiB budget; the control reports
/// the fan-out's worth.
#[tokio::test]
async fn whole_bodies_never_exceed_the_budget_and_the_control_blows_past_it() {
    let budget = 2 * MIB;
    let leg = |gate_bytes| Leg {
        gate_bytes,
        part_parallelism: 8,
        files: 24,
        each: MIB / 2,
        whole_put_max: 64 * MIB, // nothing composes: every file is one PUT
        put_delay_ms: 5,         // so the PUTs overlap and hold their bodies
    };
    let (bounded, _) = publish(&leg(budget)).await;
    assert!(bounded <= budget, "high-water {bounded} exceeded the {budget} budget");
    assert!(bounded > 0, "nothing was charged: the gate is not on this path");

    let (control, _) = publish(&leg(1 << 30)).await;
    assert!(
        control >= 4 * budget,
        "the control's high-water ({control}) is not far above the budget ({budget}): \
         the whole-body PUTs did not overlap, so the bounded leg proved nothing"
    );
}

/// (c) The clamp: a body, and a part, larger than the whole budget still
/// publish — alone — instead of waiting for permits that cannot exist.
/// The high-water mark carries the true size, so it shows the clamped
/// holder above the budget and shows it held ONE at a time.
#[tokio::test]
async fn a_single_object_larger_than_the_budget_still_publishes() {
    let budget = 2 * MIB;
    // A whole body three times the budget.
    let whole = Leg {
        gate_bytes: budget,
        part_parallelism: 8,
        files: 1,
        each: 6 * MIB,
        whole_put_max: 64 * MIB,
        put_delay_ms: 0,
    };
    let (hw, _) = tokio::time::timeout(std::time::Duration::from_secs(30), publish(&whole))
        .await
        .expect("a whole body above the budget deadlocked on the gate");
    assert_eq!(hw, 6 * MIB, "the one clamped body, charged at its true size");

    // Two 4 MiB parts of an 8 MiB object, each twice the budget: both
    // clamp to the whole budget, so they serialize — the mark is one
    // part, never two — and the object still lands byte-identical.
    let parts = Leg {
        gate_bytes: budget,
        part_parallelism: 8,
        files: 1,
        each: 8 * MIB,
        whole_put_max: 4 * MIB,
        put_delay_ms: 0,
    };
    let (hw, store) = tokio::time::timeout(std::time::Duration::from_secs(30), publish(&parts))
        .await
        .expect("a part above the budget deadlocked on the gate");
    assert_eq!(hw, 4 * MIB, "clamped parts run one at a time");
    let dir = tempfile::tempdir().unwrap();
    let mut reader = syncer(&store, dir.path()).await;
    reader.cfg.whole_put_max = 4 * MIB;
    reader.checkout().await.unwrap();
    let got = std::fs::read(dir.path().join("ckpt/shard-00.bin")).unwrap();
    let want: Vec<u8> = (0..8 * MIB).map(|i| 0u8 ^ (i as u8).wrapping_mul(31)).collect();
    assert!(got == want, "the clamped compose did not round-trip byte-identically");
}

/// A store built without a gate offers none, and the barrier runs
/// exactly as before: the bound is opt-out by construction of the
/// store, never a hidden default in the syncer.
#[tokio::test]
async fn a_store_without_a_gate_offers_none() {
    let store = MemoryStore::new().with_upload_inflight_max_bytes(0);
    assert!(store.upload_gate().is_none());
    let store = Arc::new(store);
    let dir = tempfile::tempdir().unwrap();
    let mut sc = syncer(&store, dir.path()).await;
    sc.cfg.whole_put_max = MIB;
    sc.checkout().await.unwrap();
    write_bytes(dir.path(), "ckpt/shard-00.bin", 3 * MIB, 7);
    let r = sc.run_barrier().await.unwrap();
    assert_eq!(r.uploaded.len(), 1);
}
