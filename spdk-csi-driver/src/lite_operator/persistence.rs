//! Sizing a share's disk from what the bucket actually holds.
//!
//! # Why the target is not in `spec`
//!
//! The operator does not write `spec` — the user owns it, and breaking
//! that turns a stored manifest into a weapon: `kubectl apply` of the
//! original YAML would read as "shrink me", and with
//! `reprovisionOnShrink` on that is a disk rebuild every time a GitOps
//! controller re-syncs. So the computed size lives in an
//! operator-owned annotation and `spec.persistence.size` stays exactly
//! what the user typed.
//!
//! # Why the annotation records a BASIS
//!
//! With only a target recorded, "the operator grew past spec" and "the
//! user wants something smaller" are the same state: `spec.size <
//! target`, both times. Storing the `size` the target was derived from
//! disambiguates them. Any edit to `size` invalidates the target, and
//! the user's number takes over — which makes the rule easy to state:
//!
//! > **Editing `persistence.size` always wins and resets the target.**
//!
//! That is also what keeps auto-expand and reprovision-on-shrink from
//! fighting: a user lowering `size` to shrink is not silently pinned
//! open by a target the operator computed a week ago.

use crate::lite_operator::crd::FlintShare;
use crate::lite_operator::reconcile::quantity_bytes;
use kube::ResourceExt;

/// Operator-written: `"<basis>|<target>"`. `basis` is the
/// `persistence.size` the target was computed from.
pub const ANN_SIZE_TARGET: &str = "flint.io/persistence-target";

/// Default headroom over the project's logical size.
pub const DEFAULT_BUFFER_PCT: u32 = 100;

/// Don't patch the claim for a rounding error. An expansion is an API
/// write and, on some drivers, a rate-limited volume modification — so
/// a target only counts once it is meaningfully bigger than what is
/// already provisioned.
pub const MIN_GROWTH_PCT: u128 = 10;

/// The recorded `(basis, target)`, if any and if parseable.
pub fn recorded_target(share: &FlintShare) -> Option<(String, String)> {
    let raw = share.annotations().get(ANN_SIZE_TARGET)?;
    let (basis, target) = raw.split_once('|')?;
    if basis.is_empty() || target.is_empty() {
        return None;
    }
    Some((basis.to_string(), target.to_string()))
}

pub fn format_target(basis: &str, target: &str) -> String {
    format!("{basis}|{target}")
}

/// What size the claim should actually be.
///
/// `spec.persistence.size` unless the operator has recorded a bigger
/// target FOR THAT EXACT SIZE. A stale basis means the user edited
/// `size`, and then their number is the answer.
pub fn effective_size(share: &FlintShare) -> String {
    let spec_size = share.spec.persistence.size.clone();
    let Some((basis, target)) = recorded_target(share) else {
        return spec_size;
    };
    if basis != spec_size {
        return spec_size; // the user edited size — target discarded
    }
    match (quantity_bytes(&target), quantity_bytes(&spec_size)) {
        (Some(t), Some(s)) if t > s => target,
        _ => spec_size,
    }
}

/// Inputs the hub publishes once it has read or built a manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inventory {
    pub logical_bytes: u64,
    pub largest_object_bytes: u64,
}

/// The size auto-expand wants, in bytes, before any ceiling.
///
/// Two independent lower bounds, and they are not the same question:
///
/// - **the project plus headroom** — a cache-hit-rate choice, and what
///   `bufferPercent` tunes;
/// - **the largest single object** — a CORRECTNESS floor. Below it that
///   object can never be hydrated at all, and every read of it answers
///   NOSPC however much eviction runs. Headroom does not apply: the
///   floor is about one file fitting, not about how many do.
pub fn wanted_bytes(inv: Inventory, buffer_pct: u32) -> u128 {
    let with_buffer =
        (inv.logical_bytes as u128).saturating_mul(100 + buffer_pct as u128) / 100;
    with_buffer.max(inv.largest_object_bytes as u128)
}

