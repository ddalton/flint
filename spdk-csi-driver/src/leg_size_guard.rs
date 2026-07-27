// leg_size_guard.rs — F43 doc item #8: the leg-size precondition on raid
// admission (the silent-shrink guard).
//
// SPDK raid1 assembles a fresh create at min(leg size) with NO error path
// (raid1_start assigns blockcnt directly, pre-registration, so the
// bdev-layer -EBUSY shrink guard is structurally unreachable — verified at
// v26.05.1-pre source). Once volume expansion can make legs diverge, a
// stale old-size leg at a NodeStage reassembly silently shrinks the device
// underneath a filesystem already grown by NodeExpandVolume: I/O errors on
// the tail, remount-ro, potential corruption. Nothing reaches that state
// today (legs are created equal and catch-up sizes rebuilt heads from the
// source head exactly) — this guard is the PREREQUISITE that makes it
// structurally impossible for the expansion work to ship the hazard.
//
// The rule exploits the asymmetry of the hazard: a leg LARGER than the
// filesystem is waste (the raid caps at min, the tail is unread); a leg
// SMALLER is corruption. So:
//
//   - keep exactly the LARGEST-size cohort among the sized legs (today:
//     all of them — legs are equal); every shorter leg is excluded,
//     loudly, and heals through the ordinary unavailable-replica path
//     (catch-up / re-placement);
//   - the volume's recorded capacity (pv.spec.capacity) is a FLOOR on the
//     kept size: if even the largest cohort is below it, every leg missed
//     a grow the filesystem may already believe in — never serve, fail
//     the stage (the all-legs-equally-short case member comparison cannot
//     see, including the single-survivor direct serve);
//   - unsized legs (probe row without num_blocks/block_size) are kept:
//     unknown is not evidence of divergence.
//
// Comparison is in BYTES (num_blocks × block_size): lvstore creation lets
// SPDK auto-detect block size from the backing device, so heterogeneous
// fleets can carry 512-vs-4096 legs whose num_blocks are not comparable.
// The floor uses `>=` — resize_lvol rounds up to MiB and lvols land on
// cluster granularity, so byte-exact equality against the PV quantity
// would false-mismatch (doc item #8, "measured num_blocks, not requested
// bytes", upgraded to bytes per the block-size caveat above).
//
// Layering: this module is the pure core. The driver's assembly belt runs
// it over the finalized base list (dropping short legs BEFORE the create,
// so staging degrades instead of bricking); admit_one_standby runs the
// same comparison before record_in_sync (a refusal there is a clean
// StandbyAdmissionDeferred); and the node agent's construction boundary
// (guarded_destroy::construction_boundary_verdict) refuses any
// still-unequal create/add outright as the last-resort backstop.
//
// Kill switch: FLINT_LEG_SIZE_GUARD=disabled (sits on the staging path —
// the FLINT_VOLUME_LOCK pattern).

use serde_json::Value;

pub fn enabled() -> bool {
    !std::env::var("FLINT_LEG_SIZE_GUARD").is_ok_and(|v| v.eq_ignore_ascii_case("disabled"))
}

/// Byte size of a `bdev_get_bdevs` row; None when the row carries no
/// usable size (treated as unknown, never as a mismatch).
pub fn bytes_of(bdev: &Value) -> Option<u64> {
    bdev.get("num_blocks")
        .and_then(|v| v.as_u64())
        .zip(bdev.get("block_size").and_then(|v| v.as_u64()))
        .map(|(nb, bs)| nb.saturating_mul(bs))
}

/// The belt's verdict over one finalized base list.
#[derive(Debug, Clone, PartialEq)]
pub struct LegSizePartition {
    /// Indexes (into the input) to keep, in input order.
    pub keep: Vec<usize>,
    /// Indexes to exclude, each with an operator-facing reason.
    pub exclude: Vec<(usize, String)>,
    /// Set when NO leg may serve — the stage must fail rather than
    /// construct anything (all sized legs below the capacity floor).
    pub fail_stage: Option<String>,
    /// The kept cohort's byte size, when the cohort is sized.
    pub serving_bytes: Option<u64>,
}

