//! Kube-Lease leader election for the orchestrator block — the MECHANISM
//! behind what `orchestrator_role` decides by CONFIGURATION.
//!
//! # Why this exists (F50, F53 — twice in one day)
//!
//! The controller-side orchestrators (epoch scheduler, catch-up, cutover,
//! hot-rejoin, NFS reconciler) coordinate through `volume_claims::global()`,
//! which is IN-PROCESS memory: two processes running the block arbitrate
//! nothing against each other. F50 (the vestigial operator pod) and F53 (the
//! dashboard backend) were both processes that drifted into the orchestrator
//! role through configuration alone — one scrubbed the real controller's
//! admission windows into a livelock, the other silently performed a live
//! raid admission itself. `orchestrator_role` fixed the two SHIPPED
//! misconfigurations; it cannot prevent the next one, because an env grant
//! is an honor system. This module makes the singleton mechanical: the five
//! orchestrator loops act only while this process holds the
//! `flint-orchestrators` Lease, which the API server hands to exactly one
//! holder at a time (compare-and-swap on resourceVersion).
//!
//! # Division of labour
//!
//! - `orchestrator_role::orchestrators_enabled()` decides CANDIDACY: who may
//!   campaign at all. The dashboard backend's `FLINT_ORCHESTRATORS=disabled`
//!   keeps it out of the election entirely — the F53 decision stands; this
//!   module never re-admits a process the role grant excluded.
//! - The Lease decides ACTIVITY among candidates: with a correct chart there
//!   is one candidate and the lease changes nothing; with a mis-granted
//!   second candidate (the F50/F53 class recurring) the lease keeps it
//!   standing by instead of orchestrating.
//!
//! # Gating granularity, honestly
//!
//! Orchestrator loops check `is_leader()` at TICK granularity. An operation
//! already in flight when leadership is lost (a catch-up copy, a ~250ms
//! admission window) is not interrupted — interrupting a half-built window
//! is exactly the F50 failure shape. The residual two-orchestrator exposure
//! is therefore one in-flight operation during a pathological process pause,
//! against which the F43 claim belts and the reconcile grace remain the
//! defense in depth. The lease reduces the exposure from "standing
//! misconfiguration, unbounded" to "seconds, under a pause".
//!
//! # Clock-skew independence
//!
//! Takeover decisions never compare a remote timestamp against the local
//! clock. Like client-go's LeaderElector, the elector keys on when IT
//! observed the lease record change: a holder is considered failed only
//! after the record has been seen UNCHANGED for a full lease duration on
//! this process's own monotonic clock.
//!
//! # Failure directions
//!
//! - Lease API unreachable while leading: keep orchestrating until renewal
//!   has failed for a full lease duration, then STEP DOWN (stop the loops).
//!   The safe direction — the orchestrators need the same API server to do
//!   anything real, and a partitioned ex-leader must not fight its
//!   successor.
//! - Lease API unreachable while standing by: keep campaigning, stay idle.
//! - `FLINT_ORCHESTRATOR_LEASE=disabled`: no election, `is_leader()` is
//!   permanently true — v1.21.0 behavior, for dev and emergencies. The same
//!   is true when no election was ever started (bare binaries, unit tests):
//!   a process that never campaigns is not demoted by this module.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Process-global leadership state
// ---------------------------------------------------------------------------

const NO_ELECTION: u8 = 0; // never campaigned — act as leader (dev/tests/kill switch)
const STANDING_BY: u8 = 1; // campaigning, lease held elsewhere — orchestrators idle
const LEADER: u8 = 2; // holding the lease — orchestrators act

static STATE: AtomicU8 = AtomicU8::new(NO_ELECTION);

/// May the orchestrator loops act on this tick? True unless this process is
/// actively campaigning and NOT holding the lease. Checked at the top of
/// every orchestrator tick.
pub fn is_leader() -> bool {
    STATE.load(Ordering::SeqCst) != STANDING_BY
}

