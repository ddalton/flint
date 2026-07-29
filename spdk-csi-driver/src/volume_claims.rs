// volume_claims.rs — the per-volume single-operation claim shared by the
// catch-up, cutover, and hot-rejoin planners (Tier-2 design item 4), since
// v1.20.0 leased and arbitrated (contract R2's controller half, F43).
//
// The rev-5 record contract makes concurrent operations on one volume SAFE,
// but safe-and-wasteful still burns real things: a cutover bounce restaging
// mid-window costs a quiesce and an unwind; two orchestrators shallow-copying
// against the same source fight for its bandwidth. This registry generalizes
// the catch-up orchestrator's old in-flight set: at most one long-running
// operation per volume across ALL planners.
//
// "Whoever claims first" was the v1.19 rule, and it is exactly F43: the
// epoch scheduler advances on a writes-independent 30s timer, each new epoch
// drops a converged standby back to lag=1, so catch-up (the maintenance
// loop) re-acquired the claim every tick and permanently out-raced cutover
// (the resolution loop) — the RWX standby parked forever at raid 1/2. Two
// mechanisms replace it:
//
// 1. **Arbitration by class.** Every op is a *resolver* (cutover,
//    hot-rejoin — admitting the standby ENDS the degraded episode) or a
//    *maintainer* (catch-up, volume expansion when it lands — they only
//    keep the status quo converged). A resolver denied by a live maintainer
//    holder posts a RESERVATION; while a live reservation stands, maintainer
//    claims are refused, so the resolver wins the next release instead of
//    losing the re-claim race forever. A class rule, not a hardcoded
//    cutover-beats-catch-up pair, so expansion joins as a maintainer
//    without rewriting this (F43 doc, "Design input").
//    Reservations are bounded both ways: an idle reservation (resolver
//    stopped asking) lapses after `reservation_ttl`; a reservation that IS
//    being refreshed still lapses at `reservation_max` age and enters a
//    short backoff, guaranteeing the maintainer a periodic turn — a
//    persistently failing bounce must not starve catch-up, whose claim also
//    carries the replace dispatch (F40).
// 2. **Wall-clock lease.** A holder past `lease` is seizable: the next
//    claimant takes the volume and bumps the entry generation, so the
//    stale holder's eventual RAII drop releases nothing (F39's invisible
//    wedge, now self-healing). Seizure never aborts the old task — it
//    merely re-opens scheduling; per rev-5 the overlap is safe, only
//    wasteful. The lease is generous by default (4h): catch-up bulk copies
//    are legitimately multi-hour and progress-bounded in-task
//    (FLINT_COPY_STALL_SECS), so most wedges already surface as task errors
//    that release the claim long before the lease matters.
//
// Process-global ON PURPOSE — full R2 would persist claims as episode
// fields on the record; we deliberately do not (2026-07-26 decision).
// The mutual exclusion is inherently scoped to the single controller
// instance (the same assumption CreateVolume placement and the epoch
// scheduler already make); a controller restart RELEASING every claim is
// correct behavior, not a gap; and correctness never rests here — the
// record's CAS generation (chain-gen, R1) is the token destructive calls
// verify. Persisting claims would add a kube CAS write per volume per tick
// and new failure modes for zero F43 benefit. Node-agent flows never see
// this registry; their safety comes from the record, and their local
// serialization is node_volume_locks.rs (R2's node half).
//
// The epoch scheduler does NOT claim: its cuts are the designed input of the
// chase and must keep flowing during multi-hour catch-ups. It only *consults*
// the registry (`holder`) to defer a volume's cut while a hot rejoin holds
// the claim — a scheduler cut landing inside the quiesce window would abort
// it (the window's E_f cut is strict-fresh; EEXIST unwinds).
//
// Kill switch: FLINT_CLAIM_ARBITRATION=disabled restores the v1.19
// first-come behavior (no reservations, no lease) — this sits under every
// controller planner, so operators get a standing off-switch (the
// FLINT_VOLUME_LOCK pattern).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const OP_CATCHUP: &str = "catch-up";
pub const OP_CUTOVER: &str = "cutover";
pub const OP_HOT_REJOIN: &str = "hot-rejoin";
/// Hot-rejoin's marked-dispatch reconcile (resume localization, adopt,
/// scrub). A separate op from [`OP_HOT_REJOIN`] because the CLASS differs:
/// the reconcile performs the same maintenance work the catch-up
/// orchestrator dispatches (hot_rejoin.rs marked-dispatch comment), so
/// classing it resolver would let it preempt catch-up doing identical work
/// — pure churn. Only the Rejoin quiesce-window site resolves anything.
pub const OP_HOT_REJOIN_RECONCILE: &str = "hot-rejoin-reconcile";
/// Volume expansion's future claim (F43 doc, "Design input": size the
/// arbitration for a fourth claimant). Not wired yet — the expansion work
/// registers it when ControllerExpandVolume grows a fan-out path.
pub const OP_EXPAND: &str = "expand";
/// The maintenance drain (docs/maintenance-drain-csi-node-roll.md): the
/// roller's one record round + graceful leg removal before a planned
/// csi-node restart. Resolver class: a roll campaign is operator time —
/// it must not lose the reacquisition race to catch-up's timer renewal
/// (the F43 lasso applies to any admission-shaped op), and it never
/// duplicates maintainer work (the churn that keeps the marked reconcile
/// maintainer-class does not arise). The barrier upstream guarantees the
/// volume is fully redundant at drain time, so there is no live resolver
/// for it to collide with.
pub const OP_MAINT_DRAIN: &str = "maint-drain";

