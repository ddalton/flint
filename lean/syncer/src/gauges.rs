//! `.flint-sync/gauges.json` — the Phase-3 observability minimum
//! (§2.6; review ledger OF-6).
//!
//! The question this file exists to answer is "why is the manifest not
//! advancing", and it has to be answerable from inside the pod, with
//! `cat`, before Phase 6's `/metrics` exists. A healthy no-change tick
//! and a wedged loop look identical from the outside: both publish
//! nothing.
//!
//! **Every field here is computed from LOCAL state — no bucket request,
//! ever.** That is enforced by the signature rather than by convention:
//! `write_gauges` is not `async` and takes no store, so it *cannot*
//! issue one. This is the "instrument reports on itself" class the
//! runas campaign paid for five times; here it would also make the
//! zero-added-cost oracle (leg B8) intermittently red and blame the
//! syncer for its own instrument.
//!
//! Phase 6 renders `/metrics` from this same struct — one renderer over
//! one struct cannot drift; two computations would.

use serde::{Deserialize, Serialize};

use super::{now_unix, LeanResult, Syncer};

const GAUGES: &str = "gauges.json";

/// Which coherent point last installed a citation, and what it named.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LastBoundary {
    pub source: String,
    pub seq: u64,
    pub unix: u64,
}

/// Why visibility is currently withheld. `None` = nothing is withheld.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withheld {
    /// A foreign 412 parked at least one path (it is NOT ours to
    /// publish, and the conflict record says so).
    Parked412,
}

impl Withheld {
    pub fn as_str(&self) -> &'static str {
        match self {
            Withheld::Parked412 => "parked-412",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Gauges {
    /// Since the last boundary was installed. Elapsed time, NOT
    /// exposure: an idle healthy workspace has nothing at risk and a
    /// growing `rpo_secs`. Pair it with `withheld_reason`, which carries
    /// whether there is anything to lose. Alerting on this number alone
    /// pages someone for a workspace that is simply quiet.
    pub rpo_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub withheld_reason: Option<String>,
    pub sentinel_budget_remaining: u64,
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

impl Syncer {
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

    /// Record that a boundary installed.
    pub fn note_boundary(&self, source: &str, seq: u64) -> LeanResult<()> {
        let mut g = self.load_gauges()?;
        g.last_boundary = Some(LastBoundary { source: source.into(), seq, unix: now_unix() });
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
    /// on every successful renewal and floor tick, and the healthy path
    /// must not rewrite the file every tick for no change.
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
    pub fn write_gauges(&self, withheld: Option<Withheld>) -> LeanResult<Gauges> {
        let now = now_unix();
        let prev = self.load_gauges()?;
        let budget = self.load_budget().unwrap_or_default();
        let g = Gauges {
            rpo_secs: if prev.last_durable_unix == 0 {
                0
            } else {
                now.saturating_sub(prev.last_durable_unix)
            },
            withheld_reason: withheld.map(|w| w.as_str().to_string()),
            sentinel_budget_remaining: budget.remaining(now, self.cfg.sentinel_hourly_budget),
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
/// exists to diagnose a workspace whose syncer is dead or deposed, so
/// it must neither claim the lease (which would depose the very syncer
/// under diagnosis) nor take the state-directory occupancy lock (which
/// a live syncer already holds).
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub root: String,
    pub prefix: String,
    pub gauges: Option<Gauges>,
    pub capabilities: Option<super::control::Capabilities>,
    pub remote_seq: Option<super::control::RemoteSeq>,
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
        baseline_seq: baseline.seq,
        incarnation_epoch: inc.as_ref().map(|i| i.epoch),
        incarnation_holder: inc.map(|i| i.holder_id),
        pending_sentinels,
        recent_conflicts: conflicts,
        checkout_complete: sd.join("checkout-complete").exists(),
    })
}
