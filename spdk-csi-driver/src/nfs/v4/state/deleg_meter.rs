//! Delegation metering (design §10).
//!
//! The grant/refusal split is not decoration: it is the evidence that
//! decides whether default-on is arguable at all. "Delegations are on"
//! and "delegations are doing anything" are different claims, and only
//! the counters can tell them apart — a fleet whose workload never
//! re-accesses a file gets every grant recalled and pays the recall
//! cost for no cache win.
//!
//! Shape follows [`crate::pnfs::mds::f68a_meter`]: relaxed atomics at
//! the hot sites (a handful of uncontended `fetch_add`s per RPC,
//! unmeasurable next to a network round-trip) plus a reporter task
//! that turns deltas into ONE line per interval.
//!
//! **The line is INFO on purpose.** Four discriminators in the F68
//! investigation were vacuous because the thing they looked for was
//! emitted at `debug!` while the server ran at INFO, so an absence
//! proved nothing. Every rig leg in §9 asserts on these counters; if
//! they were debug-only, a green leg would mean "the server was quiet"
//! rather than "the server was correct".
//!
//! Counters here are monotonic totals. The reporter prints deltas for
//! readability but a rig should scrape the totals — a delta line that
//! rotates away takes its evidence with it (runby, 2026-09-01).

use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Upper bucket bounds for `recall_latency_seconds`, in milliseconds.
/// The ladder's rungs are at 30s/60s and its deadline at 90s, so the
/// buckets straddle those: a histogram whose bounds miss the ladder's
/// own structure cannot show whether recalls land on the fast path or
/// only ever complete at a rung.
pub const RECALL_LATENCY_BUCKETS_MS: [u64; 9] =
    [100, 500, 1_000, 2_000, 5_000, 10_000, 30_000, 60_000, 90_000];

/// How a recall ended. Mirrors `cb_recall_outcome_total{...}` in §10.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallOutcome {
    Acked,
    Timeout,
    Refused,
    PathDown,
    ClientDisowns,
}

/// Why a delegation was revoked. Mirrors `deleg_revoked_total{...}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeReason {
    /// The 90s deadline from first successful transmit expired.
    Deadline,
    /// The back-channel stayed down through the CB_PATH_DOWN window.
    ChannelDead,
    /// The client answered the recall with a definitive refusal.
    Refused,
    /// The holder's lease had already lapsed at the conflict consult,
    /// so no recall was attempted — it could not have been answered.
    /// Distinct from `ChannelDead`, which is a REACHABILITY failure
    /// against a client that still holds a valid lease: collapsing
    /// them would hide a broken back-channel inside ordinary client
    /// churn.
    LeaseExpired,
}

impl RecallOutcome {
    pub fn label(self) -> &'static str {
        match self {
            RecallOutcome::Acked => "acked",
            RecallOutcome::Timeout => "timeout",
            RecallOutcome::Refused => "refused",
            RecallOutcome::PathDown => "path_down",
            RecallOutcome::ClientDisowns => "client_disowns",
        }
    }
}

impl RevokeReason {
    pub fn label(self) -> &'static str {
        match self {
            RevokeReason::Deadline => "deadline",
            RevokeReason::ChannelDead => "channel_dead",
            RevokeReason::Refused => "refused",
            RevokeReason::LeaseExpired => "lease_expired",
        }
    }
}

