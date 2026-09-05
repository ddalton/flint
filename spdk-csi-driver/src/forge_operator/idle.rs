//! The idle ladder, with ONE rung (design §5).
//!
//! lite's ladder has three positions — Active, Suspended, Hibernated —
//! because a share's disk survives a suspend and is deleted by a
//! hibernate, and those are genuinely different states with different
//! costs to leave. A repository's cache is an `emptyDir`: scaling to
//! zero already destroys it, and waking is a restore from the bucket
//! either way. So `Hibernated` would name the state `Suspended`
//! already is, and publishing a rung that can never be reached is how
//! a reader ends up trusting a distinction that does not exist.
//!
//! What is NOT simplified is the two-signal rule, because both of its
//! blind spots exist here too. **Suspend requires the door's heartbeat
//! to be stale AND the server's own activity clock to say idle.** An
//! agent that clones once and then computes for twenty minutes looks
//! idle to the server, and the heartbeat is what keeps it up; a
//! workload that was pointed at the Service directly has no heartbeat
//! at all, and the server's own clock is what keeps it up. It also
//! sidesteps clock comparison: the annotation is judged on the door's
//! clock and idleness on the server's, and neither has to agree with
//! the operator's.
//!
//! The clock rules themselves come from `lite_operator::idle::clock`
//! rather than being copied: a future stamp clamps to "wanted right
//! now", and a stamp further ahead than one full threshold is
//! discarded rather than clamped, because a door running an hour fast
//! would otherwise pin a repository awake for an hour and look exactly
//! like demand.

use std::collections::BTreeMap;

use kube::ResourceExt;

use crate::lite_operator::idle::clock;
pub use crate::lite_operator::idle::{ANN_IDLE_SINCE, ANN_IDLE_STATE, ANN_REQUESTED_AT};

use super::crd::{FlintRepo, RepoLifecycle};

/// The ladder's recorded position. Two values, because there are two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleState {
    Active,
    Suspended,
}

impl IdleState {
    pub fn as_str(self) -> &'static str {
        match self {
            IdleState::Active => "Active",
            IdleState::Suspended => "Suspended",
        }
    }

    /// An unrecognised value reads as `Active`, and the asymmetry is
    /// deliberate: a repository that should have been down merely costs
    /// money, while one that should have been up and is not strands
    /// whoever is waiting on it.
    pub fn parse(s: &str) -> Option<IdleState> {
        match s.trim() {
            "Active" => Some(IdleState::Active),
            "Suspended" => Some(IdleState::Suspended),
            _ => None,
        }
    }
}

pub fn state_of(anns: &BTreeMap<String, String>) -> IdleState {
    anns.get(ANN_IDLE_STATE)
        .and_then(|s| IdleState::parse(s))
        .unwrap_or(IdleState::Active)
}

/// What the reconciler should do about the ladder this pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Serve. Either the ladder is off, or something wants this
    /// repository.
    Stay,
    /// Nothing wants it and the server agrees it is quiet: replicas 0.
    Suspend,
    /// Someone asked for it back.
    Wake,
    /// Leave it exactly as it is, with a reason worth reporting.
    Hold(String),
}

/// The repository's own inputs. The server's self-report arrives
/// separately because obtaining it costs a round trip this function
/// must not make.
pub struct Inputs<'a> {
    pub repo: &'a FlintRepo,
    pub now: chrono::DateTime<chrono::Utc>,
    /// `Ok` = the server says it is quiet; `Err(why)` = it is not, or
    /// we could not ask. **A failed poll must arrive here as `Err`.**
    /// An unreachable server is an unknown server, never an idle one —
    /// the single most dangerous mistake this decision could make.
    pub server_quiet: Result<(), String>,
}