fn set_state(s: u8) {
    STATE.store(s, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct LeaseConfig {
    pub enabled: bool,
    pub lease_name: String,
    /// A holder unrenewed for this long (as observed locally) is failed.
    pub lease_duration: Duration,
    /// Renew cadence while leading.
    pub renew_period: Duration,
    /// Re-check cadence while standing by / after errors.
    pub retry_period: Duration,
}

impl LeaseConfig {
    pub fn from_env() -> Self {
        Self::from_setting(
            std::env::var("FLINT_ORCHESTRATOR_LEASE").ok().as_deref(),
        )
    }

    /// Pure form for tests: `None`/unparseable/"enabled" → on (the default);
    /// only an explicit disable turns the mechanism off.
    pub fn from_setting(raw: Option<&str>) -> Self {
        let enabled = !matches!(
            raw.map(|r| r.trim().to_ascii_lowercase()).as_deref(),
            Some("disabled") | Some("false") | Some("0") | Some("no") | Some("off")
        );
        LeaseConfig {
            enabled,
            lease_name: "flint-orchestrators".to_string(),
            lease_duration: Duration::from_secs(15),
            renew_period: Duration::from_secs(5),
            retry_period: Duration::from_secs(2),
        }
    }
}

// ---------------------------------------------------------------------------
// Lease record + API abstraction (mockable; kube impl below)
// ---------------------------------------------------------------------------

/// The subset of coordination.k8s.io/v1 Lease this module reads and writes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LeaseRecord {
    pub holder: Option<String>,
    /// Opaque renew stamp — compared for CHANGE, never against a clock.
    pub renew_stamp: Option<String>,
    pub transitions: i32,
    /// CAS token; `replace` must fail on mismatch.
    pub resource_version: Option<String>,
}

#[async_trait]
pub trait LeaseOps: Send + Sync {
    /// `Ok(None)` = lease absent (404).
    async fn get(&self) -> Result<Option<LeaseRecord>, String>;
    /// `Ok(false)` = already exists (lost the create race).
    async fn create(&self, holder: &str) -> Result<bool, String>;
    /// CAS replace: take/renew the lease. `Ok(false)` = conflict (lost).
    async fn replace(&self, prev: &LeaseRecord, holder: &str, transitions: i32)
        -> Result<bool, String>;
}

// ---------------------------------------------------------------------------
// The elector state machine (pure, clock-injected, unit-tested)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub(crate) enum Verdict {
    /// Lease absent — create it.
    Create,
    /// Holder failed (record unchanged a full lease duration) or vacated —
    /// CAS-take it.
    TakeOver,
    /// We hold it — CAS-renew.
    Renew,
    /// A live foreign holder — stay idle.
    StandBy,
}

pub(crate) struct Elector {
    id: String,
    lease_duration: Duration,
    /// (holder|renew_stamp key, when WE first observed exactly this record).
    observed: Option<(String, Instant)>,
}

impl Elector {
    pub(crate) fn new(id: String, lease_duration: Duration) -> Self {
        Elector { id, lease_duration, observed: None }
    }

    fn record_key(rec: &LeaseRecord) -> String {
        format!(
            "{}|{}",
            rec.holder.as_deref().unwrap_or(""),
            rec.renew_stamp.as_deref().unwrap_or("")
        )
    }

    /// Assess the currently observed lease. `now` is injected for tests;
    /// callers pass `Instant::now()`.
    pub(crate) fn assess(&mut self, lease: Option<&LeaseRecord>, now: Instant) -> Verdict {
        let rec = match lease {
            None => {
                self.observed = None;
                return Verdict::Create;
            }
            Some(r) => r,
        };
        if rec.holder.as_deref() == Some(self.id.as_str()) {
            return Verdict::Renew;
        }
        if rec.holder.as_deref().unwrap_or("").is_empty() {
            // Vacated (graceful release) — free to take immediately.
            return Verdict::TakeOver;
        }
        // Foreign holder: failed only once WE have seen this exact record
        // stand unchanged for a full lease duration. Remote timestamps are
        // never compared to our clock (skew independence).
        let key = Self::record_key(rec);
        match &self.observed {
            Some((seen_key, since)) if *seen_key == key => {
                if now.duration_since(*since) >= self.lease_duration {
                    Verdict::TakeOver
                } else {
                    Verdict::StandBy
                }
            }
            _ => {
                self.observed = Some((key, now));
                Verdict::StandBy
            }
        }
    }
}