#[derive(Debug, Default)]
pub struct DelegMeter {
    pub cb_recall_sent: AtomicU64,
    outcome_acked: AtomicU64,
    outcome_timeout: AtomicU64,
    outcome_refused: AtomicU64,
    outcome_path_down: AtomicU64,
    outcome_disowns: AtomicU64,
    pub delegreturn: AtomicU64,
    revoked_deadline: AtomicU64,
    revoked_channel_dead: AtomicU64,
    revoked_refused: AtomicU64,
    revoked_lease_expired: AtomicU64,
    seq4_path_down: AtomicU64,
    seq4_state_revoked: AtomicU64,
    /// `deleg_rearm_total` — back-channel rebinds that woke at least
    /// one ladder parked in its CB_PATH_DOWN window. Counted at the
    /// FIRE, not at the bind: a bind for a client with nothing parked
    /// is an ordinary mount, and counting it would bury the signal
    /// this exists to show — that a reconnect turned a would-be
    /// revocation back into a recall.
    rearm: AtomicU64,
    recall_latency_sum_ms: AtomicU64,
    recall_latency_count: AtomicU64,
    recall_latency_buckets: [AtomicU64; RECALL_LATENCY_BUCKETS_MS.len()],
    /// `delay_answered_total{site}` — the conflict site that answered
    /// NFS4ERR_DELAY. Keyed by site name because §5.2 sites are added
    /// as the fence funnel grows and a fixed array would silently drop
    /// a new one.
    delays: DashMap<&'static str, u64>,
}

impl DelegMeter {
    pub fn note_recall_sent(&self) {
        self.cb_recall_sent.fetch_add(1, Relaxed);
    }

    pub fn note_outcome(&self, o: RecallOutcome) {
        match o {
            RecallOutcome::Acked => &self.outcome_acked,
            RecallOutcome::Timeout => &self.outcome_timeout,
            RecallOutcome::Refused => &self.outcome_refused,
            RecallOutcome::PathDown => &self.outcome_path_down,
            RecallOutcome::ClientDisowns => &self.outcome_disowns,
        }
        .fetch_add(1, Relaxed);
    }

    pub fn note_delegreturn(&self) {
        self.delegreturn.fetch_add(1, Relaxed);
    }

    pub fn note_revoked(&self, r: RevokeReason) {
        match r {
            RevokeReason::Deadline => &self.revoked_deadline,
            RevokeReason::ChannelDead => &self.revoked_channel_dead,
            RevokeReason::Refused => &self.revoked_refused,
            RevokeReason::LeaseExpired => &self.revoked_lease_expired,
        }
        .fetch_add(1, Relaxed);
    }

    /// `seq4_flag_raised_total{flag}`. Takes the raw SEQ4 bit so the
    /// caller cannot disagree with the wire about which flag it set.
    pub fn note_seq4(&self, flag: u32) {
        use crate::nfs::v4::protocol::seq4_status;
        if flag & seq4_status::CB_PATH_DOWN != 0 {
            self.seq4_path_down.fetch_add(1, Relaxed);
        }
        if flag & seq4_status::RECALLABLE_STATE_REVOKED != 0 {
            self.seq4_state_revoked.fetch_add(1, Relaxed);
        }
    }

    /// A rebind re-drove parked ladders for one client.
    pub fn note_rearm(&self) {
        self.rearm.fetch_add(1, Relaxed);
    }

    pub fn rearm_total(&self) -> u64 {
        self.rearm.load(Relaxed)
    }

    pub fn note_delay(&self, site: &'static str) {
        *self.delays.entry(site).or_insert(0) += 1;
    }

    /// First-transmit → RETURNED/REVOKED, per §10.
    pub fn observe_recall_latency_ms(&self, ms: u64) {
        self.recall_latency_sum_ms.fetch_add(ms, Relaxed);
        self.recall_latency_count.fetch_add(1, Relaxed);
        for (i, bound) in RECALL_LATENCY_BUCKETS_MS.iter().enumerate() {
            if ms <= *bound {
                self.recall_latency_buckets[i].fetch_add(1, Relaxed);
            }
        }
    }

    pub fn delay_count(&self, site: &str) -> u64 {
        self.delays.get(site).map(|v| *v).unwrap_or(0)
    }

    pub fn delays_total(&self) -> u64 {
        self.delays.iter().map(|e| *e.value()).sum()
    }

    pub fn outcome_count(&self, o: RecallOutcome) -> u64 {
        match o {
            RecallOutcome::Acked => &self.outcome_acked,
            RecallOutcome::Timeout => &self.outcome_timeout,
            RecallOutcome::Refused => &self.outcome_refused,
            RecallOutcome::PathDown => &self.outcome_path_down,
            RecallOutcome::ClientDisowns => &self.outcome_disowns,
        }
        .load(Relaxed)
    }