pub fn decide(input: Inputs<'_>) -> Decision {
    let repo = input.repo;
    let anns = repo.annotations();
    let state = state_of(anns);
    let lifecycle = repo.spec.lifecycle.unwrap_or_default();

    // An admin's Suspended always wins, and a wake request does not
    // override it. The CR reports `Suspended` rather than
    // `IdleSuspended` so the door can tell it will never wake on
    // request — and refuse rather than hold.
    if lifecycle == RepoLifecycle::Suspended {
        return Decision::Stay;
    }

    let wake_requested = anns.contains_key(ANN_REQUESTED_AT);
    let after = repo.spec.idle.as_ref().and_then(|i| i.suspend_after_secs);

    if state == IdleState::Suspended {
        if wake_requested {
            return Decision::Wake;
        }
        return Decision::Hold("idle and unrequested".to_string());
    }

    // Running. Should it come down?
    let Some(after) = after else {
        // Absent is OFF. Defaulting the ladder on would suspend every
        // existing repository in a fleet, including ones whose clients
        // were pointed at the Service directly and have never heard of
        // the wake annotation.
        return Decision::Stay;
    };

    // A stamp further ahead than one full threshold is not skew any
    // more, and is discarded rather than clamped — otherwise a door
    // with a fast clock pins this repository awake for the length of
    // the skew, indistinguishably from demand and invisibly.
    if let Some(ahead) = clock::implausible_request(Some(after), anns, input.now) {
        return Decision::Hold(format!(
            "the wake stamp is {ahead}s in the future, past this repository's own {after}s \
             threshold — ignoring it as a clock fault rather than reading it as demand"
        ));
    }

    // Signal one: nobody has asked recently. No request ever recorded
    // counts as stale — a repository the door has never brokered is
    // exactly the abandoned case, and requiring a heartbeat that will
    // never come would pin it awake forever.
    let requested_recently =
        clock::requested_age_secs(anns, input.now).map(|age| age < after).unwrap_or(false);
    if requested_recently {
        return Decision::Stay;
    }

    // Signal two: the server's own clock.
    match input.server_quiet {
        Ok(()) => Decision::Suspend,
        Err(why) => Decision::Hold(why),
    }
}