/// True for both hot-rejoin ops — the epoch scheduler defers a volume's cut
/// while EITHER holds: the Rejoin window's E_f cut is strict-fresh (EEXIST
/// unwinds), and the marked reconcile can re-enter window mechanics when
/// resuming a crashed rejoin, so the pre-arbitration deferral behavior
/// (one op string covered both sites) is preserved exactly.
pub fn is_hot_rejoin_op(op: &str) -> bool {
    op == OP_HOT_REJOIN || op == OP_HOT_REJOIN_RECONCILE
}

/// Resolver ops END a degraded episode (admission/reassembly); maintainer
/// ops keep the status quo converged. Resolvers preempt maintainers —
/// admitting the standby leaves catch-up nothing to chase, so the resolver
/// finishing FIRST is also the cheapest global order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimClass {
    Resolver,
    Maintainer,
}

/// Class table — per claim SITE, not per module (hot-rejoin claims twice
/// with different classes). Unknown ops default to Maintainer: a new op
/// that forgets to register here must not silently gain preemption rights.
pub fn class_of(op: &str) -> ClaimClass {
    match op {
        OP_CUTOVER | OP_HOT_REJOIN | OP_MAINT_DRAIN => ClaimClass::Resolver,
        _ => ClaimClass::Maintainer,
    }
}

/// Tunables, injectable for tests; `global()` reads them from env once.
#[derive(Debug, Clone)]
pub struct ClaimConfig {
    /// FLINT_CLAIM_ARBITRATION=disabled → v1.19 first-come semantics.
    pub arbitration: bool,
    /// FLINT_CLAIM_LEASE_SECS (default 14400 = 4h): holder age past which
    /// any claimant may seize. Deliberately generous — catch-up bulk copies
    /// are legitimately multi-hour and carry no wall-clock watchdog BY
    /// DESIGN (catchup.rs orchestrator comment; FLINT_COPY_STALL_SECS
    /// progress-bounding is the wedge detector), so the lease exists only
    /// for the F39 residue: a task alive-but-wedged past every in-task
    /// bound. Renewal is deliberately omitted: a misfired seizure of a
    /// live, progressing holder is wasteful-but-safe (rev-5), and only
    /// happens at all when a contender actually wants the volume.
    pub lease: Duration,
    /// FLINT_CLAIM_RESERVATION_TTL_SECS (default 180): a reservation not
    /// re-asserted within this lapses (the resolver stopped wanting it).
    /// Must exceed the planner tick (60s) or a healthy resolver's own
    /// reservation expires between attempts.
    pub reservation_ttl: Duration,
    /// FLINT_CLAIM_RESERVATION_MAX_SECS (default 900): absolute age cap on
    /// one reservation even while actively refreshed — then `backoff`.
    pub reservation_max: Duration,
    /// FLINT_CLAIM_RESERVATION_BACKOFF_SECS (default 120): after a max-age
    /// lapse, no new reservation for this long — the maintainer's
    /// guaranteed turn (≥ one 60s tick).
    pub backoff: Duration,
}

fn env_secs(var: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(var)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&s| s > 0)
            .unwrap_or(default),
    )
}