    pub fn revoked_count(&self, r: RevokeReason) -> u64 {
        match r {
            RevokeReason::Deadline => &self.revoked_deadline,
            RevokeReason::ChannelDead => &self.revoked_channel_dead,
            RevokeReason::Refused => &self.revoked_refused,
            RevokeReason::LeaseExpired => &self.revoked_lease_expired,
        }
        .load(Relaxed)
    }

    pub fn revoked_total(&self) -> u64 {
        self.revoked_deadline.load(Relaxed)
            + self.revoked_channel_dead.load(Relaxed)
            + self.revoked_refused.load(Relaxed)
            + self.revoked_lease_expired.load(Relaxed)
    }

    pub fn seq4_count(&self, flag: u32) -> u64 {
        use crate::nfs::v4::protocol::seq4_status;
        if flag & seq4_status::RECALLABLE_STATE_REVOKED != 0 {
            self.seq4_state_revoked.load(Relaxed)
        } else if flag & seq4_status::CB_PATH_DOWN != 0 {
            self.seq4_path_down.load(Relaxed)
        } else {
            0
        }
    }

    /// Bucket counts, cumulative (Prometheus `le` semantics).
    pub fn latency_buckets(&self) -> Vec<(u64, u64)> {
        RECALL_LATENCY_BUCKETS_MS
            .iter()
            .enumerate()
            .map(|(i, b)| (*b, self.recall_latency_buckets[i].load(Relaxed)))
            .collect()
    }

    pub fn latency_count(&self) -> u64 {
        self.recall_latency_count.load(Relaxed)
    }

    pub fn latency_sum_ms(&self) -> u64 {
        self.recall_latency_sum_ms.load(Relaxed)
    }

    /// p99-ish read for the rigs: the smallest bucket bound that
    /// already covers `pct` of observations. Coarse by construction —
    /// a bucketed histogram cannot do better — and the §9 leg asserts
    /// "recall p99 < 5s", which the 5,000ms bound answers exactly.
    /// Returns None when nothing has been observed, so a rig cannot
    /// mistake "no recalls happened" for "every recall was fast" —
    /// the same absence-vs-health confusion the F68 hunt kept hitting.
    pub fn latency_percentile_ms(&self, pct: f64) -> Option<u64> {
        let total = self.latency_count();
        if total == 0 {
            return None;
        }
        let want = (total as f64 * pct).ceil() as u64;
        for (bound, count) in self.latency_buckets() {
            if count >= want {
                return Some(bound);
            }
        }
        Some(u64::MAX)
    }
}

/// One reporter line's worth of totals. Deltas are computed against
/// the previous snapshot by the reporter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DelegMeterTotals {
    pub granted: u64,
    pub refused: u64,
    pub recall_sent: u64,
    pub acked: u64,
    pub timeout: u64,
    pub refused_recalls: u64,
    pub path_down: u64,
    pub disowns: u64,
    pub returned: u64,
    pub revoked: u64,
    pub delays: u64,
    pub rearms: u64,
}

impl DelegMeterTotals {
    /// True when nothing at all moved — the reporter stays SILENT, as
    /// f68a does, so an idle server does not print a line a minute
    /// forever and push real evidence out of a rotating log.
    pub fn is_zero(&self) -> bool {
        *self == DelegMeterTotals::default()
    }