/// Whether the server's `/status` says it is quiet enough to come down.
///
/// Reuses the hub's snapshot type because forge's `/status` is
/// deliberately in that shape — the ladder, the phase vocabulary and
/// this predicate are the same across the two front ends, and one of
/// them is enough.
pub fn server_quiet(
    snap: &crate::lite_operator::hubstatus::HubSnapshot,
    after: u64,
) -> Result<(), String> {
    snap.suspendable(after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge_operator::crd::{FlintRepoSpec, RepoIdle};

    fn repo(after: Option<u64>, anns: &[(&str, &str)]) -> FlintRepo {
        let mut r = FlintRepo::new(
            "proj",
            FlintRepoSpec {
                project_id: "proj".into(),
                bucket: "b".into(),
                key_prefix: "p/".into(),
                endpoint: None,
                credentials_secret_ref: None,
                default_branch: None,
                consumers: None,
                branches: None,
                idle: after.map(|a| RepoIdle { suspend_after_secs: Some(a) }),
                export: None,
                fleet: None,
                log_level: None,
                lifecycle: None,
            },
        );
        r.metadata.namespace = Some("tenant".into());
        r.metadata.annotations = Some(
            anns.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        );
        r
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-09-04T12:00:00Z").unwrap().into()
    }

    fn decide_with(r: &FlintRepo, quiet: Result<(), String>) -> Decision {
        decide(Inputs { repo: r, now: now(), server_quiet: quiet })
    }

    /// Suspend needs BOTH signals. Each covers the other's blind spot:
    /// an agent that clones once and then computes for twenty minutes
    /// looks idle to the server, and a client pointed at the Service
    /// directly has no heartbeat at all.
    #[test]
    fn suspending_needs_both_signals_to_agree() {
        let stale = repo(Some(600), &[(ANN_REQUESTED_AT, "2026-09-04T11:00:00Z")]);
        assert_eq!(decide_with(&stale, Ok(())), Decision::Suspend);

        // The server says busy.
        assert!(matches!(
            decide_with(&stale, Err("last client activity 3s ago".into())),
            Decision::Hold(_)
        ));

        // The door says wanted.
        let fresh = repo(Some(600), &[(ANN_REQUESTED_AT, "2026-09-04T11:59:00Z")]);
        assert_eq!(decide_with(&fresh, Ok(())), Decision::Stay);
    }

    /// The single most dangerous mistake this decision could make. An
    /// unreachable server is unknown, never idle.
    #[test]
    fn an_unreadable_server_is_never_suspended() {
        let stale = repo(Some(600), &[(ANN_REQUESTED_AT, "2026-09-04T11:00:00Z")]);
        match decide_with(&stale, Err("could not read the server's status: timeout".into())) {
            Decision::Hold(why) => assert!(why.contains("timeout"), "{why}"),
            other => panic!("an unknown server must never be suspended: {other:?}"),
        }
    }

    /// A repository the door has never brokered is exactly the
    /// abandoned case. Requiring a heartbeat that will never come would
    /// pin it awake forever.
    #[test]
    fn a_repository_with_no_recorded_request_can_still_suspend() {
        let never = repo(Some(600), &[]);
        assert_eq!(decide_with(&never, Ok(())), Decision::Suspend);
    }

    /// Absent is OFF, per repository. Defaulting the ladder on would
    /// suspend every repository in a fleet, including ones whose
    /// clients were pointed at the Service and have never heard of the
    /// wake annotation.
    #[test]
    fn a_repository_with_no_ladder_is_never_touched() {
        let off = repo(None, &[]);
        assert_eq!(decide_with(&off, Ok(())), Decision::Stay);
    }

    /// A door with a fast clock stamps ahead. Clamping reads that as
    /// "wanted right now" forever, which pins the repository awake
    /// invisibly; past one full threshold it is a clock fault and is
    /// named as one.
    #[test]
    fn a_wake_stamp_from_the_far_future_is_a_clock_fault_and_not_demand() {
        let skewed = repo(Some(60), &[(ANN_REQUESTED_AT, "2026-09-04T13:00:00Z")]);
        match decide_with(&skewed, Ok(())) {
            Decision::Hold(why) => assert!(why.contains("future"), "{why}"),
            other => panic!("expected a hold naming the skew, got {other:?}"),
        }
        // Inside one threshold it is ordinary skew, and the clamp
        // absorbs it: this repository IS wanted.
        let slight = repo(Some(600), &[(ANN_REQUESTED_AT, "2026-09-04T12:00:30Z")]);
        assert_eq!(decide_with(&slight, Ok(())), Decision::Stay);
    }

    /// The door arms the annotation; the ladder wakes on it.
    #[test]
    fn a_suspended_repository_wakes_on_a_request_and_holds_without_one() {
        let asked = repo(
            Some(600),
            &[(ANN_IDLE_STATE, "Suspended"), (ANN_REQUESTED_AT, "2026-09-04T11:59:59Z")],
        );
        assert_eq!(decide_with(&asked, Ok(())), Decision::Wake);

        let quiet = repo(Some(600), &[(ANN_IDLE_STATE, "Suspended")]);
        assert!(matches!(decide_with(&quiet, Ok(())), Decision::Hold(_)));
    }

    /// An admin's decision is not reversed by a request. The CR reports
    /// `Suspended` rather than `IdleSuspended` so the door refuses
    /// instead of holding.
    #[test]
    fn an_admin_suspend_outranks_a_wake_request() {
        let mut r = repo(Some(600), &[(ANN_REQUESTED_AT, "2026-09-04T11:59:59Z")]);
        r.spec.lifecycle = Some(RepoLifecycle::Suspended);
        assert_eq!(decide_with(&r, Ok(())), Decision::Stay);
    }

    /// An unrecognised annotation reads as Active, because a repository
    /// that should have been down costs money and one that should have
    /// been up strands whoever is waiting.
    #[test]
    fn an_unknown_state_annotation_reads_as_active() {
        let anns = BTreeMap::from([(ANN_IDLE_STATE.to_string(), "Hibernated".to_string())]);
        assert_eq!(state_of(&anns), IdleState::Active);
        assert_eq!(state_of(&BTreeMap::new()), IdleState::Active);
    }
}
