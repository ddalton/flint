//! The tier reporter — the last A12 deliverable: DataPathMeter-style,
//! SILENT WHEN IDLE (the F68a house pattern).
//!
//! One log line per interval, and only when something is true worth
//! saying: counter deltas moved (heartbeat renews excluded — routine),
//! a dirty backlog exists (a WEDGED flush must not be silent: its
//! signature is zero deltas + growing oldest-unflushed age), or a
//! hydration is in flight. WARNs: time-to-full below threshold, and
//! oldest-unflushed age beyond threshold. Publish 412s stay INFO —
//! they are arbitration inputs, never operator events (A6).
//!
//! S3 request cost: the outstanding-MPU probe (ListMultipartUploads)
//! runs only on ACTIVE intervals — a quiet tier sends zero requests.
//!
//! F68a note (A12's exclusion clause): in v1 the tier REFUSES any
//! non-standalone posture, and standalone registers no DSes — the
//! F68a WARN (which arms only with Active DSes) is structurally
//! disarmed, so there is no lane to exclude. Hydration I/O is direct
//! file I/O (never metered as MDS data) either way. When the full
//! pNFS+tier posture lands, evicted-serve READs must be excluded from
//! `mds_data_bytes` explicitly — that wiring belongs to whichever
//! step lifts the standalone restriction.
//!
//! Env knobs: FLINT_TIER_REPORT_SECS (default 60),
//! FLINT_TIER_WARN_TTF_MINS (default 30),
//! FLINT_TIER_WARN_UNFLUSHED_SECS (default 600).

use crate::state_backend::StateBackend;
use crate::tier::meter::MeterSnapshot;
use crate::tier::store::ObjectStore;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Point-in-time gauges the collector reads next to the counter delta.
#[derive(Debug, Default, Clone)]
pub struct Gauges {
    pub dirty_files: usize,
    /// Stat-sum of the dirty paths — the upper bound of what the next
    /// flush cycle uploads.
    pub dirty_bytes: u64,
    /// 0 when nothing is dirty.
    pub oldest_unflushed_secs: u64,
    pub hydration_inflight: usize,
    /// WARM (bulk-fill) restores among the inflight — the fill's
    /// live progress signal.
    pub warm_inflight: usize,
    pub evicted_files: usize,
    pub evicted_bytes: u64,
    /// None while the space gauge is not live.
    pub headroom_bytes: Option<u64>,
    /// used-bytes growth across this interval (0 when shrinking).
    pub used_growth_bytes: u64,
    pub mpu_count: usize,
    pub mpu_oldest_secs: u64,
    /// None = fenced.
    pub epoch: Option<u64>,
    pub epoch_renew_age_secs: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Knobs {
    pub interval_secs: u64,
    pub warn_ttf_secs: u64,
    pub warn_unflushed_secs: u64,
}

impl Default for Knobs {
    fn default() -> Self {
        Knobs { interval_secs: 60, warn_ttf_secs: 30 * 60, warn_unflushed_secs: 600 }
    }
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64)
    } else if b >= 1 << 20 {
        format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64)
    } else if b >= 1 << 10 {
        format!("{:.1} KiB", b as f64 / (1u64 << 10) as f64)
    } else {
        format!("{} B", b)
    }
}

fn fmt_secs(s: u64) -> String {
    if s >= 7200 {
        format!("{:.1}h", s as f64 / 3600.0)
    } else if s >= 120 {
        format!("{}m", s / 60)
    } else {
        format!("{}s", s)
    }
}