    /// One-line human rendering for the reporter: deltas for the
    /// interval plus the live gauges. Deliberately dense and stable —
    /// rigs grep this, so the field names are part of the contract.
    pub fn render(&self, outstanding: u64, under_recall: u64, p99_ms: Option<u64>) -> String {
        format!(
            "granted +{} refused +{} · recall sent +{} acked +{} timeout +{} refused +{} path_down +{} disown +{} · returned +{} revoked +{} · delay +{} rearm +{} · outstanding {} under_recall {} · recall p99 {}",
            self.granted,
            self.refused,
            self.recall_sent,
            self.acked,
            self.timeout,
            self.refused_recalls,
            self.path_down,
            self.disowns,
            self.returned,
            self.revoked,
            self.delays,
            self.rearms,
            outstanding,
            under_recall,
            // "n/a" rather than 0: no observations is not a fast p99,
            // and a rig grepping this must not read silence as health.
            match p99_ms {
                // The overflow sentinel, rendered as what it means.
                // A bucketed histogram cannot say how far past its top
                // bound an observation was, and printing u64::MAX as a
                // number put "recall p99 18446744073709551615ms" in the
                // log the first time a recall was revoked AT the 90s
                // deadline (the deadline sleep wakes a hair after it,
                // so the sample lands just outside the last bucket).
                // A comparison against it still fails correctly; a
                // human reading it saw garbage.
                Some(u64::MAX) => format!(
                    ">{}ms",
                    RECALL_LATENCY_BUCKETS_MS[RECALL_LATENCY_BUCKETS_MS.len() - 1],
                ),
                Some(ms) => format!("{}ms", ms),
                None => "n/a".to_string(),
            }
        )
    }