// ---------------------------------------------------------------------------
// One election step (pure enough to test with a mock LeaseOps)
// ---------------------------------------------------------------------------

pub(crate) struct StepOutcome {
    pub leader: bool,
    /// Whether the lease API answered this step (renew-liveness bookkeeping).
    pub api_ok: bool,
}

pub(crate) async fn election_step(
    elector: &mut Elector,
    ops: &dyn LeaseOps,
    now: Instant,
) -> StepOutcome {
    let lease = match ops.get().await {
        Ok(l) => l,
        Err(_) => return StepOutcome { leader: false, api_ok: false },
    };
    match elector.assess(lease.as_ref(), now) {
        Verdict::Create => match ops.create(&elector.id).await {
            Ok(true) => StepOutcome { leader: true, api_ok: true },
            Ok(false) => StepOutcome { leader: false, api_ok: true },
            Err(_) => StepOutcome { leader: false, api_ok: false },
        },
        Verdict::TakeOver => {
            let prev = lease.unwrap_or_default();
            let transitions = prev.transitions + 1;
            match ops.replace(&prev, &elector.id, transitions).await {
                Ok(true) => StepOutcome { leader: true, api_ok: true },
                Ok(false) => StepOutcome { leader: false, api_ok: true },
                Err(_) => StepOutcome { leader: false, api_ok: false },
            }
        }
        Verdict::Renew => {
            let prev = lease.unwrap_or_default();
            let transitions = prev.transitions;
            match ops.replace(&prev, &elector.id, transitions).await {
                Ok(true) => StepOutcome { leader: true, api_ok: true },
                // Conflict on our own renew = we were fenced out.
                Ok(false) => StepOutcome { leader: false, api_ok: true },
                Err(_) => StepOutcome { leader: false, api_ok: false },
            }
        }
        Verdict::StandBy => StepOutcome { leader: false, api_ok: true },
    }
}

// ---------------------------------------------------------------------------
// The long-running election loop
// ---------------------------------------------------------------------------

/// Campaign forever. Flips the process-global leadership state that
/// `is_leader()` reports; the orchestrator loops read it every tick.
pub async fn run_election(ops: Arc<dyn LeaseOps>, id: String, cfg: LeaseConfig) {
    set_state(STANDING_BY);
    info!(
        holder = %id, lease = %cfg.lease_name,
        duration_secs = cfg.lease_duration.as_secs(),
        "🗳️ [ORCH_LEASE] campaigning — orchestrators idle until the lease is held"
    );
    let mut elector = Elector::new(id.clone(), cfg.lease_duration);
    let mut was_leader = false;
    // While leading: last time the lease API confirmed our hold. Once it has
    // been silent/failing for a full lease duration, step down.
    let mut last_confirmed = Instant::now();
    loop {
        let now = Instant::now();
        let out = election_step(&mut elector, ops.as_ref(), now).await;

        let leader_now = if out.api_ok {
            last_confirmed = now;
            out.leader
        } else if was_leader && now.duration_since(last_confirmed) < cfg.lease_duration {
            // API flake within budget — keep acting; renewal will retry.
            true
        } else {
            false
        };

        if leader_now && !was_leader {
            info!(holder = %id, "🗳️ [ORCH_LEASE] ACQUIRED — this process ACTS as the orchestrator");
        } else if !leader_now && was_leader {
            warn!(
                holder = %id,
                "🗳️ [ORCH_LEASE] STEPPED DOWN — lease lost or unrenewable; orchestrators idle (in-flight operations finish, new ticks skip)"
            );
        }
        set_state(if leader_now { LEADER } else { STANDING_BY });
        was_leader = leader_now;

        let sleep = if leader_now { cfg.renew_period } else { cfg.retry_period };
        tokio::time::sleep(sleep).await;
    }
}