/// The pure half: delta + gauges → None (stay silent) or
/// (warn?, message). Split from the collector so the silence rule and
/// the WARN arithmetic are unit-testable.
pub fn compose(delta: &MeterSnapshot, g: &Gauges, k: &Knobs) -> Option<(bool, String)> {
    // Routine heartbeat renews are not activity.
    let mut act = *delta;
    act.epoch_renews = 0;
    if act.is_zero() && g.dirty_files == 0 && g.hydration_inflight == 0 {
        return None;
    }

    let mut seg: Vec<String> = Vec::new();
    let mut warn_reasons: Vec<String> = Vec::new();

    let up_bytes = delta.bytes_uploaded + delta.bytes_copied;
    if delta.publishes + delta.flushes_clean_match + delta.publish_failures + delta.publish_412s
        > 0
    {
        let mut s = format!("up {} gen/{}", delta.publishes, fmt_bytes(up_bytes));
        if delta.flushes_clean_match > 0 {
            s.push_str(&format!(" · clean {}", delta.flushes_clean_match));
        }
        if delta.publish_failures > 0 {
            s.push_str(&format!(" · fail {}", delta.publish_failures));
        }
        if delta.publish_412s > 0 {
            s.push_str(&format!(" · 412 {} (arbitrated)", delta.publish_412s));
        }
        seg.push(s);
    }
    if delta.flushes_fenced > 0 {
        seg.push(format!("fenced {}", delta.flushes_fenced));
    }

    if g.dirty_files > 0 {
        seg.push(format!(
            "dirty {}/{} oldest {}",
            g.dirty_files,
            fmt_bytes(g.dirty_bytes),
            fmt_secs(g.oldest_unflushed_secs)
        ));
        if g.oldest_unflushed_secs > k.warn_unflushed_secs {
            warn_reasons.push(format!(
                "oldest un-flushed file is {} old (threshold {})",
                fmt_secs(g.oldest_unflushed_secs),
                fmt_secs(k.warn_unflushed_secs)
            ));
        }
    }

    if delta.hydrations_completed
        + delta.hydration_failures
        + delta.evicted_op_delays
        + g.hydration_inflight as u64
        > 0
    {
        let mut s = format!(
            "hydrate {} done/{}",
            delta.hydrations_completed,
            fmt_bytes(delta.hydration_bytes)
        );
        if let Some(avg) = delta.hydration_millis.checked_div(delta.hydrations_completed) {
            s.push_str(&format!(" avg {}ms", avg));
        }
        if g.hydration_inflight > 0 {
            s.push_str(&format!(" · {} inflight", g.hydration_inflight));
        }
        if delta.hydration_failures > 0 {
            s.push_str(&format!(" · {} failed", delta.hydration_failures));
        }
        if delta.evicted_op_delays > 0 {
            s.push_str(&format!(" · {} DELAYs", delta.evicted_op_delays));
        }
        if delta.hydration_foreign_adopts > 0 {
            s.push_str(&format!(" · {} S3-wins adopts", delta.hydration_foreign_adopts));
        }
        seg.push(s);
    }

    // The warm fill (bulk hydration): progress while it runs, and a
    // WARN when it hit the space bound — files an operator asked to
    // pre-warm are staying cold.
    if g.warm_inflight > 0 || delta.warm_skipped_space + delta.warm_abandoned > 0 {
        let mut s = format!("warm {} inflight", g.warm_inflight);
        if delta.warm_skipped_space > 0 {
            s.push_str(&format!(" · {} skipped (space)", delta.warm_skipped_space));
            warn_reasons.push(format!(
                "warm fill stopped at the space bound ({} file(s) skipped) — the tree \
                 stays partially cold",
                delta.warm_skipped_space
            ));
        }
        if delta.warm_abandoned > 0 {
            s.push_str(&format!(" · {} abandoned", delta.warm_abandoned));
        }
        seg.push(s);
    }

    let refusals =
        delta.evict_refused_dirty + delta.evict_refused_policy + delta.evict_refused_verify;
    if delta.files_evicted > 0 || refusals > 0 || g.evicted_files > 0 {
        let mut s = String::new();
        if delta.files_evicted > 0 {
            s.push_str(&format!("evicted +{} ", delta.files_evicted));
        }
        s.push_str(&format!("→ {} files/{} cold", g.evicted_files, fmt_bytes(g.evicted_bytes)));
        if refusals > 0 {
            s.push_str(&format!(
                " · refused d{}/p{}/v{}",
                delta.evict_refused_dirty, delta.evict_refused_policy, delta.evict_refused_verify
            ));
        }
        seg.push(s);
    }

    if delta.import_stubs + delta.import_skipped_tombstoned > 0 {
        seg.push(format!(
            "import +{} stubs ({} tombstoned skipped)",
            delta.import_stubs, delta.import_skipped_tombstoned
        ));
    }
    if delta.manifest_writes > 0 {
        seg.push(format!("manifest +{}", delta.manifest_writes));
    }
    if delta.manifest_failures > 0 {
        seg.push(format!("manifest FAILED {}", delta.manifest_failures));
    }
    if delta.nospc_write_refusals + delta.nospc_create_refusals > 0 {
        seg.push(format!(
            "NOSPC w{}/c{}",
            delta.nospc_write_refusals, delta.nospc_create_refusals
        ));
    }
    if delta.ballast_releases > 0 {
        seg.push(format!("ballast RELEASED ×{}", delta.ballast_releases));
    }

    if let Some(headroom) = g.headroom_bytes {
        let mut s = format!("headroom {}", fmt_bytes(headroom));
        let rate = g.used_growth_bytes.checked_div(k.interval_secs).unwrap_or(0); // bytes/s
        if let Some(ttf) = headroom.checked_div(rate) {
            s.push_str(&format!(" (~{} to full)", fmt_secs(ttf)));
            if ttf < k.warn_ttf_secs {
                warn_reasons.push(format!(
                    "PVC full in ~{} at the current write rate",
                    fmt_secs(ttf)
                ));
            }
        }
        seg.push(s);
    }

    if g.mpu_count > 0 {
        seg.push(format!("mpu {} (oldest {})", g.mpu_count, fmt_secs(g.mpu_oldest_secs)));
    }

    match g.epoch {
        Some(e) => seg.push(format!("epoch {} ({} since renew)", e, fmt_secs(g.epoch_renew_age_secs))),
        None => seg.push("epoch FENCED".into()),
    }
    if delta.epoch_renew_failures > 0 {
        seg.push(format!("renew failures {}", delta.epoch_renew_failures));
    }

    let msg = seg.join(" · ");
    if warn_reasons.is_empty() {
        Some((false, msg))
    } else {
        Some((true, format!("{} — {}", warn_reasons.join("; "), msg)))
    }
}

