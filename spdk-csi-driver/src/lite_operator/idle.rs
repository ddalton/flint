//! The idle ladder's memory.
//!
//! ## Why this needs a durable carrier at all
//!
//! The reconciler is level-triggered and server-side-applies everything
//! it renders. `render` computes `replicas` from `spec.lifecycle`, and
//! `claim_plan` re-applies a missing PVC. So a suspend written only to
//! `status`, or held only in the controller's memory, is undone by the
//! very next reconcile — within seconds, and again forever after. An
//! operator restart forgets it entirely.
//!
//! The state therefore has to live somewhere the reconciler reads
//! BEFORE it renders, and that survives a restart. That leaves the CR
//! itself. The one place it must not go is `spec`: the user owns spec,
//! the operator does not write it, and breaking that rule turns every
//! `kubectl apply` of a stored manifest into an accidental wake — or an
//! accidental re-suspend. So the carrier is ANNOTATIONS, which are
//! metadata, and the rule "the operator never writes spec" holds.
//!
//! ## The three annotations
//!
//! - `flint.io/idle-state` — operator-written: what the ladder did.
//! - `flint.io/idle-since` — operator-written: when, RFC3339.
//! - `flint.io/requested-at` — FRONT-DOOR-written: someone wants this
//!   share awake. The operator reads it and never writes it. This is the
//!   whole wake protocol: touch an annotation, and the level-triggered
//!   reconcile does the rest.
//!
//! ## Precedence, which is the part that bites
//!
//! `spec.lifecycle: Suspended` is an ADMIN decision and always wins. A
//! wake request does not override it, and the phase reported for it
//! (`Suspended`) is deliberately different from the ladder's
//! (`IdleSuspended`), so a front door can tell "will wake on request"
//! from "someone said no" instead of retrying forever against a share
//! that is never coming back.

use crate::lite_operator::crd::{FlintShare, IdleSpec, Lifecycle};
use kube::ResourceExt;

/// What the ladder has done to this share.
pub const ANN_IDLE_STATE: &str = "flint.io/idle-state";
/// When it did it (RFC3339), for observability and for the hibernate
/// rung's own timer.
pub const ANN_IDLE_SINCE: &str = "flint.io/idle-since";
/// The front door's wake request / keepalive (RFC3339). Written by the
/// front door, never by the operator.
pub const ANN_REQUESTED_AT: &str = "flint.io/requested-at";
/// Optional hint consumed once at wake: `warm` asks the hub to bulk-fill
/// after its import instead of hydrating on demand.
pub const ANN_WAKE_INTENT: &str = "flint.io/wake-intent";

/// The ladder's durable position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleState {
    /// Running normally.
    Active,
    /// Scaled to zero by the ladder. PVC kept.
    Suspended,
    /// Scaled to zero and the PVC deleted. The bucket is the only copy.
    Hibernated,
    /// Scaled to 1 and waiting for a clean drain before the PVC may be
    /// deleted. A transient state that must be durable anyway: an
    /// operator restart mid-verification has to know it was verifying,
    /// or it would either delete unverified or wake the share for good.
    HibernateVerifying,
}

impl IdleState {
    pub fn as_str(self) -> &'static str {
        match self {
            IdleState::Active => "Active",
            IdleState::Suspended => "Suspended",
            IdleState::Hibernated => "Hibernated",
            IdleState::HibernateVerifying => "HibernateVerifying",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Active" => IdleState::Active,
            "Suspended" => IdleState::Suspended,
            "Hibernated" => IdleState::Hibernated,
            "HibernateVerifying" => IdleState::HibernateVerifying,
            _ => return None,
        })
    }

    /// Does this state mean the hub should be scaled to zero?
    pub fn is_down(self) -> bool {
        matches!(self, IdleState::Suspended | IdleState::Hibernated)
    }
}

fn annotation<'a>(share: &'a FlintShare, key: &str) -> Option<&'a str> {
    share.annotations().get(key).map(|s| s.as_str())
}

/// The ladder's recorded position, `Active` when unset or unreadable.
///
/// An unrecognised value reads as `Active`: the safe direction is
/// "serve", because a share that should have been down merely costs
/// money, while a share that should have been up and is not strands
/// whoever is waiting on it.
pub fn state_of(share: &FlintShare) -> IdleState {
    annotation(share, ANN_IDLE_STATE)
        .and_then(IdleState::parse)
        .unwrap_or(IdleState::Active)
}