    pub fn delta(&self, prev: &DelegMeterTotals) -> DelegMeterTotals {
        DelegMeterTotals {
            granted: self.granted.saturating_sub(prev.granted),
            refused: self.refused.saturating_sub(prev.refused),
            recall_sent: self.recall_sent.saturating_sub(prev.recall_sent),
            acked: self.acked.saturating_sub(prev.acked),
            timeout: self.timeout.saturating_sub(prev.timeout),
            refused_recalls: self.refused_recalls.saturating_sub(prev.refused_recalls),
            path_down: self.path_down.saturating_sub(prev.path_down),
            disowns: self.disowns.saturating_sub(prev.disowns),
            returned: self.returned.saturating_sub(prev.returned),
            revoked: self.revoked.saturating_sub(prev.revoked),
            delays: self.delays.saturating_sub(prev.delays),
            rearms: self.rearms.saturating_sub(prev.rearms),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfs::v4::protocol::seq4_status;

    /// A recall slower than the largest bucket must read as ">90000ms",
    /// not as the sentinel. This is not cosmetic: the FIRST revocation
    /// this server ever performed against a real client landed here —
    /// the 90s deadline sleep wakes a hair after 90s, so the sample
    /// falls just outside the last bucket — and the log said
    /// "recall p99 18446744073709551615ms".
    #[test]
    fn a_recall_past_the_last_bucket_renders_as_greater_than_not_as_u64_max() {
        let m = DelegMeter::default();
        m.observe_recall_latency_ms(90_001);
        let p99 = m.latency_percentile_ms(0.99);
        assert_eq!(p99, Some(u64::MAX), "the sentinel is what a rig compares against");
        let line = DelegMeterTotals::default().render(0, 0, p99);
        assert!(line.ends_with("recall p99 >90000ms"), "got {line}");
        assert!(
            !line.contains("18446744073709551615"),
            "the sentinel must never reach a human as a number: {line}",
        );

        // ...and an in-range observation still prints as itself.
        let m2 = DelegMeter::default();
        m2.observe_recall_latency_ms(1_500);
        let line2 = DelegMeterTotals::default().render(0, 0, m2.latency_percentile_ms(0.99));
        assert!(line2.ends_with("recall p99 2000ms"), "got {line2}");
    }

    #[test]
    fn the_reporter_line_says_n_a_not_zero_when_nothing_was_recalled() {
        // The vacuity lesson in one line of output: a rig grepping
        // "recall p99" must not read a silent server as a fast one.
        // 0ms and n/a are different claims.
        let t = DelegMeterTotals { granted: 3, ..Default::default() };
        let quiet = t.render(3, 0, None);
        assert!(quiet.contains("recall p99 n/a"), "{quiet}");
        assert!(!quiet.contains("p99 0ms"), "{quiet}");
        let busy = t.render(3, 1, Some(1_000));
        assert!(busy.contains("recall p99 1000ms"), "{busy}");
        assert!(busy.contains("outstanding 3 under_recall 1"), "{busy}");
    }

    #[test]
    fn latency_buckets_are_cumulative_and_straddle_the_ladder() {
        let m = DelegMeter::default();
        m.observe_recall_latency_ms(50); // fast path
        m.observe_recall_latency_ms(45_000); // past rung 2, before deadline
        let b = m.latency_buckets();
        // cumulative: every bound at or above an observation counts it
        assert_eq!(b[0], (100, 1), "50ms lands in le=100");
        assert_eq!(b[5], (10_000, 1), "45s is not <= 10s");
        assert_eq!(b[7], (60_000, 2), "both are <= 60s");
        assert_eq!(m.latency_count(), 2);
        assert_eq!(m.latency_sum_ms(), 45_050);
    }

    #[test]
    fn a_percentile_with_no_observations_is_none_not_zero() {
        // The whole point: "no recalls happened" must not read as
        // "every recall was instant". A rig asserting p99 < 5s against
        // a silent server would otherwise pass vacuously.
        let m = DelegMeter::default();
        assert_eq!(m.latency_percentile_ms(0.99), None);
        m.observe_recall_latency_ms(1);
        assert_eq!(m.latency_percentile_ms(0.99), Some(100));
    }

    #[test]
    fn p99_reports_the_bucket_that_actually_covers_the_tail() {
        let m = DelegMeter::default();
        for _ in 0..99 {
            m.observe_recall_latency_ms(10);
        }
        m.observe_recall_latency_ms(45_000);
        // 99 of 100 <= 100ms, so p99 is the 100ms bound; p999 must
        // climb past it rather than hiding the 45s straggler.
        assert_eq!(m.latency_percentile_ms(0.99), Some(100));
        assert_eq!(m.latency_percentile_ms(0.999), Some(60_000));
    }

    #[test]
    fn seq4_counts_follow_the_wire_bits_not_the_callers_opinion() {
        let m = DelegMeter::default();
        m.note_seq4(seq4_status::CB_PATH_DOWN);
        m.note_seq4(seq4_status::RECALLABLE_STATE_REVOKED);
        // a single raise carrying BOTH bits counts once for each
        m.note_seq4(seq4_status::CB_PATH_DOWN | seq4_status::RECALLABLE_STATE_REVOKED);
        assert_eq!(m.seq4_count(seq4_status::CB_PATH_DOWN), 2);
        assert_eq!(m.seq4_count(seq4_status::RECALLABLE_STATE_REVOKED), 2);
    }

    #[test]
    fn an_idle_interval_is_silent() {
        let t = DelegMeterTotals::default();
        assert!(t.is_zero());
        let moved = DelegMeterTotals { granted: 1, ..Default::default() };
        assert!(!moved.is_zero());
        assert_eq!(moved.delta(&t).granted, 1);
    }

    #[test]
    fn outcomes_and_revokes_are_counted_per_label() {
        let m = DelegMeter::default();
        m.note_outcome(RecallOutcome::Acked);
        m.note_outcome(RecallOutcome::Acked);
        m.note_outcome(RecallOutcome::PathDown);
        m.note_revoked(RevokeReason::Deadline);
        assert_eq!(m.outcome_count(RecallOutcome::Acked), 2);
        assert_eq!(m.outcome_count(RecallOutcome::Timeout), 0);
        assert_eq!(m.outcome_count(RecallOutcome::PathDown), 1);
        assert_eq!(m.revoked_count(RevokeReason::Deadline), 1);
        assert_eq!(m.revoked_total(), 1);
    }

    #[test]
    fn delay_sites_are_counted_separately() {
        let m = DelegMeter::default();
        m.note_delay("write");
        m.note_delay("write");
        m.note_delay("setattr");
        assert_eq!(m.delay_count("write"), 2);
        assert_eq!(m.delay_count("setattr"), 1);
        assert_eq!(m.delay_count("rename"), 0);
        assert_eq!(m.delays_total(), 3);
    }
}