impl ClaimConfig {
    pub fn from_env() -> Self {
        ClaimConfig {
            arbitration: !std::env::var("FLINT_CLAIM_ARBITRATION")
                .is_ok_and(|v| v.eq_ignore_ascii_case("disabled")),
            lease: env_secs("FLINT_CLAIM_LEASE_SECS", 14400),
            reservation_ttl: env_secs("FLINT_CLAIM_RESERVATION_TTL_SECS", 180),
            reservation_max: env_secs("FLINT_CLAIM_RESERVATION_MAX_SECS", 900),
            backoff: env_secs("FLINT_CLAIM_RESERVATION_BACKOFF_SECS", 120),
        }
    }
}

struct Holder {
    op: &'static str,
    since: Instant,
    generation: u64,
}

struct Reservation {
    op: &'static str,
    placed: Instant,
    refreshed: Instant,
}

#[derive(Default)]
struct Entry {
    holder: Option<Holder>,
    reservation: Option<Reservation>,
    /// No new reservation before this instant (max-age anti-starvation).
    reserve_backoff_until: Option<Instant>,
}

pub struct VolumeClaims {
    inner: Mutex<HashMap<String, Entry>>,
    cfg: ClaimConfig,
    /// Registry-wide monotonic generation source. NEVER per-entry: entries
    /// are removed on release and recreated at default, so a per-entry
    /// counter resets and a seized-from holder's late drop could match a
    /// LATER innocent holder's generation and evict it (review finding,
    /// 2026-07-26, repro-proven). Registry-scoped, a generation is issued
    /// once in the process lifetime.
    next_generation: std::sync::atomic::AtomicU64,
}

impl Default for VolumeClaims {
    fn default() -> Self {
        VolumeClaims::with_config(ClaimConfig::from_env())
    }
}

/// RAII claim on one volume; releases on drop (including task panic/abort
/// unwind — a crashed operation must never wedge its volume). After a
/// seizure the drop is a no-op for the NEW holder (generation mismatch).
pub struct VolumeClaim<'a> {
    claims: &'a VolumeClaims,
    volume_id: String,
    generation: u64,
}