// ---------------------------------------------------------------------------
// Kube implementation
// ---------------------------------------------------------------------------

pub struct KubeLeaseOps {
    api: kube::Api<k8s_openapi::api::coordination::v1::Lease>,
    name: String,
    lease_duration_secs: i32,
}

impl KubeLeaseOps {
    pub fn new(client: kube::Client, namespace: &str, cfg: &LeaseConfig) -> Self {
        KubeLeaseOps {
            api: kube::Api::namespaced(client, namespace),
            name: cfg.lease_name.clone(),
            lease_duration_secs: cfg.lease_duration.as_secs() as i32,
        }
    }

    fn is_conflict(e: &kube::Error) -> bool {
        matches!(e, kube::Error::Api(ae) if ae.code == 409)
    }

    fn micro_now() -> k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime {
        k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(
            k8s_openapi::jiff::Timestamp::now(),
        )
    }

    fn spec(
        &self,
        holder: &str,
        acquire: k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime,
        transitions: i32,
    ) -> k8s_openapi::api::coordination::v1::LeaseSpec {
        k8s_openapi::api::coordination::v1::LeaseSpec {
            holder_identity: Some(holder.to_string()),
            lease_duration_seconds: Some(self.lease_duration_secs),
            acquire_time: Some(acquire),
            renew_time: Some(Self::micro_now()),
            lease_transitions: Some(transitions),
            ..Default::default()
        }
    }
}

#[async_trait]
impl LeaseOps for KubeLeaseOps {
    async fn get(&self) -> Result<Option<LeaseRecord>, String> {
        match self.api.get_opt(&self.name).await {
            Ok(None) => Ok(None),
            Ok(Some(l)) => {
                let spec = l.spec.unwrap_or_default();
                Ok(Some(LeaseRecord {
                    holder: spec.holder_identity,
                    renew_stamp: spec.renew_time.map(|t| t.0.to_string()),
                    transitions: spec.lease_transitions.unwrap_or(0),
                    resource_version: l.metadata.resource_version,
                }))
            }
            Err(e) => Err(e.to_string()),
        }
    }