/// The collector task (start_tier spawns it after the tier is up).
pub fn spawn(
    backend: Arc<dyn StateBackend>,
    store: Arc<dyn ObjectStore>,
    key_prefix: String,
    guard: Arc<crate::tier::epoch::EpochGuard>,
) -> tokio::task::JoinHandle<()> {
    let interval_secs = std::env::var("FLINT_TIER_REPORT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(60);
    let knobs = Knobs {
        interval_secs,
        warn_ttf_secs: std::env::var("FLINT_TIER_WARN_TTF_MINS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30)
            .saturating_mul(60),
        warn_unflushed_secs: std::env::var("FLINT_TIER_WARN_UNFLUSHED_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(600),
    };
    tokio::spawn(async move {
        let mut prev = crate::tier::meter::snapshot();
        let mut prev_used: Option<u64> = None;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // immediate first fire: skip
        loop {
            tick.tick().await;
            let cur = crate::tier::meter::snapshot();
            let delta = cur.delta_since(&prev);
            prev = cur;

            let mut g = Gauges::default();
            // Space first (also feeds the growth tracker every
            // interval, active or not, so a later rate is honest).
            if let Some(v) = crate::tier::space::view() {
                let used = v.total_bytes.saturating_sub(v.free_bytes);
                g.headroom_bytes = Some(v.avail_bytes);
                g.used_growth_bytes = prev_used
                    .map(|p| used.saturating_sub(p))
                    .unwrap_or(0);
                prev_used = Some(used);
            }
            let (ef, eb) = crate::tier::evict::marker_stats();
            g.evicted_files = ef;
            g.evicted_bytes = eb;
            g.hydration_inflight = crate::tier::hydrate::inflight_count();
            g.warm_inflight = crate::tier::hydrate::warm_inflight();
            g.epoch = guard.current();
            g.epoch_renew_age_secs = guard.renew_age_secs();
            if let Ok(rows) = backend.tier_list_dirty().await {
                g.dirty_files = rows.len();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                for r in &rows {
                    g.oldest_unflushed_secs =
                        g.oldest_unflushed_secs.max(now.saturating_sub(r.dirtied_unix));
                    if let Some(p) = &r.path {
                        if let Ok(md) = std::fs::symlink_metadata(p) {
                            g.dirty_bytes += md.len();
                        }
                    }
                }
            }

            // The MPU probe costs an S3 LIST — only on active
            // intervals (compose's own silence rule, pre-checked).
            let mut act = delta;
            act.epoch_renews = 0;
            let active = !act.is_zero() || g.dirty_files > 0 || g.hydration_inflight > 0;
            if !active {
                continue;
            }
            match store.list_uploads(&key_prefix).await {
                Ok(ups) => {
                    g.mpu_count = ups.len();
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    g.mpu_oldest_secs = ups
                        .iter()
                        .filter_map(|u| u.initiated_unix)
                        .map(|t| now.saturating_sub(t))
                        .max()
                        .unwrap_or(0);
                }
                Err(e) => debug!("tier reporter: MPU probe failed: {}", e),
            }

            match compose(&delta, &g, &knobs) {
                Some((true, msg)) => warn!("🚨 🪣 tier last {}s: {}", interval_secs, msg),
                Some((false, msg)) => info!("📊 🪣 tier last {}s: {}", interval_secs, msg),
                None => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held_gauges() -> Gauges {
        Gauges { epoch: Some(3), epoch_renew_age_secs: 4, ..Gauges::default() }
    }

    #[test]
    fn idle_stays_silent_even_through_heartbeats() {
        let mut delta = MeterSnapshot::default();
        delta.epoch_renews = 6; // routine heartbeat traffic
        assert_eq!(compose(&delta, &held_gauges(), &Knobs::default()), None);
    }

    #[test]
    fn a_wedged_flush_is_not_silent() {
        // Zero deltas but a standing dirty backlog: the wedge shape.
        let delta = MeterSnapshot::default();
        let mut g = held_gauges();
        g.dirty_files = 3;
        g.dirty_bytes = 5 << 20;
        g.oldest_unflushed_secs = 700;
        let (warned, msg) = compose(&delta, &g, &Knobs::default()).expect("must report");
        assert!(warned, "past warn_unflushed_secs ⇒ WARN: {}", msg);
        assert!(msg.contains("dirty 3/5.0 MiB"), "{}", msg);
    }

    #[test]
    fn activity_renders_the_segments() {
        let mut delta = MeterSnapshot::default();
        delta.publishes = 2;
        delta.bytes_uploaded = 3 << 20;
        delta.hydrations_completed = 2;
        delta.hydration_bytes = 16 << 20;
        delta.hydration_millis = 840;
        delta.evicted_op_delays = 12;
        delta.manifest_writes = 1;
        let mut g = held_gauges();
        g.evicted_files = 7;
        g.evicted_bytes = 1 << 30;
        g.headroom_bytes = Some(50 << 30);
        let (warned, msg) = compose(&delta, &g, &Knobs::default()).expect("must report");
        assert!(!warned, "{}", msg);
        for want in [
            "up 2 gen/3.0 MiB",
            "hydrate 2 done/16.0 MiB avg 420ms",
            "12 DELAYs",
            "7 files/1.0 GiB cold",
            "manifest +1",
            "headroom 50.0 GiB",
            "epoch 3 (4s since renew)",
        ] {
            assert!(msg.contains(want), "missing {:?} in {}", want, msg);
        }
        assert!(!msg.contains("to full"), "no growth ⇒ no ETA: {}", msg);
    }

    #[test]
    fn time_to_full_warns_below_threshold() {
        let mut delta = MeterSnapshot::default();
        delta.publishes = 1;
        let mut g = held_gauges();
        // 1 GiB headroom, ~10 MiB/s growth ⇒ ~102s to full.
        g.headroom_bytes = Some(1 << 30);
        g.used_growth_bytes = 600 << 20; // per 60s interval
        let (warned, msg) = compose(&delta, &g, &Knobs::default()).expect("must report");
        assert!(warned, "{}", msg);
        assert!(msg.contains("to full"), "{}", msg);

        // Same growth against a huge PVC: INFO with an ETA.
        g.headroom_bytes = Some(4 << 40);
        let (warned2, msg2) = compose(&delta, &g, &Knobs::default()).unwrap();
        assert!(!warned2, "{}", msg2);
        assert!(msg2.contains("to full"), "{}", msg2);
    }

    #[test]
    fn fenced_epoch_is_named() {
        let mut delta = MeterSnapshot::default();
        delta.flushes_fenced = 2;
        let mut g = held_gauges();
        g.epoch = None;
        let (_, msg) = compose(&delta, &g, &Knobs::default()).expect("must report");
        assert!(msg.contains("epoch FENCED"), "{}", msg);
        assert!(msg.contains("fenced 2"), "{}", msg);
    }
}
