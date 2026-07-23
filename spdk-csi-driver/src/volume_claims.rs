// volume_claims.rs — the per-volume single-operation claim shared by the
// catch-up, cutover, and hot-rejoin planners (Tier-2 design item 4).
//
// The rev-5 record contract makes concurrent operations on one volume SAFE,
// but safe-and-wasteful still burns real things: a cutover bounce restaging
// mid-window costs a quiesce and an unwind; two orchestrators shallow-copying
// against the same source fight for its bandwidth. This registry generalizes
// the catch-up orchestrator's old in-flight set: at most one long-running
// operation per volume across ALL planners, whoever claims first.
//
// Process-global on purpose — the mutual exclusion is inherently scoped to
// the single controller instance (the same assumption CreateVolume placement
// and the epoch scheduler already make). Node-agent flows never see it;
// their safety comes from the record, not from this registry.
//
// The epoch scheduler does NOT claim: its cuts are the designed input of the
// chase and must keep flowing during multi-hour catch-ups. It only *consults*
// the registry (`holder`) to defer a volume's cut while a hot rejoin holds
// the claim — a scheduler cut landing inside the quiesce window would abort
// it (the window's E_f cut is strict-fresh; EEXIST unwinds).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

pub const OP_CATCHUP: &str = "catch-up";
pub const OP_CUTOVER: &str = "cutover";
pub const OP_HOT_REJOIN: &str = "hot-rejoin";

#[derive(Default)]
pub struct VolumeClaims {
    inner: Mutex<HashMap<String, (&'static str, std::time::Instant)>>,
}

/// RAII claim on one volume; releases on drop (including task panic/abort
/// unwind — a crashed operation must never wedge its volume).
pub struct VolumeClaim<'a> {
    claims: &'a VolumeClaims,
    volume_id: String,
}

impl VolumeClaims {
    pub fn new() -> Self {
        VolumeClaims::default()
    }

    /// Claim `volume_id` for `op`. None = someone else holds it (skip the
    /// volume this tick). "Claims are short-lived relative to tick cadence"
    /// is an ASSUMPTION, not an invariant — which is why holders carry an
    /// acquisition timestamp: F39's wedge held one invisibly for the whole
    /// incident while every skip site stayed silent. Skip sites must log
    /// via [`Self::holder`]'s age.
    pub fn try_claim<'a>(&'a self, volume_id: &str, op: &'static str) -> Option<VolumeClaim<'a>> {
        let mut inner = self.inner.lock().expect("volume-claims lock poisoned");
        if inner.contains_key(volume_id) {
            return None;
        }
        inner.insert(volume_id.to_string(), (op, std::time::Instant::now()));
        Some(VolumeClaim { claims: self, volume_id: volume_id.to_string() })
    }

    /// Which operation currently holds `volume_id` and for how long.
    pub fn holder(&self, volume_id: &str) -> Option<(&'static str, std::time::Duration)> {
        self.inner
            .lock()
            .expect("volume-claims lock poisoned")
            .get(volume_id)
            .map(|(op, since)| (*op, since.elapsed()))
    }
}

/// One skip-site log line (F39: starvation must be visible). Info below the
/// starvation threshold, warn above it — a wedged holder surfaces in logs
/// long before a human goes looking.
pub fn log_claim_skip(volume_id: &str, wanted_op: &str, claims: &VolumeClaims) {
    let threshold = std::env::var("FLINT_CLAIM_STARVATION_WARN_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(900);
    match claims.holder(volume_id) {
        Some((held_by, age)) if age.as_secs() >= threshold => {
            tracing::warn!(
                volume_id,
                wanted_op,
                held_by,
                held_secs = age.as_secs(),
                "[CLAIMS] volume claim held past the starvation threshold — the holder may be wedged (F39 shape)"
            );
        }
        Some((held_by, age)) => {
            tracing::info!(
                volume_id,
                wanted_op,
                held_by,
                held_secs = age.as_secs(),
                "[CLAIMS] volume claimed by another operation — skipping this tick"
            );
        }
        None => {
            // Raced a release between try_claim and this log — benign.
            tracing::info!(volume_id, wanted_op, "[CLAIMS] claim skipped (released mid-race)");
        }
    }
}

impl Drop for VolumeClaim<'_> {
    fn drop(&mut self) {
        self.claims
            .inner
            .lock()
            .expect("volume-claims lock poisoned")
            .remove(&self.volume_id);
    }
}

/// The controller-wide registry all planner loops share.
pub fn global() -> &'static VolumeClaims {
    static GLOBAL: OnceLock<VolumeClaims> = OnceLock::new();
    GLOBAL.get_or_init(VolumeClaims::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_is_exclusive_per_volume_and_released_on_drop() {
        let claims = VolumeClaims::new();
        let held = claims.try_claim("vol1", OP_CATCHUP).expect("first claim");
        assert!(claims.try_claim("vol1", OP_HOT_REJOIN).is_none());
        assert!(claims.try_claim("vol1", OP_CATCHUP).is_none(), "not reentrant either");
        assert_eq!(claims.holder("vol1").map(|(op, _)| op), Some(OP_CATCHUP));

        // A different volume is independent.
        let other = claims.try_claim("vol2", OP_CUTOVER).expect("other volume");
        assert_eq!(claims.holder("vol2").map(|(op, _)| op), Some(OP_CUTOVER));
        drop(other);

        drop(held);
        assert!(claims.holder("vol1").is_none());
        assert!(claims.try_claim("vol1", OP_HOT_REJOIN).is_some());
    }

    #[test]
    fn holder_reports_none_for_unclaimed() {
        let claims = VolumeClaims::new();
        assert!(claims.holder("vol1").is_none());
    }
}