/// What the target should become, or `None` to leave the claim alone.
///
/// `current` is what is provisioned NOW — the live claim, not the spec,
/// because that is what an expansion has to beat to be worth an API
/// write. Returns bytes; the caller renders the quantity.
pub fn expand_to(
    inv: Inventory,
    buffer_pct: u32,
    current_bytes: u128,
    max_bytes: u128,
) -> Option<u128> {
    let want = wanted_bytes(inv, buffer_pct).min(max_bytes);
    // Never shrink, and never churn: growth has to clear the floor by a
    // margin, or a project that ends every write a few bytes larger
    // would patch its claim on every barrier.
    if want <= current_bytes {
        return None;
    }
    let growth = want - current_bytes;
    if growth.saturating_mul(100) < current_bytes.saturating_mul(MIN_GROWTH_PCT) {
        return None;
    }
    Some(want)
}

/// Render bytes as a Kubernetes quantity, rounded UP to a whole Gi.
///
/// Up, always: rounding a size down could land under the largest
/// object and re-create the very condition the expansion exists to
/// clear. Whole Gi because storage classes provision in them anyway,
/// and a claim reading `17179869184` helps nobody.
pub fn as_gi(bytes: u128) -> String {
    const GI: u128 = 1024 * 1024 * 1024;
    let gi = bytes.div_ceil(GI).max(1);
    format!("{gi}Gi")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GI: u128 = 1024 * 1024 * 1024;

    fn inv(logical: u64, largest: u64) -> Inventory {
        Inventory { logical_bytes: logical, largest_object_bytes: largest }
    }

    /// The floor is a correctness bound and the buffer is a preference,
    /// so the buffer must never be able to talk the floor down. A
    /// project of one big file and little else is exactly where a
    /// naive `logical * 2` would under-provision.
    #[test]
    fn the_largest_object_floor_survives_a_zero_buffer() {
        // 10 GiB project, one 9 GiB file, no headroom asked for.
        let i = inv(10 * GI as u64, 9 * GI as u64);
        assert_eq!(wanted_bytes(i, 0), 10 * GI, "the project itself still fits");

        // A project SMALLER than its largest object cannot happen, but a
        // buffer of 0 on a single-file project can: the floor is what
        // keeps it hydratable.
        let single = inv(9 * GI as u64, 9 * GI as u64);
        assert_eq!(wanted_bytes(single, 0), 9 * GI);
        assert_eq!(wanted_bytes(single, 100), 18 * GI, "buffer applies above the floor");
    }

    #[test]
    fn the_buffer_is_a_percentage_of_the_project() {
        let i = inv(10 * GI as u64, 1 * GI as u64);
        assert_eq!(wanted_bytes(i, 0), 10 * GI);
        assert_eq!(wanted_bytes(i, 50), 15 * GI);
        assert_eq!(wanted_bytes(i, 100), 20 * GI);
    }

    /// Growth is one-way, so every guard that stops it matters more
    /// than one that starts it.
    #[test]
    fn expansion_stops_at_the_ceiling_and_never_goes_backwards() {
        let i = inv(100 * GI as u64, 1 * GI as u64); // wants 200Gi at 100%
        assert_eq!(
            expand_to(i, 100, 10 * GI, 50 * GI),
            Some(50 * GI),
            "the ceiling clamps, it does not veto"
        );
        assert_eq!(
            expand_to(i, 100, 50 * GI, 50 * GI),
            None,
            "already at the ceiling: nothing to do"
        );
        // A claim already larger than the project must never shrink.
        let small = inv(1 * GI as u64, 1 * GI as u64);
        assert_eq!(expand_to(small, 100, 500 * GI, 1000 * GI), None);
    }

    /// A project that grows by a few bytes per barrier must not patch
    /// its claim on every barrier.
    #[test]
    fn a_trivial_increase_is_not_worth_an_api_write() {
        // Wants 100Gi + 1%, against a 100Gi claim: under the margin.
        let i = inv((101 * GI / 2) as u64, 1);
        assert_eq!(expand_to(i, 100, 100 * GI, 1000 * GI), None);
        // Clearly bigger: taken.
        let big = inv((80 * GI) as u64, 1);
        assert_eq!(expand_to(big, 100, 100 * GI, 1000 * GI), Some(160 * GI));
    }

    #[test]
    fn quantities_round_up_never_down() {
        assert_eq!(as_gi(GI), "1Gi");
        assert_eq!(as_gi(GI + 1), "2Gi", "a size must never round BELOW what was asked");
        assert_eq!(as_gi(0), "1Gi");
        assert_eq!(as_gi(20 * GI), "20Gi");
    }
}