/// Partition `legs` (label, measured bytes) against the capacity `floor`.
/// See the module header for the rule and its rationale.
pub fn partition_legs(legs: &[(String, Option<u64>)], floor: Option<u64>) -> LegSizePartition {
    let max_bytes = legs.iter().filter_map(|(_, b)| *b).max();

    if let (Some(max), Some(floor_b)) = (max_bytes, floor) {
        if max < floor_b {
            let detail: Vec<String> = legs
                .iter()
                .filter_map(|(n, b)| b.map(|b| format!("{}={}B", n, b)))
                .collect();
            return LegSizePartition {
                keep: Vec::new(),
                exclude: Vec::new(),
                fail_stage: Some(format!(
                    "every leg is below the volume's recorded capacity ({}B): {} — the \
                     filesystem may already be grown past every leg; serving any of them \
                     risks silent corruption (F43 doc item #8)",
                    floor_b,
                    detail.join(", ")
                )),
                serving_bytes: Some(max),
            };
        }
    }

    let mut keep = Vec::new();
    let mut exclude = Vec::new();
    for (i, (label, bytes)) in legs.iter().enumerate() {
        match (*bytes, max_bytes) {
            (Some(b), Some(max)) if b < max => {
                exclude.push((
                    i,
                    format!(
                        "leg {} is {}B but the serving cohort is {}B — a short leg admitted \
                         to a raid1 create silently shrinks the device under its filesystem \
                         (F43 doc item #8); excluded, heals via catch-up / re-placement",
                        label, b, max
                    ),
                ));
            }
            _ => keep.push(i),
        }
    }
    LegSizePartition { keep, exclude, fail_stage: None, serving_bytes: max_bytes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn legs(v: &[(&str, Option<u64>)]) -> Vec<(String, Option<u64>)> {
        v.iter().map(|(n, b)| (n.to_string(), *b)).collect()
    }

    #[test]
    fn bytes_multiplies_and_tolerates_missing_fields() {
        assert_eq!(
            bytes_of(&json!({ "num_blocks": 262144, "block_size": 4096 })),
            Some(1 << 30)
        );
        assert_eq!(bytes_of(&json!({ "num_blocks": 262144 })), None);
        assert_eq!(bytes_of(&json!({})), None);
    }

    #[test]
    fn equal_legs_all_kept_today_invariant() {
        let p = partition_legs(&legs(&[("a", Some(100)), ("b", Some(100))]), Some(90));
        assert_eq!(p.keep, vec![0, 1]);
        assert!(p.exclude.is_empty());
        assert!(p.fail_stage.is_none());
        assert_eq!(p.serving_bytes, Some(100));
    }

    #[test]
    fn short_leg_excluded_grown_kept_the_c2b_shape() {
        // Post-expand: one stale old-size leg among grown legs. Keeping it
        // would assemble at min() — the silent shrink.
        let p = partition_legs(
            &legs(&[("grown1", Some(200)), ("stale", Some(100)), ("grown2", Some(200))]),
            Some(200),
        );
        assert_eq!(p.keep, vec![0, 2]);
        assert_eq!(p.exclude.len(), 1);
        assert_eq!(p.exclude[0].0, 1);
        assert!(p.exclude[0].1.contains("short leg"));
        assert!(p.fail_stage.is_none());
    }

    #[test]
    fn lone_grown_survivor_beats_a_stale_majority() {
        // (stale x2, grown x1): majority voting would pick the shrink —
        // the size asymmetry (larger is waste, smaller is corruption) is
        // what decides, not the head count.
        let p = partition_legs(
            &legs(&[("stale1", Some(100)), ("stale2", Some(100)), ("grown", Some(200))]),
            None,
        );
        assert_eq!(p.keep, vec![2]);
        assert_eq!(p.exclude.len(), 2);
    }

    #[test]
    fn all_legs_below_floor_fails_the_stage() {
        // Every leg missed the grow the filesystem may believe in —
        // including the single-survivor direct-serve case (one leg).
        let p = partition_legs(&legs(&[("a", Some(100)), ("b", Some(100))]), Some(200));
        assert!(p.keep.is_empty());
        assert!(p.fail_stage.as_deref().unwrap_or("").contains("recorded capacity"));
        let single = partition_legs(&legs(&[("survivor", Some(100))]), Some(200));
        assert!(single.fail_stage.is_some(), "direct-serve short survivor must fail");
    }

    #[test]
    fn floor_is_a_floor_not_an_equality() {
        // MiB round-up / cluster granularity: legs legitimately exceed the
        // PV quantity — >= must pass. 1025 MiB legs over a 1 GiB floor.
        let p = partition_legs(&legs(&[("a", Some(1025 << 20)), ("b", Some(1025 << 20))]),
            Some(1 << 30));
        assert!(p.fail_stage.is_none());
        assert_eq!(p.keep, vec![0, 1]);
    }

    #[test]
    fn unsized_legs_are_kept_unknown_is_not_divergence() {
        let p = partition_legs(&legs(&[("a", Some(100)), ("b", None)]), None);
        assert_eq!(p.keep, vec![0, 1]);
        // And a fully-unsized list never fails the stage on the floor.
        let p = partition_legs(&legs(&[("a", None)]), Some(200));
        assert!(p.fail_stage.is_none());
        assert_eq!(p.keep, vec![0]);
    }
}
