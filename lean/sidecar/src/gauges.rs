//! `.flint-sync/gauges.json` — the Phase-3 observability minimum
//! (§2.6; review ledger OF-6).
//!
//! The question this file exists to answer is "why is the manifest not
//! advancing", and it has to be answerable from inside the pod, with
//! `cat`, before Phase 6's `/metrics` exists. In `gated` especially, a
//! healthy tick and a wedged loop look identical from the outside:
//! both publish nothing.
//!
//! **Every field here is computed from LOCAL state — no bucket request,
//! ever.** That is enforced by the signature rather than by convention:
//! `write_gauges` is not `async` and takes no store, so it *cannot*
//! issue one. This is the "instrument reports on itself" class the
//! runas campaign paid for five times; here it would also make the
//! zero-added-cost oracle (leg B8) intermittently red and blame the
//! sidecar for its own instrument.
//!
//! Phase 6 renders `/metrics` from this same struct — one renderer over
//! one struct cannot drift; two computations would.

use serde::{Deserialize, Serialize};

use super::{now_unix, LeanResult, Sidecar};

const GAUGES: &str = "gauges.json";

/// Which coherent point last installed a citation, and what it named.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LastBoundary {
    pub source: String,
    pub seq: u64,
    pub unix: u64,
}

/// Why visibility is currently withheld. `None` = nothing is withheld.
///
/// (The plan's draft listed `copy-probe-failed`; §8 Q2 withdrew the
/// CopyObject staging engine entirely, so the analogous reason names
/// the machinery that actually exists — the versioning conformance
/// probe.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withheld {
    /// Staged work waiting for a coherent point that has not arrived.
    QuiescePending,
    /// Staged work past quiescence, waiting on this tick's citation.
    AwaitingBoundary,
    /// A foreign 412 parked at least one path (it is NOT ours to
    /// publish, and the conflict record says so).
    Parked412,
    /// The citation CAS lost its races.
    CasConflict,
    /// The version surface probe failed — gated mode is refused, not
    /// degraded.
    VersionProbeFailed,
}