pub fn since(share: &FlintShare) -> Option<chrono::DateTime<chrono::Utc>> {
    parse_time(annotation(share, ANN_IDLE_SINCE))
}

pub fn requested_at(share: &FlintShare) -> Option<chrono::DateTime<chrono::Utc>> {
    parse_time(annotation(share, ANN_REQUESTED_AT))
}

pub fn wake_intent(share: &FlintShare) -> Option<&str> {
    annotation(share, ANN_WAKE_INTENT)
}

/// Whether this wake should pull the working set back during import.
///
/// `Some(true)` = `warm`, `Some(false)` = `cold`, `None` = no intent
/// expressed, so the share's own `hydrateWarmAfterImport` stands.
///
/// This is the one thing the front door knows and the operator cannot:
/// whether a person is about to open the project, or something merely
/// touched it. An unrecognised value reads as no intent rather than as
/// `cold` — guessing "do less" on a typo would show up as a slow
/// project and nothing else.
pub fn wake_warm_fill(share: &FlintShare) -> Option<bool> {
    match wake_intent(share)?.trim() {
        "warm" => Some(true),
        "cold" => Some(false),
        _ => None,
    }
}

fn parse_time(s: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = s?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// Seconds since the front door last said it wanted this share.
///
/// A future timestamp is clamped to 0 — "someone wants this share right
/// now". The front door's clock and the operator's need not agree, and
/// a skewed clock must not be able to make a live share look abandoned.
/// `None` = no request has ever been recorded.
pub fn requested_age_secs(
    share: &FlintShare,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<u64> {
    let at = requested_at(share)?;
    Some((now - at).num_seconds().max(0) as u64)
}

/// How far in the FUTURE the request stamp is, if it is.
///
/// `requested_age_secs` clamps a future stamp to 0 — "wanted right
/// now" — which is the right reading of ordinary clock skew and the
/// wrong reading without a ceiling. A front door running an hour fast
/// stamps an hour ahead, the clamp reports 0s forever, and the share
/// is pinned awake for the length of the skew: indistinguishable from
/// real demand, and invisible. This is the term that makes it
/// distinguishable. `None` = no stamp, or a stamp in the past.
pub fn request_skew_secs(
    share: &FlintShare,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<u64> {
    let at = requested_at(share)?;
    let ahead = (at - now).num_seconds();
    (ahead > 0).then_some(ahead as u64)
}

/// A request stamp too far ahead to be skew, given this share's own
/// threshold. `Some(ahead_secs)` = do not trust it, and say so.
///
/// The bound is one full `suspendAfter`: inside that, a fast clock
/// costs at most one extra window of uptime and the clamp absorbs it;
/// beyond it, the stamp can outlive its own threshold indefinitely,
/// which is not skew any more. Shares with the ladder off cannot be
/// pinned by definition, so there is nothing to judge.
pub fn implausible_request(
    cfg: Option<&IdleSpec>,
    share: &FlintShare,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<u64> {
    let after = cfg.and_then(|c| c.suspend_after_secs)?;
    request_skew_secs(share, now).filter(|ahead| *ahead > after)
}

/// What the reconciler should do about the ladder this pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Serve. Either the ladder is off, or something wants this share.
    Stay,
    /// Nothing wants it and the hub agrees it is idle: scale to zero,
    /// keep the PVC.
    Suspend,
    /// It has been down long enough to reclaim the disk. Scale to ONE
    /// and verify a clean flush before deleting anything — see the
    /// reconciler.
    BeginHibernate,
    /// A wake was requested (or an admin flipped lifecycle back).
    Wake,
    /// Leave it exactly as it is, with a reason worth reporting.
    Hold(String),
}

/// The share's own inputs to the decision — everything except the hub's
/// self-report, which the caller supplies separately because obtaining
/// it costs a network round trip that this function must not make.
pub struct Inputs<'a> {
    pub share: &'a FlintShare,
    pub now: chrono::DateTime<chrono::Utc>,
    /// `Ok` = the hub says it is quiet; `Err(why)` = it is not, or we
    /// could not ask. **A failed poll must arrive here as `Err`.** An
    /// unreachable hub is an unknown hub, never an idle one.
    pub hub_quiet: Result<(), String>,
    /// `Some(true)` = a client still holds a lease.
    pub sessions_live: Option<bool>,
}

/// The suspend/hibernate/wake decision.
///
/// **Suspend requires TWO independent signals to agree**: the front
/// door's heartbeat is stale AND the hub's own activity clock says
/// idle. Each covers the other's blind spot. An agent that computes in
/// memory for twenty minutes without touching the filesystem looks idle
/// to the hub, and the heartbeat is what keeps it alive; a workload
/// that mounted without the front door in the loop has no heartbeat at
/// all, and the hub's own clock is what keeps it alive. It also
/// sidesteps clock comparison: the annotation is judged on the front
/// door's clock and idleness on the hub's, and neither has to agree
/// with the operator's.
pub fn decide(cfg: Option<&IdleSpec>, input: Inputs<'_>) -> Decision {
    let share = input.share;
    let state = state_of(share);
    let lifecycle = share.spec.lifecycle.clone().unwrap_or_default();

    // An admin's Suspended always wins, and a wake request does not
    // override it. Reported as `Suspended`, not `IdleSuspended`, so the
    // front door can tell it will never wake on request.
    if lifecycle == Lifecycle::Suspended {
        return Decision::Stay;
    }

    let wake_requested = input
        .share
        .annotations()
        .contains_key(ANN_REQUESTED_AT);
    let request_age = requested_age_secs(share, input.now);

    // Down, and someone asked for it back.
    if state.is_down() || state == IdleState::HibernateVerifying {
        // A hibernate mid-verification finishes; a wake during it is
        // handled by the reconciler, which cannot simply abandon a
        // half-verified drain.
        if state == IdleState::HibernateVerifying {
            return Decision::Hold("verifying the flush before deleting the PVC".to_string());
        }
        if wake_requested {
            return Decision::Wake;
        }
        // Still down and unwanted. Should it go a rung lower?
        if state == IdleState::Suspended {
            if let Some(after) = cfg.and_then(|c| c.hibernate_after_secs) {
                let down_for = since(share)
                    .map(|t| (input.now - t).num_seconds().max(0) as u64)
                    .unwrap_or(0);
                if down_for >= after {
                    return Decision::BeginHibernate;
                }
                return Decision::Hold(format!(
                    "suspended for {down_for}s; hibernate at {after}s"
                ));
            }
        }
        return Decision::Hold("idle and unrequested".to_string());
    }

    // Running. Should it come down?
    let Some(after) = cfg.and_then(|c| c.suspend_after_secs) else {
        // The ladder is off for this share. Absent is OFF per rung —
        // defaulting it on would auto-suspend every existing share in a
        // fleet, including tier-off ones whose consumers mount
        // `status.address` as a plain PV and have never heard of the
        // wake annotation.
        return Decision::Stay;
    };

    // Signal one: nobody has asked for this share recently. No request
    // ever recorded counts as stale — a share the front door has never
    // brokered is exactly the abandoned case, and requiring a heartbeat
    // that will never come would pin it awake forever.
    //
    // A stamp further ahead than one full threshold is discarded rather
    // than clamped. Clamping reads it as "wanted right now", so a front
    // door with a badly wrong clock would hold the share up for the
    // length of its skew — and because the clamp reports 0s, the reason
    // would read exactly like genuine demand. Discarding it falls
    // through to the hub's own activity clock, which is the signal that
    // does not depend on anyone else's notion of the time.
    let trusted_age = match implausible_request(cfg, share, input.now) {
        Some(_) => None,
        None => request_age,
    };
    if let Some(age) = trusted_age {
        if age < after {
            return Decision::Hold(format!("requested {age}s ago, under the {after}s threshold"));
        }
    }

    // Signal two: the hub's own activity clock.
    if let Err(why) = &input.hub_quiet {
        return Decision::Hold(why.clone());
    }

    // Optional third: live NFS sessions. Off by default, because an
    // idle mount renews its lease forever — "has sessions" would pin
    // every mounted share awake permanently, which is the state this
    // ladder exists to end.
    if cfg.and_then(|c| c.suspend_with_sessions) == Some(false)
        && input.sessions_live == Some(true)
    {
        return Decision::Hold("a client still holds a lease".to_string());
    }

    Decision::Suspend
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lite_operator::crd::{FlintShareSpec, PersistenceSpec};
    use kube::core::ObjectMeta;
    use std::collections::BTreeMap;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-08-19T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn share(anns: &[(&str, &str)]) -> FlintShare {
        let mut a = BTreeMap::new();
        for (k, v) in anns {
            a.insert(k.to_string(), v.to_string());
        }
        FlintShare {
            metadata: ObjectMeta {
                name: Some("s".into()),
                namespace: Some("ns".into()),
                annotations: if a.is_empty() { None } else { Some(a) },
                ..Default::default()
            },
            spec: FlintShareSpec {
                bucket: Some("b".into()),
                key_prefix: None,
                endpoint: None,
                region: None,
                credentials_secret_ref: None,
                import_on_start: None,
                persistence: PersistenceSpec { size: "20Gi".into(), storage_class_name: None },
                service: None,
                image: None,
                log_level: None,
                resources: None,
                node_selector: None,
                settings: None,
                lifecycle: None,
                reclaim: None,
                existing_claim: None,
                restart_policy: None,
                startup_failure_threshold: None,
                termination_grace_period_seconds: None,
                monitoring: None,
                idle: None,
            },
            status: None,
        }
    }

    fn idle(suspend: Option<u64>, hibernate: Option<u64>) -> IdleSpec {
        IdleSpec {
            suspend_after_secs: suspend,
            hibernate_after_secs: hibernate,
            suspend_with_sessions: None,
        }
    }

    /// The third signal is OPT-IN, and it has to actually work when
    /// opted into. It was inert until the reconciler threaded
    /// `nfs.activeLeases` through: the caller passed a hardcoded
    /// `None`, so `suspendWithSessions: false` was a knob the CRD
    /// advertised and nothing honoured.
    ///
    /// The default stays off deliberately. An idle NFSv4 mount renews
    /// its lease forever, so defaulting this on would pin every mounted
    /// share awake permanently — the state the ladder exists to end.
    #[test]
    fn the_sessions_guard_is_opt_in_but_real_once_opted_into() {
        let s = share(&[]);
        let quiet = |cfg: &IdleSpec, sessions| {
            decide(
                Some(cfg),
                Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: sessions },
            )
        };

        // Default: a live lease does NOT hold the suspend.
        assert_eq!(quiet(&idle(Some(60), None), Some(true)), Decision::Suspend);

        // Opted in: it does.
        let guarded = IdleSpec {
            suspend_after_secs: Some(60),
            hibernate_after_secs: None,
            suspend_with_sessions: Some(false),
        };
        assert!(matches!(quiet(&guarded, Some(true)), Decision::Hold(_)));

        // Opted in with nobody mounted: still suspends.
        assert_eq!(quiet(&guarded, Some(false)), Decision::Suspend);

        // Opted in and the hub did not say. UNKNOWN is not "nobody",
        // but it is not a hold either — the two signals that DID answer
        // both say idle, and this rung only ever adds a refusal.
        assert_eq!(quiet(&guarded, None), Decision::Suspend);
    }

    /// **Absent is OFF.** Defaulting the ladder on would auto-suspend
    /// every share in an existing fleet — including tier-off ones whose
    /// consumers mount `status.address` as a plain PV and have never
    /// heard of the wake annotation. Their mounts would hang and nothing
    /// in their world would know to wake anything.
    #[test]
    fn a_share_with_no_idle_policy_is_never_touched() {
        let s = share(&[]);
        let d = decide(
            None,
            Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: None },
        );
        assert_eq!(d, Decision::Stay);

        // And a policy with only the hibernate rung set cannot suspend
        // (CEL refuses that shape at admission too).
        let d = decide(
            Some(&idle(None, Some(3600))),
            Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: None },
        );
        assert_eq!(d, Decision::Stay);
    }

    /// The two signals must AND. Each alone is a known blind spot: an
    /// agent computing in memory looks idle to the hub, and a workload
    /// that mounted without the front door has no heartbeat.
    #[test]
    fn suspending_needs_both_signals_to_agree() {
        let cfg = idle(Some(900), None);

        // Hub idle, but the front door asked for it recently.
        let s = share(&[(ANN_REQUESTED_AT, "2026-08-19T11:59:00Z")]);
        let d = decide(
            Some(&cfg),
            Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: None },
        );
        assert!(matches!(d, Decision::Hold(w) if w.contains("60s ago")), "recent request must hold");

        // Front door quiet, but the hub says someone is working.
        let s = share(&[(ANN_REQUESTED_AT, "2026-08-19T10:00:00Z")]);
        let d = decide(
            Some(&cfg),
            Inputs {
                share: &s,
                now: now(),
                hub_quiet: Err("last client activity 3s ago".into()),
                sessions_live: None,
            },
        );
        assert!(matches!(d, Decision::Hold(w) if w.contains("3s ago")));

        // Both quiet.
        let d = decide(
            Some(&cfg),
            Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: None },
        );
        assert_eq!(d, Decision::Suspend);
    }

    /// **An unreachable hub is not an idle hub.** The poll failure
    /// arrives as `Err` and must hold, not suspend — otherwise a
    /// network blip scales down a fleet.
    #[test]
    fn an_unreachable_hub_is_never_suspended() {
        let s = share(&[(ANN_REQUESTED_AT, "2026-08-19T00:00:00Z")]);
        let d = decide(
            Some(&idle(Some(60), None)),
            Inputs {
                share: &s,
                now: now(),
                hub_quiet: Err("GET http://10.1.2.3:8080/status: connection refused".into()),
                sessions_live: None,
            },
        );
        assert!(matches!(d, Decision::Hold(w) if w.contains("connection refused")));
    }

    /// A share the front door has never brokered is the abandoned case.
    /// Requiring a heartbeat that will never arrive would pin it awake
    /// forever, which defeats the ladder for exactly the shares that
    /// most need it.
    #[test]
    fn a_share_with_no_recorded_request_can_still_suspend() {
        let s = share(&[]);
        let d = decide(
            Some(&idle(Some(900), None)),
            Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: None },
        );
        assert_eq!(d, Decision::Suspend);
    }

    /// A front door whose clock runs fast must not be able to make a
    /// live share look abandoned. A future timestamp means "wanted
    /// now".
    #[test]
    fn a_future_request_timestamp_clamps_to_now() {
        // 60s ahead, judged against a 900s threshold: ordinary skew.
        let s = share(&[(ANN_REQUESTED_AT, "2026-08-19T12:01:00Z")]);
        assert_eq!(requested_age_secs(&s, now()), Some(0));
        let cfg = idle(Some(900), None);
        assert_eq!(implausible_request(Some(&cfg), &s, now()), None);
        let d = decide(
            Some(&cfg),
            Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: None },
        );
        assert!(matches!(d, Decision::Hold(_)), "modest skew still reads as demand");
    }

    /// The clamp without a ceiling is a way to pin a share awake
    /// forever. A front door an hour fast stamps an hour ahead; the
    /// clamp reports 0s every pass, so the share never suspends and
    /// the reason reads exactly like real demand. Past one full
    /// threshold the stamp is discarded and the hub's own activity
    /// clock — which depends on nobody else's notion of the time —
    /// decides.
    #[test]
    fn a_request_from_the_far_future_is_discarded_rather_than_clamped() {
        let s = share(&[(ANN_REQUESTED_AT, "2026-08-19T13:00:00Z")]);
        let cfg = idle(Some(60), None);

        // Still clamps — the accessor's contract is unchanged, and
        // that is precisely why it cannot be the thing that judges.
        assert_eq!(requested_age_secs(&s, now()), Some(0));
        assert_eq!(request_skew_secs(&s, now()), Some(3600));
        assert_eq!(implausible_request(Some(&cfg), &s, now()), Some(3600));

        assert_eq!(
            decide(
                Some(&cfg),
                Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: None },
            ),
            Decision::Suspend,
            "a nonsense stamp must not outvote the hub saying it is quiet"
        );

        // It is only the REQUEST signal that is discarded. A hub that
        // says it is busy still holds the share up.
        assert!(matches!(
            decide(
                Some(&cfg),
                Inputs {
                    share: &s,
                    now: now(),
                    hub_quiet: Err("wrote 4 MiB 3s ago".into()),
                    sessions_live: None,
                },
            ),
            Decision::Hold(_)
        ));

        // A past stamp has no skew, and the ladder being off means
        // there is no threshold to judge against.
        let past = share(&[(ANN_REQUESTED_AT, "2026-08-19T11:00:00Z")]);
        assert_eq!(request_skew_secs(&past, now()), None);
        assert_eq!(implausible_request(Some(&idle(None, None)), &s, now()), None);
    }

    /// A wake request is presence-only, so a skewed clock can still
    /// wake a share — which is the safe direction. Discarding the
    /// stamp must never mean refusing to come back up.
    #[test]
    fn a_skewed_stamp_still_wakes_a_suspended_share() {
        let s = share(&[
            (ANN_IDLE_STATE, "Suspended"),
            (ANN_REQUESTED_AT, "2026-08-19T13:00:00Z"),
        ]);
        assert_eq!(
            decide(
                Some(&idle(Some(60), None)),
                Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: None },
            ),
            Decision::Wake
        );
    }

    /// An admin's `lifecycle: Suspended` always wins, and a wake
    /// request does not override it.
    #[test]
    fn an_admin_suspend_outranks_a_wake_request() {
        let mut s = share(&[(ANN_REQUESTED_AT, "2026-08-19T11:59:59Z")]);
        s.spec.lifecycle = Some(Lifecycle::Suspended);
        let d = decide(
            Some(&idle(Some(60), None)),
            Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: None },
        );
        assert_eq!(d, Decision::Stay, "the reconciler renders replicas 0 for this share");
    }

    /// The wake protocol: touching the annotation on a down share is
    /// the entire request.
    #[test]
    fn touching_the_request_annotation_wakes_a_suspended_share() {
        let s = share(&[
            (ANN_IDLE_STATE, "Suspended"),
            (ANN_IDLE_SINCE, "2026-08-19T11:00:00Z"),
            (ANN_REQUESTED_AT, "2026-08-19T11:59:59Z"),
        ]);
        assert_eq!(state_of(&s), IdleState::Suspended);
        let d = decide(
            Some(&idle(Some(60), None)),
            Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: None },
        );
        assert_eq!(d, Decision::Wake);
    }

    /// The second rung: down long enough, and with a hibernate policy,
    /// the disk is reclaimed. Without the policy it just stays
    /// suspended — hibernate is opt-in on its own.
    #[test]
    fn a_long_suspended_share_hibernates_only_if_asked_to() {
        let s = share(&[(ANN_IDLE_STATE, "Suspended"), (ANN_IDLE_SINCE, "2026-08-19T00:00:00Z")]);

        let d = decide(
            Some(&idle(Some(900), None)),
            Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: None },
        );
        assert!(matches!(d, Decision::Hold(_)), "no hibernate rung ⇒ stays suspended");

        // 12h down, hibernate at 6h.
        let d = decide(
            Some(&idle(Some(900), Some(6 * 3600))),
            Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: None },
        );
        assert_eq!(d, Decision::BeginHibernate);

        // Not yet.
        let d = decide(
            Some(&idle(Some(900), Some(24 * 3600))),
            Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: None },
        );
        assert!(matches!(d, Decision::Hold(w) if w.contains("hibernate at")));
    }

    /// A hibernate that was mid-verification when the operator
    /// restarted must resume verifying, not silently delete and not
    /// silently wake.
    #[test]
    fn a_half_verified_hibernate_resumes_verifying() {
        let s = share(&[(ANN_IDLE_STATE, "HibernateVerifying")]);
        let d = decide(
            Some(&idle(Some(900), Some(3600))),
            Inputs { share: &s, now: now(), hub_quiet: Ok(()), sessions_live: None },
        );
        assert!(matches!(d, Decision::Hold(w) if w.contains("verifying")));
    }

    /// An unrecognised state annotation — a downgrade, or a typo — must
    /// read as Active. Serving a share that should be down costs money;
    /// keeping one down that should be up strands whoever is waiting.
    #[test]
    fn an_unknown_state_annotation_reads_as_active() {
        let s = share(&[(ANN_IDLE_STATE, "Frobnicated")]);
        assert_eq!(state_of(&s), IdleState::Active);
    }
}