    async fn create(&self, holder: &str) -> Result<bool, String> {
        let now = Self::micro_now();
        let lease = k8s_openapi::api::coordination::v1::Lease {
            metadata: kube::api::ObjectMeta {
                name: Some(self.name.clone()),
                ..Default::default()
            },
            spec: Some(self.spec(holder, now, 0)),
        };
        match self.api.create(&kube::api::PostParams::default(), &lease).await {
            Ok(_) => Ok(true),
            Err(e) if Self::is_conflict(&e) => Ok(false),
            // A create losing to a concurrent create surfaces as 409 via
            // AlreadyExists; some servers report it as a plain API error —
            // classify any AlreadyExists-shaped failure as a lost race.
            Err(kube::Error::Api(ae)) if ae.reason == "AlreadyExists" => Ok(false),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn replace(
        &self,
        prev: &LeaseRecord,
        holder: &str,
        transitions: i32,
    ) -> Result<bool, String> {
        let acquire = Self::micro_now();
        let lease = k8s_openapi::api::coordination::v1::Lease {
            metadata: kube::api::ObjectMeta {
                name: Some(self.name.clone()),
                // The CAS: replace fails 409 unless this still matches.
                resource_version: prev.resource_version.clone(),
                ..Default::default()
            },
            spec: Some(self.spec(holder, acquire, transitions)),
        };
        match self.api.replace(&self.name, &kube::api::PostParams::default(), &lease).await {
            Ok(_) => Ok(true),
            Err(e) if Self::is_conflict(&e) => Ok(false),
            Err(e) => Err(e.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn rec(holder: &str, stamp: &str, rv: &str) -> LeaseRecord {
        LeaseRecord {
            holder: Some(holder.to_string()),
            renew_stamp: Some(stamp.to_string()),
            transitions: 0,
            resource_version: Some(rv.to_string()),
        }
    }

    const DUR: Duration = Duration::from_secs(15);

    #[test]
    fn absent_lease_is_created() {
        let mut e = Elector::new("me".into(), DUR);
        assert_eq!(e.assess(None, Instant::now()), Verdict::Create);
    }

    #[test]
    fn own_lease_is_renewed() {
        let mut e = Elector::new("me".into(), DUR);
        let r = rec("me", "t1", "1");
        assert_eq!(e.assess(Some(&r), Instant::now()), Verdict::Renew);
    }

    #[test]
    fn fresh_foreign_holder_means_stand_by_even_with_an_ancient_renew_stamp() {
        // Clock-skew independence: the stamp is opaque. However old the
        // REMOTE timestamp claims to be, the first local observation starts
        // the failure timer at zero.
        let mut e = Elector::new("me".into(), DUR);
        let r = rec("other", "1970-01-01T00:00:00Z", "1");
        assert_eq!(e.assess(Some(&r), Instant::now()), Verdict::StandBy);
    }

    #[test]
    fn unchanged_foreign_record_is_taken_over_after_a_full_lease_duration() {
        let mut e = Elector::new("me".into(), DUR);
        let base = Instant::now();
        let r = rec("other", "t1", "1");
        assert_eq!(e.assess(Some(&r), base), Verdict::StandBy);
        assert_eq!(e.assess(Some(&r), base + Duration::from_secs(10)), Verdict::StandBy);
        assert_eq!(e.assess(Some(&r), base + Duration::from_secs(15)), Verdict::TakeOver);
    }

    #[test]
    fn a_renewing_holder_resets_the_failure_timer() {
        let mut e = Elector::new("me".into(), DUR);
        let base = Instant::now();
        assert_eq!(e.assess(Some(&rec("other", "t1", "1")), base), Verdict::StandBy);
        // The holder renewed (stamp changed) just before our deadline —
        // timer restarts from this observation.
        let renewed = rec("other", "t2", "2");
        assert_eq!(
            e.assess(Some(&renewed), base + Duration::from_secs(14)),
            Verdict::StandBy
        );
        assert_eq!(
            e.assess(Some(&renewed), base + Duration::from_secs(28)),
            Verdict::StandBy
        );
        assert_eq!(
            e.assess(Some(&renewed), base + Duration::from_secs(29)),
            Verdict::TakeOver
        );
    }

    #[test]
    fn a_vacated_lease_is_taken_immediately() {
        let mut e = Elector::new("me".into(), DUR);
        let r = LeaseRecord {
            holder: None,
            renew_stamp: Some("t1".into()),
            transitions: 3,
            resource_version: Some("9".into()),
        };
        assert_eq!(e.assess(Some(&r), Instant::now()), Verdict::TakeOver);
    }

    // -- election_step against a mock ---------------------------------------

    struct MockOps {
        lease: Mutex<Option<LeaseRecord>>,
        conflict_on_write: Mutex<bool>,
        fail_api: Mutex<bool>,
    }

    impl MockOps {
        fn new(lease: Option<LeaseRecord>) -> Self {
            MockOps {
                lease: Mutex::new(lease),
                conflict_on_write: Mutex::new(false),
                fail_api: Mutex::new(false),
            }
        }
    }

    #[async_trait]
    impl LeaseOps for MockOps {
        async fn get(&self) -> Result<Option<LeaseRecord>, String> {
            if *self.fail_api.lock().unwrap() {
                return Err("api down".into());
            }
            Ok(self.lease.lock().unwrap().clone())
        }
        async fn create(&self, holder: &str) -> Result<bool, String> {
            if *self.fail_api.lock().unwrap() {
                return Err("api down".into());
            }
            if *self.conflict_on_write.lock().unwrap() {
                return Ok(false);
            }
            *self.lease.lock().unwrap() =
                Some(rec(holder, "created", "1"));
            Ok(true)
        }
        async fn replace(
            &self,
            _prev: &LeaseRecord,
            holder: &str,
            transitions: i32,
        ) -> Result<bool, String> {
            if *self.fail_api.lock().unwrap() {
                return Err("api down".into());
            }
            if *self.conflict_on_write.lock().unwrap() {
                return Ok(false);
            }
            let mut l = self.lease.lock().unwrap();
            let mut r = rec(holder, "renewed", "2");
            r.transitions = transitions;
            *l = Some(r);
            Ok(true)
        }
    }

    #[tokio::test]
    async fn step_acquires_an_absent_lease() {
        let ops = MockOps::new(None);
        let mut e = Elector::new("me".into(), DUR);
        let out = election_step(&mut e, &ops, Instant::now()).await;
        assert!(out.leader && out.api_ok);
        assert_eq!(
            ops.lease.lock().unwrap().as_ref().unwrap().holder.as_deref(),
            Some("me")
        );
    }

    #[tokio::test]
    async fn step_loses_the_create_race_gracefully() {
        let ops = MockOps::new(None);
        *ops.conflict_on_write.lock().unwrap() = true;
        let mut e = Elector::new("me".into(), DUR);
        let out = election_step(&mut e, &ops, Instant::now()).await;
        assert!(!out.leader && out.api_ok);
    }

    #[tokio::test]
    async fn step_takes_over_an_expired_holder_and_bumps_transitions() {
        let ops = MockOps::new(Some(rec("other", "t1", "1")));
        let mut e = Elector::new("me".into(), DUR);
        let base = Instant::now();
        // First observation arms the timer …
        let out = election_step(&mut e, &ops, base).await;
        assert!(!out.leader);
        // … expiry allows the CAS take-over.
        let out = election_step(&mut e, &ops, base + Duration::from_secs(16)).await;
        assert!(out.leader);
        let l = ops.lease.lock().unwrap();
        let l = l.as_ref().unwrap();
        assert_eq!(l.holder.as_deref(), Some("me"));
        assert_eq!(l.transitions, 1, "a take-over must record a leadership transition");
    }

    #[tokio::test]
    async fn step_steps_down_when_fenced_out_of_its_own_renew() {
        // We believe we hold it; the CAS conflict says otherwise.
        let ops = MockOps::new(Some(rec("me", "t1", "1")));
        *ops.conflict_on_write.lock().unwrap() = true;
        let mut e = Elector::new("me".into(), DUR);
        let out = election_step(&mut e, &ops, Instant::now()).await;
        assert!(!out.leader && out.api_ok);
    }

    #[tokio::test]
    async fn step_reports_api_failure_distinctly() {
        let ops = MockOps::new(None);
        *ops.fail_api.lock().unwrap() = true;
        let mut e = Elector::new("me".into(), DUR);
        let out = election_step(&mut e, &ops, Instant::now()).await;
        assert!(!out.leader && !out.api_ok);
    }

    // -- config + global state ----------------------------------------------

    #[test]
    fn the_kill_switch_and_only_the_kill_switch_disables_the_election() {
        assert!(LeaseConfig::from_setting(None).enabled, "default is ON");
        assert!(LeaseConfig::from_setting(Some("enabled")).enabled);
        assert!(LeaseConfig::from_setting(Some("garbage")).enabled, "unparseable stays ON");
        assert!(!LeaseConfig::from_setting(Some("disabled")).enabled);
        assert!(!LeaseConfig::from_setting(Some("off")).enabled);
        assert!(!LeaseConfig::from_setting(Some(" Disabled ")).enabled);
    }

    // The global-state tests share the process-wide STATE — serialize them.
    static STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn a_process_that_never_campaigns_acts_as_leader() {
        let _g = STATE_TEST_LOCK.lock().unwrap();
        set_state(NO_ELECTION);
        assert!(is_leader(), "unit tests / dev binaries / kill switch must not be demoted");
    }

    #[test]
    fn campaigning_denies_leadership_until_elected() {
        let _g = STATE_TEST_LOCK.lock().unwrap();
        set_state(STANDING_BY);
        assert!(!is_leader());
        set_state(LEADER);
        assert!(is_leader());
        set_state(NO_ELECTION); // restore the default for any later reader
    }
}