/// Why a `try_claim` was denied — for skip-site logging (F39: starvation
/// must be visible; F43: *yielding* must be visible too).
#[derive(Debug, Clone, PartialEq)]
pub enum Denial {
    /// Another op holds the volume (op, held-for).
    Held { by: &'static str, age: Duration },
    /// No holder, but a resolver reservation stands (op, outstanding-for).
    Reserved { by: &'static str, age: Duration },
}

impl VolumeClaims {
    pub fn new() -> Self {
        VolumeClaims::default()
    }

    pub fn with_config(cfg: ClaimConfig) -> Self {
        VolumeClaims {
            inner: Mutex::new(HashMap::new()),
            cfg,
            // Generation 0 is never issued: an outstanding guard always
            // carries a nonzero generation no future grant repeats.
            next_generation: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Claim `volume_id` for `op`. None = denied this tick (`denial`
    /// explains; skip sites must log via [`log_claim_skip`]).
    ///
    /// Arbitration (F43): a resolver denied by a live maintainer holder
    /// posts/refreshes a reservation; maintainers are refused while a live
    /// reservation stands, so the resolver wins the release instead of
    /// re-losing the claim race every tick. A holder past the lease is
    /// seized outright (generation bump); a live resolver reservation makes
    /// the expired volume resolver-only until it lapses.
    pub fn try_claim<'a>(&'a self, volume_id: &str, op: &'static str) -> Option<VolumeClaim<'a>> {
        let mut inner = self.inner.lock().expect("volume-claims lock poisoned");
        let entry = inner.entry(volume_id.to_string()).or_default();
        let now = Instant::now();
        let class = class_of(op);
        let arbitration = self.cfg.arbitration;

        // Lapse bookkeeping first, so every branch below sees live state.
        if let Some(r) = &entry.reservation {
            let idle = now.duration_since(r.refreshed) > self.cfg.reservation_ttl;
            let aged = now.duration_since(r.placed) > self.cfg.reservation_max;
            if idle || aged {
                if aged {
                    // The maintainer's guaranteed turn (see module header).
                    entry.reserve_backoff_until = Some(now + self.cfg.backoff);
                    tracing::warn!(
                        volume_id,
                        reserved_by = r.op,
                        max_secs = self.cfg.reservation_max.as_secs(),
                        "[CLAIMS] resolver reservation hit its age cap without landing — lapsed \
                         into backoff so maintainers get a turn (is the resolver's op failing?)"
                    );
                }
                entry.reservation = None;
            }
        }

        if let Some(h) = &entry.holder {
            let age = now.duration_since(h.since);
            if arbitration && age > self.cfg.lease {
                // Expired: seizable — but a live resolver reservation keeps
                // the queue's head for the resolver class.
                let reserved_for_resolver = entry.reservation.is_some();
                if class == ClaimClass::Resolver || !reserved_for_resolver {
                    tracing::warn!(
                        volume_id,
                        seized_from = h.op,
                        held_secs = age.as_secs(),
                        by = op,
                        lease_secs = self.cfg.lease.as_secs(),
                        "[CLAIMS] lease expired — claim SEIZED (the old operation, if still \
                         alive, runs on harmlessly per rev-5; its release becomes a no-op)"
                    );
                    let generation = self
                        .next_generation
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    entry.reservation = None;
                    entry.holder = Some(Holder { op, since: now, generation });
                    return Some(VolumeClaim {
                        claims: self,
                        volume_id: volume_id.to_string(),
                        generation,
                    });
                }
                return None;
            }
            // Live holder: a resolver blocked by a maintainer reserves its
            // spot at the head of the release.
            if arbitration
                && class == ClaimClass::Resolver
                && class_of(h.op) == ClaimClass::Maintainer
                && entry.reserve_backoff_until.map(|t| now >= t).unwrap_or(true)
            {
                match &mut entry.reservation {
                    Some(r) => r.refreshed = now,
                    None => {
                        entry.reservation =
                            Some(Reservation { op, placed: now, refreshed: now });
                        tracing::info!(
                            volume_id,
                            wanted_op = op,
                            held_by = h.op,
                            "[CLAIMS] resolver reserved the next claim — maintainer re-claims \
                             will yield (F43 arbitration)"
                        );
                    }
                }
            }
            return None;
        }

        // No holder. Maintainers yield to a standing resolver reservation.
        if arbitration && class == ClaimClass::Maintainer && entry.reservation.is_some() {
            return None;
        }
        entry.reservation = None;
        let generation = self
            .next_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        entry.holder = Some(Holder { op, since: now, generation });
        Some(VolumeClaim {
            claims: self,
            volume_id: volume_id.to_string(),
            generation,
        })
    }

    /// F48 (runah 2.9 run A): a resolver that reserved a volume and then
    /// found its work GONE on the next tick (the precondition evaporated —
    /// e.g. hot-rejoin's standby was demoted 12s after the reservation)
    /// must hand the queue back explicitly. Without this, the idle
    /// reservation starves every maintainer until the TTL lapse — observed
    /// live as ~4 minutes of catch-up AND expansion yielding to a resolver
    /// that never came back. No-op unless `op` is the reserving operation
    /// (never releases someone else's reservation).
    pub fn release_reservation(&self, volume_id: &str, op: &'static str) {
        let mut inner = self.inner.lock().expect("volume-claims lock poisoned");
        let Some(entry) = inner.get_mut(volume_id) else { return };
        if entry.reservation.as_ref().map(|r| r.op) != Some(op) {
            return;
        }
        entry.reservation = None;
        tracing::info!(
            volume_id,
            op,
            "[CLAIMS] resolver released its reservation — no work left for this volume (F48)"
        );
        // Same GC rule as Drop: a holder-less, reservation-less entry with
        // no live backoff has nothing left to remember.
        let now = Instant::now();
        if entry.holder.is_none() && !entry.reserve_backoff_until.map(|t| now < t).unwrap_or(false)
        {
            inner.remove(volume_id);
        }
    }

    /// Which operation currently holds `volume_id` and for how long.
    pub fn holder(&self, volume_id: &str) -> Option<(&'static str, Duration)> {
        self.inner
            .lock()
            .expect("volume-claims lock poisoned")
            .get(volume_id)
            .and_then(|e| e.holder.as_ref())
            .map(|h| (h.op, h.since.elapsed()))
    }

    /// Why the last `try_claim` for this volume would be denied right now —
    /// holder first, standing reservation second. None = free (raced).
    pub fn denial(&self, volume_id: &str) -> Option<Denial> {
        let inner = self.inner.lock().expect("volume-claims lock poisoned");
        let entry = inner.get(volume_id)?;
        if let Some(h) = &entry.holder {
            return Some(Denial::Held { by: h.op, age: h.since.elapsed() });
        }
        entry
            .reservation
            .as_ref()
            .map(|r| Denial::Reserved { by: r.op, age: r.placed.elapsed() })
    }
}

/// One skip-site log line (F39: starvation must be visible; F43: yielding
/// must be too). Info below the starvation threshold, warn above it — a
/// wedged holder surfaces in logs long before a human goes looking.
pub fn log_claim_skip(volume_id: &str, wanted_op: &str, claims: &VolumeClaims) {
    let threshold = std::env::var("FLINT_CLAIM_STARVATION_WARN_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(900);
    match claims.denial(volume_id) {
        Some(Denial::Held { by, age }) if age.as_secs() >= threshold => {
            tracing::warn!(
                volume_id,
                wanted_op,
                held_by = by,
                held_secs = age.as_secs(),
                "[CLAIMS] volume claim held past the starvation threshold — the holder may be wedged (F39 shape)"
            );
        }
        Some(Denial::Held { by, age }) => {
            tracing::info!(
                volume_id,
                wanted_op,
                held_by = by,
                held_secs = age.as_secs(),
                "[CLAIMS] volume claimed by another operation — skipping this tick"
            );
        }
        Some(Denial::Reserved { by, age }) => {
            tracing::info!(
                volume_id,
                wanted_op,
                reserved_by = by,
                reserved_secs = age.as_secs(),
                "[CLAIMS] yielding to a reserved resolver operation (F43 arbitration)"
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
        let mut inner = self.claims.inner.lock().expect("volume-claims lock poisoned");
        let now = Instant::now();
        let ttl = self.claims.cfg.reservation_ttl;
        let max = self.claims.cfg.reservation_max;
        // A released volume may never be claimed again (PV deleted
        // mid-episode) and the lapse bookkeeping in try_claim only runs for
        // volumes some planner still visits — so releases are also the
        // registry's garbage collection: lapse-check this entry's
        // reservation, then sweep every holder-less entry whose arbitration
        // state has expired. O(volumes) under a lock only taken at claim
        // boundaries; volumes number in the hundreds.
        let dead = |e: &Entry| {
            e.holder.is_none()
                && !e
                    .reservation
                    .as_ref()
                    .map(|r| {
                        now.duration_since(r.refreshed) <= ttl
                            && now.duration_since(r.placed) <= max
                    })
                    .unwrap_or(false)
                && !e.reserve_backoff_until.map(|t| now < t).unwrap_or(false)
        };
        if let Some(entry) = inner.get_mut(&self.volume_id) {
            // Post-seizure, the stale guard's generation no longer matches:
            // the release belongs to the NEW holder, not us.
            if entry.holder.as_ref().map(|h| h.generation) == Some(self.generation) {
                entry.holder = None;
            }
        }
        inner.retain(|_, e| !dead(e));
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

    fn cfg_ms(lease: u64, ttl: u64, max: u64, backoff: u64) -> ClaimConfig {
        ClaimConfig {
            arbitration: true,
            lease: Duration::from_millis(lease),
            reservation_ttl: Duration::from_millis(ttl),
            reservation_max: Duration::from_millis(max),
            backoff: Duration::from_millis(backoff),
        }
    }

    /// Generous windows: nothing expires unless a test sleeps on purpose.
    fn wide() -> VolumeClaims {
        VolumeClaims::with_config(cfg_ms(60_000, 60_000, 120_000, 10_000))
    }

    #[test]
    fn claim_is_exclusive_per_volume_and_released_on_drop() {
        let claims = wide();
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
        let claims = wide();
        assert!(claims.holder("vol1").is_none());
    }

    #[test]
    fn class_table() {
        assert_eq!(class_of(OP_CUTOVER), ClaimClass::Resolver);
        assert_eq!(class_of(OP_HOT_REJOIN), ClaimClass::Resolver);
        assert_eq!(class_of(OP_CATCHUP), ClaimClass::Maintainer);
        assert_eq!(class_of(OP_EXPAND), ClaimClass::Maintainer);
        // Per-SITE classing: the marked-dispatch reconcile does catch-up's
        // work and must not preempt it.
        assert_eq!(class_of(OP_HOT_REJOIN_RECONCILE), ClaimClass::Maintainer);
        // The maintenance drain is operator time — it must not lose the
        // reacquisition race to catch-up's timer (the F43 lasso shape).
        assert_eq!(class_of(OP_MAINT_DRAIN), ClaimClass::Resolver);
        // Unregistered ops never gain preemption rights.
        assert_eq!(class_of("mystery-op"), ClaimClass::Maintainer);
        // The epoch scheduler's cut deferral covers BOTH hot-rejoin sites.
        assert!(is_hot_rejoin_op(OP_HOT_REJOIN));
        assert!(is_hot_rejoin_op(OP_HOT_REJOIN_RECONCILE));
        assert!(!is_hot_rejoin_op(OP_CUTOVER));
    }

    /// THE F43 regression: catch-up re-claims every tick (the epoch
    /// scheduler's 30s timer resets standby lag, so it always has work);
    /// under first-come rules cutover loses the race forever. With
    /// arbitration, cutover must win within two ticks.
    #[test]
    fn resolver_wins_within_two_ticks_of_a_reclaiming_maintainer() {
        let claims = wide();

        // Tick 1: catch-up holds; cutover is denied but reserves.
        let catchup = claims.try_claim("vol", OP_CATCHUP).expect("tick 1");
        assert!(claims.try_claim("vol", OP_CUTOVER).is_none());
        drop(catchup); // catch-up cycle ends

        // Tick 2: catch-up loses the re-claim (reservation), cutover wins —
        // ORDER is the F43 race: the maintainer asks first and still loses.
        assert!(
            claims.try_claim("vol", OP_CATCHUP).is_none(),
            "maintainer must yield to the standing resolver reservation"
        );
        let cutover = claims.try_claim("vol", OP_CUTOVER).expect("tick 2: resolver wins");
        assert_eq!(claims.holder("vol").map(|(op, _)| op), Some(OP_CUTOVER));

        // Resolution done: catch-up resumes normally.
        drop(cutover);
        assert!(claims.try_claim("vol", OP_CATCHUP).is_some());
    }

    #[test]
    fn reservation_denial_is_visible() {
        let claims = wide();
        let held = claims.try_claim("vol", OP_CATCHUP).expect("hold");
        assert!(claims.try_claim("vol", OP_CUTOVER).is_none());
        match claims.denial("vol") {
            Some(Denial::Held { by, .. }) => assert_eq!(by, OP_CATCHUP),
            other => panic!("expected Held, got {:?}", other),
        }
        drop(held);
        match claims.denial("vol") {
            Some(Denial::Reserved { by, .. }) => assert_eq!(by, OP_CUTOVER),
            other => panic!("expected Reserved, got {:?}", other),
        }
    }

    #[test]
    fn idle_reservation_lapses_after_ttl() {
        let claims = VolumeClaims::with_config(cfg_ms(60_000, 30, 120_000, 10));
        let held = claims.try_claim("vol", OP_CATCHUP).expect("hold");
        assert!(claims.try_claim("vol", OP_CUTOVER).is_none()); // reserves
        drop(held);
        std::thread::sleep(Duration::from_millis(60)); // > ttl, resolver never returns
        assert!(
            claims.try_claim("vol", OP_CATCHUP).is_some(),
            "idle reservation must lapse — the resolver stopped wanting it"
        );
    }

    /// A resolver whose op keeps failing must not starve the maintainer
    /// (whose claim also carries the F40 replace dispatch): at max age the
    /// reservation lapses into backoff and the maintainer gets a turn.
    #[test]
    fn aged_reservation_backs_off_then_alternates() {
        let claims = VolumeClaims::with_config(cfg_ms(60_000, 60_000, 50, 40));
        let held = claims.try_claim("vol", OP_CATCHUP).expect("hold");
        assert!(claims.try_claim("vol", OP_CUTOVER).is_none()); // reservation placed
        std::thread::sleep(Duration::from_millis(70)); // > reservation_max
        // Refresh attempt inside backoff must NOT re-place the reservation.
        assert!(claims.try_claim("vol", OP_CUTOVER).is_none());
        drop(held);
        // Maintainer's guaranteed turn.
        let again = claims
            .try_claim("vol", OP_CATCHUP)
            .expect("maintainer turn after reservation age cap");
        // Backoff over: the resolver may reserve again and wins the release.
        std::thread::sleep(Duration::from_millis(50));
        assert!(claims.try_claim("vol", OP_CUTOVER).is_none()); // reserves anew
        drop(again);
        assert!(claims.try_claim("vol", OP_CATCHUP).is_none(), "yields again");
        assert!(claims.try_claim("vol", OP_CUTOVER).is_some(), "resolver lands");
    }

    #[test]
    fn expired_lease_is_seized_and_stale_drop_is_inert() {
        let claims = VolumeClaims::with_config(cfg_ms(30, 60_000, 120_000, 10));
        let stale = claims.try_claim("vol", OP_CATCHUP).expect("hold");
        std::thread::sleep(Duration::from_millis(60)); // > lease
        let seized = claims.try_claim("vol", OP_CUTOVER).expect("seize past lease");
        assert_eq!(claims.holder("vol").map(|(op, _)| op), Some(OP_CUTOVER));
        // The seized task eventually finishes: its release must not evict
        // the new holder.
        drop(stale);
        assert_eq!(
            claims.holder("vol").map(|(op, _)| op),
            Some(OP_CUTOVER),
            "stale guard's drop must be a no-op after seizure"
        );
        drop(seized);
        assert!(claims.holder("vol").is_none());
    }

    /// Review finding 2026-07-26 (repro-proven pre-fix): with a PER-ENTRY
    /// generation counter, releasing the seizing holder removed the entry;
    /// the next claim recreated it at generation 1 — colliding with the
    /// still-outstanding seized-from guard, whose late drop then evicted
    /// the innocent new holder (voiding mutual exclusion and the epoch
    /// scheduler's hot-rejoin cut deferral). Generations are now
    /// registry-wide monotonic.
    #[test]
    fn stale_guard_from_before_a_seizure_cannot_evict_a_later_holder() {
        let claims = VolumeClaims::with_config(cfg_ms(30, 60_000, 120_000, 10));
        let wedged = claims.try_claim("vol", OP_CATCHUP).expect("hold then wedge");
        std::thread::sleep(Duration::from_millis(60)); // > lease
        let seizer = claims.try_claim("vol", OP_CUTOVER).expect("seize");
        drop(seizer); // seizing holder finishes; entry may be removed
        let innocent = claims.try_claim("vol", OP_HOT_REJOIN).expect("fresh claim");
        // The wedged task finally errors out — its drop must be inert.
        drop(wedged);
        assert_eq!(
            claims.holder("vol").map(|(op, _)| op),
            Some(OP_HOT_REJOIN),
            "a seized-from guard's late drop must never evict a later holder"
        );
        drop(innocent);
        assert!(claims.holder("vol").is_none());
    }

    /// Review finding 2026-07-26: releases also garbage-collect — an entry
    /// whose reservation lapsed must not outlive its volume just because
    /// no planner ever claims it again (PV deleted mid-episode).
    #[test]
    fn releases_sweep_dead_entries_of_other_volumes() {
        let claims = VolumeClaims::with_config(cfg_ms(60_000, 30, 50, 10));
        // vol-gone: maintainer holds, resolver reserves, maintainer
        // releases while the reservation is LIVE (entry kept), and the
        // volume is never claimed again.
        let held = claims.try_claim("vol-gone", OP_CATCHUP).expect("hold");
        assert!(claims.try_claim("vol-gone", OP_CUTOVER).is_none()); // reserves
        drop(held);
        assert!(matches!(claims.denial("vol-gone"), Some(Denial::Reserved { .. })));
        std::thread::sleep(Duration::from_millis(70)); // reservation lapses (ttl+max)
        // Any OTHER volume's release sweeps the dead entry.
        let other = claims.try_claim("vol-live", OP_CATCHUP).expect("other");
        drop(other);
        assert!(claims.denial("vol-gone").is_none(), "dead entry must be swept");
    }

    #[test]
    fn maintainer_cannot_seize_past_a_live_resolver_reservation() {
        let claims = VolumeClaims::with_config(cfg_ms(30, 60_000, 120_000, 10));
        let _held = claims.try_claim("vol", OP_CATCHUP).expect("maintainer holds");
        assert!(claims.try_claim("vol", OP_CUTOVER).is_none()); // reserves
        std::thread::sleep(Duration::from_millis(60)); // maintainer's lease expires
        // The expired volume is resolver-only while the reservation stands:
        assert!(
            claims.try_claim("vol", OP_EXPAND).is_none(),
            "a second maintainer must not seize past the resolver's reservation"
        );
        let resolver = claims.try_claim("vol", OP_CUTOVER);
        assert!(resolver.is_some(), "the reserved class seizes the expired holder");
        assert_eq!(claims.holder("vol").map(|(op, _)| op), Some(OP_CUTOVER));
    }

    #[test]
    fn resolvers_do_not_reserve_against_resolvers() {
        let claims = wide();
        let held = claims.try_claim("vol", OP_HOT_REJOIN).expect("resolver holds");
        assert!(claims.try_claim("vol", OP_CUTOVER).is_none());
        drop(held);
        // No reservation was placed — first-come between resolvers.
        assert!(claims.denial("vol").is_none());
        assert!(claims.try_claim("vol", OP_CATCHUP).is_some());
    }

    #[test]
    fn kill_switch_restores_first_come() {
        let mut cfg = cfg_ms(30, 30, 50, 10);
        cfg.arbitration = false;
        let claims = VolumeClaims::with_config(cfg);
        let held = claims.try_claim("vol", OP_CATCHUP).expect("hold");
        assert!(claims.try_claim("vol", OP_CUTOVER).is_none());
        std::thread::sleep(Duration::from_millis(60)); // past lease
        assert!(
            claims.try_claim("vol", OP_CUTOVER).is_none(),
            "no lease seizure with arbitration disabled"
        );
        drop(held);
        // No reservation either: the maintainer's re-claim wins first-come.
        assert!(claims.try_claim("vol", OP_CATCHUP).is_some());
    }

    /// F48 (runah 2.9 run A): hot-rejoin reserved, its standby was demoted
    /// 12s later, and the idle reservation starved catch-up AND expansion
    /// for the full TTL (~4 min observed). A resolver with no work must
    /// hand the queue back immediately.
    #[test]
    fn released_reservation_frees_maintainers_immediately() {
        let claims = wide();
        let held = claims.try_claim("vol", OP_CATCHUP).expect("hold");
        assert!(claims.try_claim("vol", OP_HOT_REJOIN).is_none()); // reserves
        drop(held);
        assert!(matches!(claims.denial("vol"), Some(Denial::Reserved { .. })));

        // The resolver's next tick finds no work → releases.
        claims.release_reservation("vol", OP_HOT_REJOIN);
        assert!(
            claims.try_claim("vol", OP_CATCHUP).is_some(),
            "maintainer must claim immediately after the release — no TTL wait"
        );
    }

    /// The release never touches someone else's reservation, and releasing
    /// a volume with no entry is a no-op.
    #[test]
    fn release_reservation_is_owner_scoped() {
        let claims = wide();
        claims.release_reservation("never-claimed", OP_HOT_REJOIN); // no-op

        let held = claims.try_claim("vol", OP_CATCHUP).expect("hold");
        assert!(claims.try_claim("vol", OP_CUTOVER).is_none()); // cutover reserves
        drop(held);
        // A different resolver must not clear cutover's reservation.
        claims.release_reservation("vol", OP_HOT_REJOIN);
        assert!(
            claims.try_claim("vol", OP_CATCHUP).is_none(),
            "cutover's reservation must survive a foreign release"
        );
        claims.release_reservation("vol", OP_CUTOVER);
        assert!(claims.try_claim("vol", OP_CATCHUP).is_some());
    }

    /// Releasing while a HOLDER exists clears only the reservation — the
    /// holder (and its entry) stay.
    #[test]
    fn release_reservation_leaves_a_live_holder_alone() {
        let claims = wide();
        let held = claims.try_claim("vol", OP_CATCHUP).expect("hold");
        assert!(claims.try_claim("vol", OP_HOT_REJOIN).is_none()); // reserves
        claims.release_reservation("vol", OP_HOT_REJOIN);
        assert_eq!(claims.holder("vol").map(|(op, _)| op), Some(OP_CATCHUP));
        drop(held);
        // With the reservation gone, first-come applies again.
        assert!(claims.try_claim("vol", OP_CATCHUP).is_some());
    }

    #[test]
    fn concurrent_claimants_exactly_one_winner_per_round() {
        let claims = std::sync::Arc::new(wide());
        for _ in 0..50 {
            let mut handles = Vec::new();
            let won = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            for op in [OP_CATCHUP, OP_CATCHUP, OP_EXPAND, OP_EXPAND] {
                let claims = claims.clone();
                let won = won.clone();
                handles.push(std::thread::spawn(move || {
                    if let Some(c) = claims.try_claim("vol-conc", op) {
                        won.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        drop(c);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            // Winners release before the round ends, so between rounds the
            // volume is free; within a round at most one thread can hold at
            // any instant — but sequential wins are legal (drop → next
            // claim). The invariant: at least one winner, never a wedge.
            assert!(won.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        }
        assert!(claims.holder("vol-conc").is_none());
    }
}