impl Withheld {
    pub fn as_str(&self) -> &'static str {
        match self {
            Withheld::QuiescePending => "quiesce-pending",
            Withheld::AwaitingBoundary => "awaiting-boundary",
            Withheld::Parked412 => "parked-412",
            Withheld::CasConflict => "cas-conflict",
            Withheld::VersionProbeFailed => "version-probe-failed",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Gauges {
    /// `live` | `fenced` — must agree with `capabilities.json`. An
    /// agent reading only the operational file must not conclude a
    /// zombie is healthy.
    pub state: String,
    pub boundary_mode: String,
    /// Since the last DURABLE write (a staged byte or an installed
    /// barrier), not since the last visible one.
    ///
    /// It is elapsed time, NOT exposure: an idle healthy workspace has
    /// nothing at risk and a growing `rpo_secs`. Pair it with
    /// `staged_uncited_count`/`withheld_reason` — those carry whether
    /// there is anything to lose. Alerting on this number alone pages
    /// someone for a workspace that is simply quiet.
    pub rpo_secs: u64,
    /// Since the last CITATION. In cadence/hybrid this equals
    /// `rpo_secs` by construction; in gated it is the number the lag
    /// cap bounds.
    ///
    /// Deliberately the SAME arithmetic `citation_due` tests, elapsed
    /// time and all: a gauge that reported a different number than the
    /// mechanism it describes would be worse than no gauge — an
    /// operator would tune `visibilityLagBoundSecs` against a figure
    /// the cap never sees.
    pub visibility_lag_secs: u64,
    pub staged_uncited_count: u64,
    pub staged_uncited_bytes: u64,
    /// How long the OLDEST still-cited version has been noncurrent —
    /// D8's inversion, gauged. Gated staging makes the cited version
    /// noncurrent, so the retention backstop runs a clock against live
    /// cited data; this is that clock, and `noncurrentRetentionDays` is
    /// the number it must never reach.
    pub cited_noncurrent_age_max_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub withheld_reason: Option<String>,
    pub sentinel_budget_remaining: u64,
    /// Cumulative `forced-lag-cap` + `forced-backlog-cap` citations.
    /// A workspace that forces every citation has no coherence
    /// contract left, and this is how anyone finds out.
    pub forced_citation_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_boundary: Option<LastBoundary>,
    /// Unix time of the first renewal the store refused with 401/403
    /// (`StoreError::Auth`) since the last successful one — a
    /// credential or broker fault, never contention and never a lease
    /// conflict. `None` once a renewal succeeds.
    ///
    /// This is the ONLY liveness fact a credential-paused holder can
    /// still record. The renewal that would carry it into the lease
    /// echo is precisely the request that is failing, so the store
    /// cannot be told; local evidence is all there is. Without it an
    /// operator sees a lease going stale and a pod that is plainly
    /// Running, and nothing that connects the two — the diagnosis
    /// `StoreError::Auth` was split out of `Other` to make possible in
    /// the first place (`flint-store/src/lib.rs`), stopping one layer
    /// short of the binary that needed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_paused_since_unix: Option<u64>,
    /// Refreshed on EVERY tick, news or not: an idle-but-healthy
    /// workspace must be distinguishable from a dead one (the same
    /// heartbeat rule `remote.seq` carries).
    pub updated_unix: u64,
    /// Carried across ticks so `rpo_secs` measures durability rather
    /// than "time since this process started".
    #[serde(default)]
    pub last_durable_unix: u64,
}

impl Sidecar {
    fn gauges_path(&self) -> std::path::PathBuf {
        self.cfg.state_dir().join(GAUGES)
    }

    /// Last written gauges, or defaults. Never fails on a corrupt file:
    /// a diagnosis surface that panics is worse than a stale one.
    pub fn load_gauges(&self) -> LeanResult<Gauges> {
        let p = self.gauges_path();
        if !p.exists() {
            return Ok(Gauges::default());
        }
        Ok(std::fs::read(&p)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default())
    }

    /// Record that a citation installed. Bumps the forced counter when
    /// the source was a cap rather than a declared coherent point.
    pub fn note_boundary(&self, source: &str, seq: u64) -> LeanResult<()> {
        let mut g = self.load_gauges()?;
        if source.starts_with("forced-") {
            g.forced_citation_count += 1;
        }
        g.last_boundary = Some(LastBoundary { source: source.into(), seq, unix: now_unix() });
        g.last_durable_unix = now_unix();
        self.save_gauges(&g)
    }

    /// Record that bytes were made durable without being cited (the
    /// gated upload lane).
    pub fn note_durable(&self) -> LeanResult<()> {
        let mut g = self.load_gauges()?;
        g.last_durable_unix = now_unix();
        self.save_gauges(&g)
    }

    /// Record that the store refused our credentials on a renewal.
    /// First refusal wins: the gauge answers "since when", so a second
    /// consecutive failure must not keep resetting the clock to now.
    pub fn note_auth_pause(&self) -> LeanResult<()> {
        let mut g = self.load_gauges()?;
        if g.auth_paused_since_unix.is_none() {
            g.auth_paused_since_unix = Some(now_unix());
            self.save_gauges(&g)?;
        }
        Ok(())
    }

    /// Clear the pause. Writes only on an actual transition: this runs
    /// on every successful renewal, and the healthy path must not
    /// rewrite the file every heartbeat for no change.
    pub fn clear_auth_pause(&self) -> LeanResult<()> {
        let mut g = self.load_gauges()?;
        if g.auth_paused_since_unix.is_some() {
            g.auth_paused_since_unix = None;
            self.save_gauges(&g)?;
        }
        Ok(())
    }

    fn save_gauges(&self, g: &Gauges) -> LeanResult<()> {
        let bytes = serde_json::to_vec_pretty(g)
            .map_err(|e| super::LeanError::State(format!("gauges: {e}")))?;
        super::control::write_atomic(&self.gauges_path(), &bytes)
    }

    /// Recompute and write the gauges. Deliberately synchronous and
    /// store-free: a scrape, a tick and an exec all cost zero bucket
    /// requests, and the type system is what says so.
    pub fn write_gauges(&self, fenced: bool, withheld: Option<Withheld>) -> LeanResult<Gauges> {
        let now = now_unix();
        let prev = self.load_gauges()?;
        let stage = self.load_stage()?;
        let budget = self.load_budget().unwrap_or_default();

        let staged_bytes: u64 = stage.entries.values().map(|e| e.size).sum();
        // The noncurrent clock starts when a version BECOMES noncurrent
        // — i.e. when our staging PUT landed over it — so the oldest
        // staged entry is the one nearest the backstop.
        let oldest_stage = stage.entries.values().map(|e| e.staged_unix).min();
        let cited_noncurrent_age_max_secs =
            oldest_stage.map(|t| now.saturating_sub(t)).unwrap_or(0);

        // Withheld is a fact about the stage, so an argument of `None`
        // must not paper over staged work the caller forgot to describe.
        let withheld = withheld.or_else(|| {
            if stage.entries.is_empty() && stage.withheld_deletes.is_empty() {
                None
            } else if stage.stable_since_unix > 0 {
                Some(Withheld::AwaitingBoundary)
            } else {
                Some(Withheld::QuiescePending)
            }
        });

        let last_citation = stage.last_citation_unix.max(
            prev.last_boundary.as_ref().map(|b| b.unix).unwrap_or(0),
        );
        let g = Gauges {
            state: if fenced { "fenced".into() } else { "live".into() },
            boundary_mode: self.cfg.boundary_mode.as_str().to_string(),
            rpo_secs: if prev.last_durable_unix == 0 {
                0
            } else {
                now.saturating_sub(prev.last_durable_unix)
            },
            visibility_lag_secs: if last_citation == 0 {
                0
            } else {
                now.saturating_sub(last_citation)
            },
            staged_uncited_count: stage.entries.len() as u64,
            staged_uncited_bytes: staged_bytes,
            cited_noncurrent_age_max_secs,
            withheld_reason: withheld.map(|w| w.as_str().to_string()),
            sentinel_budget_remaining: budget.remaining(now, self.cfg.sentinel_hourly_budget),
            forced_citation_count: prev.forced_citation_count,
            last_boundary: prev.last_boundary.clone(),
            updated_unix: now,
            last_durable_unix: prev.last_durable_unix,
            // Carried, not recomputed: `write_gauges` is store-free by
            // construction, so it cannot observe the credential state
            // that set this. Dropping it here would erase the pause on
            // the very next tick — the gauge would exist and always
            // read `None`.
            auth_paused_since_unix: prev.auth_paused_since_unix,
        };
        self.save_gauges(&g)?;
        Ok(g)
    }
}

/// What `flint-sync status` renders. Read STRICTLY from files: the verb
/// exists to diagnose a workspace whose sidecar is dead or deposed, so
/// it must neither claim the lease (which would depose the very sidecar
/// under diagnosis) nor take the state-directory occupancy lock (which
/// a live sidecar already holds).
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub root: String,
    pub prefix: String,
    pub gauges: Option<Gauges>,
    pub capabilities: Option<super::control::Capabilities>,
    pub remote_seq: Option<super::control::RemoteSeq>,
    pub pending_stage_entries: usize,
    pub pending_stage_bytes: u64,
    pub withheld_deletes: usize,
    pub baseline_seq: u64,
    pub incarnation_epoch: Option<u64>,
    pub incarnation_holder: Option<String>,
    /// A pending sentinel still standing (verb name) — the "my agent is
    /// blocked on an ack" question.
    pub pending_sentinels: Vec<String>,
    pub recent_conflicts: usize,
    pub checkout_complete: bool,
}

fn read_json<T: serde::de::DeserializeOwned>(p: &std::path::Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

pub fn status_report(cfg: &super::LeanConfig) -> LeanResult<StatusReport> {
    let sd = cfg.state_dir();
    let cd = cfg.control_dir();
    let stage: super::gated::PendingStage = read_json(&sd.join("pending.json")).unwrap_or_default();
    let baseline: super::state::Baseline = read_json(&sd.join("baseline.json")).unwrap_or_default();
    let inc: Option<super::state::Incarnation> = read_json(&sd.join("incarnation.json"));
    let mut pending_sentinels = vec![];
    // Named by the SAME function that writes them. The previous form
    // built the name a second time and got it wrong, so this field —
    // the "is my agent blocked on an ack?" answer — was permanently
    // empty on a workspace that had a sentinel standing.
    for verb in [super::sentinel::Verb::Publish, super::sentinel::Verb::Sync] {
        if sd.join(verb.pending_name()).exists() {
            pending_sentinels.push(verb.sentinel_name().to_string());
        }
    }
    let conflicts = std::fs::read_to_string(sd.join("conflicts.jsonl"))
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    Ok(StatusReport {
        root: cfg.root.display().to_string(),
        prefix: cfg.prefix.clone(),
        gauges: read_json(&sd.join(GAUGES)),
        capabilities: read_json(&cd.join(super::control::CAPABILITIES)),
        remote_seq: read_json(&cd.join(super::control::REMOTE_SEQ)),
        pending_stage_entries: stage.entries.len(),
        pending_stage_bytes: stage.entries.values().map(|e| e.size).sum(),
        withheld_deletes: stage.withheld_deletes.len(),
        baseline_seq: baseline.seq,
        incarnation_epoch: inc.as_ref().map(|i| i.epoch),
        incarnation_holder: inc.map(|i| i.holder_id),
        pending_sentinels,
        recent_conflicts: conflicts,
        checkout_complete: sd.join("checkout-complete").exists(),
    })
}
