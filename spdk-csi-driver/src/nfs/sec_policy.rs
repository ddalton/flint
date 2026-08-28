//! The minimum security flavor an export will accept (`FLINT_NFS_MIN_SEC`).
//!
//! Offering a strong flavor is not requiring one. Before this module the
//! server advertised `krb5p` through SECINFO and then served whatever the
//! client actually presented — so an export configured for Kerberos was
//! only ever as strong as `sec=sys`, and the choice belonged to the
//! client. Every guarantee the Kerberos work bought back (RFC 3961/3962/
//! 8009 crypto, RFC 2203 framing) was opt-in by the peer it was meant to
//! constrain.
//!
//! The floor closes that. It is one total order over what a call can
//! arrive as, a single comparison, and — importantly — the SAME
//! comparison on both the accept path and the SECINFO advertisement, so
//! the server cannot invite a client into a flavor it will then refuse.
//! `advertisement_matches_enforcement` pins that symmetry; it is the
//! property, not the two code paths, that is the real invariant.
//!
//! Default is [`SecLevel::None`] — accept anything, which is the
//! behaviour that shipped before this module existed. Raising the floor
//! is an operator's deliberate act, and it is logged at startup.

use super::rpc::AuthFlavor;
use super::rpcsec_gss::GssService;
use std::sync::OnceLock;

/// What a call arrived as, ordered weakest to strongest.
///
/// Derived `Ord` follows declaration order, which is the security
/// order — the variants are written in it deliberately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SecLevel {
    /// AUTH_NONE — no claim of identity at all.
    None,
    /// AUTH_SYS — an unverified uid/gid asserted by the client.
    Sys,
    /// RPCSEC_GSS, `rpc_gss_svc_none` — authenticated, not protected.
    Krb5,
    /// RPCSEC_GSS, `rpc_gss_svc_integrity` — every call and reply MIC'd.
    Krb5i,
    /// RPCSEC_GSS, `rpc_gss_svc_privacy` — every call and reply sealed.
    Krb5p,
}

impl SecLevel {
    /// Weakest first. Used by tests to sweep the whole order rather than
    /// spot-check the ends of it.
    pub const ALL: [SecLevel; 5] = [
        SecLevel::None,
        SecLevel::Sys,
        SecLevel::Krb5,
        SecLevel::Krb5i,
        SecLevel::Krb5p,
    ];

    /// The `sec=` spelling a Linux client would use.
    pub fn name(self) -> &'static str {
        match self {
            SecLevel::None => "none",
            SecLevel::Sys => "sys",
            SecLevel::Krb5 => "krb5",
            SecLevel::Krb5i => "krb5i",
            SecLevel::Krb5p => "krb5p",
        }
    }

    /// Parse an operator-supplied name.
    ///
    /// Case- and whitespace-insensitive on purpose: `FLINT_NFS_MIN_SEC`
    /// is a security control, and a value that differs from the intended
    /// one only in case must not silently mean something weaker. The
    /// aliases are the other spellings the RPC and NFS worlds use for
    /// the same two bottom rungs.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "null" | "auth_none" => Some(SecLevel::None),
            "sys" | "unix" | "auth_sys" => Some(SecLevel::Sys),
            "krb5" => Some(SecLevel::Krb5),
            "krb5i" => Some(SecLevel::Krb5i),
            "krb5p" => Some(SecLevel::Krb5p),
            _ => None,
        }
    }

    /// The level a call actually arrived at.
    ///
    /// `service` is the negotiated RPCSEC_GSS service, known only once a
    /// context is established. `None` for a GSS call maps to [`Krb5`] —
    /// the floor of what GSS can mean — so an unestablished context can
    /// never be scored above what it has proven.
    ///
    /// [`Krb5`]: SecLevel::Krb5
    pub fn of_call(flavor: AuthFlavor, service: Option<GssService>) -> Self {
        match flavor {
            AuthFlavor::Null => SecLevel::None,
            AuthFlavor::Unix => SecLevel::Sys,
            AuthFlavor::RpcsecGss => match service {
                Some(GssService::Privacy) => SecLevel::Krb5p,
                Some(GssService::Integrity) => SecLevel::Krb5i,
                Some(GssService::None) | None => SecLevel::Krb5,
            },
        }
    }
}

/// What the floor decided about one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Serve it.
    Allow,
    /// Refuse it with AUTH_TOOWEAK (RFC 5531 `auth_stat`).
    TooWeak { arrived: SecLevel, floor: SecLevel },
}

impl Admission {
    pub fn is_allowed(self) -> bool {
        matches!(self, Admission::Allow)
    }
}

/// The export's accept floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecPolicy {
    floor: SecLevel,
}

impl SecPolicy {
    pub const ENV: &'static str = "FLINT_NFS_MIN_SEC";

    pub fn new(floor: SecLevel) -> Self {
        Self { floor }
    }

    pub fn floor(&self) -> SecLevel {
        self.floor
    }

    /// Whether a call that arrived at `level` may be served.
    pub fn permits(&self, level: SecLevel) -> bool {
        level >= self.floor
    }

    /// Whether SECINFO should offer `level`.
    ///
    /// Identical to [`permits`](Self::permits), and that is the point:
    /// the advertisement is derived from the accept rule rather than
    /// maintained alongside it.
    pub fn advertises(&self, level: SecLevel) -> bool {
        self.permits(level)
    }

    /// The verdict for one call.
    ///
    /// `is_null` marks the RPC NULL procedure, which is exempt at every
    /// floor. NULL carries no arguments and returns no data, and it is
    /// how clients and monitoring probe liveness before they hold any
    /// credential — refusing it breaks the probe while protecting
    /// nothing. It is the only carve-out, and it is here rather than in
    /// the server so that it is a decision with a test on it.
    pub fn admit(&self, arrived: SecLevel, is_null: bool) -> Admission {
        if is_null || self.permits(arrived) {
            Admission::Allow
        } else {
            Admission::TooWeak {
                arrived,
                floor: self.floor,
            }
        }
    }

    /// Read the floor from the environment, rejecting a value that is
    /// not a flavor name.
    ///
    /// The error is deliberately not swallowed: a typo in a security
    /// knob that quietly leaves the floor on the ground is the failure
    /// this whole module exists to prevent. Callers turn it into a
    /// refusal to start.
    pub fn validate_env() -> Result<Self, String> {
        match std::env::var(Self::ENV) {
            Err(_) => Ok(Self::new(SecLevel::None)),
            Ok(raw) => Self::validate_env_value(&raw),
        }
    }

    /// [`validate_env`](Self::validate_env) for one already-read value —
    /// the seam tests use, so they never race over a process-wide var.
    pub fn validate_env_value(raw: &str) -> Result<Self, String> {
        // Unset and set-to-blank mean the same thing: no floor asked for.
        if raw.trim().is_empty() {
            return Ok(Self::new(SecLevel::None));
        }
        match SecLevel::parse(raw) {
            None => Err(unknown_flavor_message(raw)),
            // A krb5p FLOOR is refused, deliberately — see
            // `KRB5P_FLOOR_UNSUPPORTED`. krb5p remains a perfectly good
            // level for a call to ARRIVE at; it is unusable only as an
            // accept floor.
            Some(SecLevel::Krb5p) => Err(KRB5P_FLOOR_UNSUPPORTED.to_string()),
            Some(level) => Ok(Self::new(level)),
        }
    }

    /// The floor for code that cannot return an error.
    ///
    /// An unparseable value fails CLOSED, to the strongest floor. That
    /// will refuse mounts, loudly, which is the correct direction to be
    /// wrong in for a security control — the alternative is an operator
    /// who believes enforcement is on while every `sec=sys` client is
    /// being served. Startup validates first, so in a server that came
    /// up at all this branch is unreachable.
    pub fn from_env() -> Self {
        Self::or_fail_closed(Self::validate_env())
    }

    /// The fallback half of [`from_env`](Self::from_env), split out so
    /// the direction of failure can be asserted without a test setting
    /// a process-wide variable other tests are reading.
    fn or_fail_closed(validated: Result<Self, String>) -> Self {
        validated.unwrap_or_else(|e| {
            tracing::error!("{} — refusing everything below krb5p until it is fixed", e);
            Self::new(SecLevel::Krb5p)
        })
    }
}

/// Why `FLINT_NFS_MIN_SEC=krb5p` is refused at startup.
///
/// Measured against a stock Linux client, 2026-08-27: a `sec=krb5p`
/// mount does its NFSv4 state management — EXCHANGE_ID, CREATE_SESSION,
/// the machine credential — over **krb5i**, and only the filesystem
/// operations over krb5p. Three runs, one variable:
///
/// | floor | mount | result | services seen |
/// |-------|-------|--------|---------------|
/// | krb5p | krb5p | REFUSED | Integrity, None — no Privacy ever sent |
/// | krb5i | krb5p | mounted | Integrity, None, **Privacy** |
/// | none  | krb5p | mounted | Integrity, None, Privacy |
///
/// So an RPC-layer krb5p floor refuses the krb5i state-management calls
/// and the mount dies before one private byte is exchanged. The floor
/// would advertise the strongest posture in the tree and deliver an
/// unmountable export.
///
/// Doing it properly means what knfsd does: security is a property of
/// the EXPORT, not of every RPC, enforced as `NFS4ERR_WRONGSEC` (10016)
/// on the filehandle-establishing operations so the client re-negotiates
/// via SECINFO. That needs per-operation enforcement inside the COMPOUND
/// dispatcher, with SEQUENCE still processed first on 4.1 — a protocol
/// change, not a config change. Until then this refuses rather than
/// pretends.
const KRB5P_FLOOR_UNSUPPORTED: &str = "FLINT_NFS_MIN_SEC=krb5p is not supported as an accept floor: a Linux sec=krb5p mount does its NFSv4 state management over krb5i, so an RPC-level krb5p floor refuses those calls and the mount fails before any private data flows. Use krb5i to require Kerberos (clients may still choose sec=krb5p for data); per-export krb5p enforcement needs NFS4ERR_WRONGSEC in the COMPOUND dispatcher, which is not implemented.";

/// The startup error for an unrecognised `FLINT_NFS_MIN_SEC`.
///
/// Split out so it can be asserted on without touching the process
/// environment, which other tests in this binary are also reading.
fn unknown_flavor_message(raw: &str) -> String {
    format!(
        "{}={:?} is not a security flavor. Expected one of: {}.",
        SecPolicy::ENV,
        raw,
        SecLevel::ALL
            .iter()
            .map(|l| l.name())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

impl Default for SecPolicy {
    fn default() -> Self {
        Self::new(SecLevel::None)
    }
}

static POLICY: OnceLock<SecPolicy> = OnceLock::new();

/// The process-wide floor, read from the environment once.
///
/// Cached because SECINFO consults it per call. Tests exercise
/// [`SecPolicy`] directly rather than through this, so they never race
/// each other over a process-global.
pub fn active() -> SecPolicy {
    *POLICY.get_or_init(SecPolicy::from_env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_order_runs_weakest_to_strongest() {
        // Pins the derived Ord to the security order. Reordering the
        // variants for tidiness would silently invert every comparison
        // in this module.
        let mut sorted = SecLevel::ALL;
        sorted.sort();
        assert_eq!(sorted, SecLevel::ALL);
        assert!(SecLevel::None < SecLevel::Sys);
        assert!(SecLevel::Sys < SecLevel::Krb5);
        assert!(SecLevel::Krb5 < SecLevel::Krb5i);
        assert!(SecLevel::Krb5i < SecLevel::Krb5p);
    }

    #[test]
    fn a_floor_admits_itself_and_everything_above_it() {
        for (i, &floor) in SecLevel::ALL.iter().enumerate() {
            let policy = SecPolicy::new(floor);
            for (j, &arrived) in SecLevel::ALL.iter().enumerate() {
                assert_eq!(
                    policy.permits(arrived),
                    j >= i,
                    "floor {} vs arrival {}",
                    floor.name(),
                    arrived.name()
                );
            }
        }
    }

    #[test]
    fn the_default_floor_admits_everything() {
        // The pre-existing behaviour. If this flips, every deployment
        // that never set the knob starts refusing its own clients.
        let policy = SecPolicy::default();
        assert_eq!(policy.floor(), SecLevel::None);
        for &level in &SecLevel::ALL {
            assert!(policy.permits(level), "{} refused by default", level.name());
        }
    }

    #[test]
    fn a_krb5p_floor_refuses_sys_and_none_and_krb5_and_krb5i() {
        let policy = SecPolicy::new(SecLevel::Krb5p);
        assert!(!policy.permits(SecLevel::None));
        assert!(!policy.permits(SecLevel::Sys));
        assert!(!policy.permits(SecLevel::Krb5));
        assert!(!policy.permits(SecLevel::Krb5i));
        assert!(policy.permits(SecLevel::Krb5p));
    }

    #[test]
    fn advertisement_matches_enforcement() {
        // The invariant the SECINFO comment in `v4::compound` describes:
        // advertising what you will not honour is the same defect as
        // claiming protection you do not apply, pointed the other way.
        for &floor in &SecLevel::ALL {
            let policy = SecPolicy::new(floor);
            for &level in &SecLevel::ALL {
                assert_eq!(
                    policy.advertises(level),
                    policy.permits(level),
                    "floor {} disagrees with itself about {}",
                    floor.name(),
                    level.name()
                );
            }
        }
    }

    #[test]
    fn a_call_is_scored_by_what_it_actually_carried() {
        assert_eq!(SecLevel::of_call(AuthFlavor::Null, None), SecLevel::None);
        assert_eq!(SecLevel::of_call(AuthFlavor::Unix, None), SecLevel::Sys);
        assert_eq!(
            SecLevel::of_call(AuthFlavor::RpcsecGss, Some(GssService::None)),
            SecLevel::Krb5
        );
        assert_eq!(
            SecLevel::of_call(AuthFlavor::RpcsecGss, Some(GssService::Integrity)),
            SecLevel::Krb5i
        );
        assert_eq!(
            SecLevel::of_call(AuthFlavor::RpcsecGss, Some(GssService::Privacy)),
            SecLevel::Krb5p
        );
    }

    #[test]
    fn an_unestablished_gss_context_scores_no_higher_than_krb5() {
        // A GSS call whose service is not yet known must not be credited
        // with the protection it has not negotiated.
        assert_eq!(
            SecLevel::of_call(AuthFlavor::RpcsecGss, None),
            SecLevel::Krb5
        );
        assert!(!SecPolicy::new(SecLevel::Krb5i).permits(SecLevel::of_call(
            AuthFlavor::RpcsecGss,
            None
        )));
    }

    #[test]
    fn a_uid_asserted_by_the_client_does_not_reach_a_kerberos_floor() {
        // AUTH_SYS carries a uid, which reads like identity and is not.
        for floor in [SecLevel::Krb5, SecLevel::Krb5i, SecLevel::Krb5p] {
            assert!(!SecPolicy::new(floor).permits(SecLevel::of_call(AuthFlavor::Unix, None)));
        }
    }

    #[test]
    fn every_flavor_name_round_trips() {
        for &level in &SecLevel::ALL {
            assert_eq!(SecLevel::parse(level.name()), Some(level));
        }
    }

    #[test]
    fn parsing_ignores_case_and_surrounding_space() {
        assert_eq!(SecLevel::parse("  KRB5P "), Some(SecLevel::Krb5p));
        assert_eq!(SecLevel::parse("Krb5I"), Some(SecLevel::Krb5i));
        assert_eq!(SecLevel::parse("SYS"), Some(SecLevel::Sys));
    }

    #[test]
    fn the_alternate_spellings_land_on_the_same_rung() {
        assert_eq!(SecLevel::parse("auth_sys"), Some(SecLevel::Sys));
        assert_eq!(SecLevel::parse("unix"), Some(SecLevel::Sys));
        assert_eq!(SecLevel::parse("auth_none"), Some(SecLevel::None));
        assert_eq!(SecLevel::parse("null"), Some(SecLevel::None));
    }

    #[test]
    fn a_near_miss_is_not_a_flavor() {
        // The whole point of validating: `krb5pp` must not read as
        // "something unrecognised, therefore no floor".
        for bad in ["krb5pp", "krb", "kerberos", "sec=krb5p", "5", "", "krb5x"] {
            assert_eq!(SecLevel::parse(bad), None, "{bad:?} parsed as a flavor");
        }
    }

    #[test]
    fn an_unparseable_floor_fails_closed_not_open() {
        // `from_env` cannot report an error, so the direction it is
        // wrong in matters: strongest, not weakest. Getting this
        // backwards is the exact failure the module exists to prevent —
        // an operator who set the knob, typoed it, and is served plain
        // `sec=sys` while believing otherwise.
        let fallback = SecPolicy::or_fail_closed(Err(unknown_flavor_message("krb5pp")));
        assert_eq!(fallback.floor(), SecLevel::Krb5p);
        assert!(!fallback.permits(SecLevel::Sys));
        assert!(!fallback.permits(SecLevel::None));
        assert!(!fallback.permits(SecLevel::Krb5i));
    }

    #[test]
    fn an_absent_or_blank_setting_is_not_an_error() {
        // Distinct from a typo: unset means "no floor asked for", and
        // must not trip the fail-closed path and strand a deployment
        // that never opted in.
        assert_eq!(
            SecPolicy::or_fail_closed(Ok(SecPolicy::new(SecLevel::None))).floor(),
            SecLevel::None
        );
    }

    #[test]
    fn a_call_below_the_floor_is_refused_and_says_by_how_much() {
        let policy = SecPolicy::new(SecLevel::Krb5p);
        assert_eq!(
            policy.admit(SecLevel::Sys, false),
            Admission::TooWeak {
                arrived: SecLevel::Sys,
                floor: SecLevel::Krb5p
            }
        );
        assert!(!policy.admit(SecLevel::Sys, false).is_allowed());
        assert!(policy.admit(SecLevel::Krb5p, false).is_allowed());
    }

    #[test]
    fn null_is_exempt_at_every_floor() {
        // The one carve-out. If this stops holding, liveness probes and
        // `rpcinfo` break against any hardened export.
        for &floor in &SecLevel::ALL {
            for &arrived in &SecLevel::ALL {
                assert!(
                    SecPolicy::new(floor).admit(arrived, true).is_allowed(),
                    "floor {} refused a NULL that arrived as {}",
                    floor.name(),
                    arrived.name()
                );
            }
        }
    }

    #[test]
    fn the_null_exemption_does_not_leak_into_real_calls() {
        // Guards against "fixing" the exemption by making it
        // unconditional — the mutation that would silently disable the
        // whole floor while keeping every other test in this file green.
        let policy = SecPolicy::new(SecLevel::Krb5i);
        for &arrived in &[SecLevel::None, SecLevel::Sys, SecLevel::Krb5] {
            assert!(
                !policy.admit(arrived, false).is_allowed(),
                "{} admitted as a non-NULL call under a krb5i floor",
                arrived.name()
            );
        }
    }

    #[test]
    fn admit_agrees_with_permits_on_everything_that_is_not_null() {
        for &floor in &SecLevel::ALL {
            let policy = SecPolicy::new(floor);
            for &arrived in &SecLevel::ALL {
                assert_eq!(
                    policy.admit(arrived, false).is_allowed(),
                    policy.permits(arrived),
                    "floor {} disagrees with itself about {}",
                    floor.name(),
                    arrived.name()
                );
            }
        }
    }

    #[test]
    fn a_krb5p_floor_is_refused_with_a_reason() {
        // Measured, not assumed: a krb5p floor makes the export
        // unmountable because Linux runs state management over krb5i.
        // Refusing at startup beats advertising the strongest posture
        // in the tree and serving nobody.
        let err = SecPolicy::validate_env_value("krb5p").unwrap_err();
        assert!(err.contains("krb5p"), "{err}");
        assert!(err.contains("krb5i"), "{err} should point at the usable floor");
        assert!(err.contains("state management"), "{err} should say WHY");
    }

    #[test]
    fn krb5p_is_still_a_valid_level_for_a_call_to_arrive_at() {
        // Only the FLOOR is refused. A krb5p call must still be scored,
        // admitted, and advertised — otherwise refusing the floor would
        // quietly downgrade the service itself.
        assert_eq!(SecLevel::parse("krb5p"), Some(SecLevel::Krb5p));
        assert!(SecPolicy::new(SecLevel::Krb5i).permits(SecLevel::Krb5p));
        assert!(SecPolicy::new(SecLevel::Krb5i).advertises(SecLevel::Krb5p));
    }

    #[test]
    fn every_floor_below_krb5p_is_still_accepted() {
        for name in ["none", "sys", "krb5", "krb5i"] {
            assert!(
                SecPolicy::validate_env_value(name).is_ok(),
                "{name} should be a usable floor"
            );
        }
    }

    #[test]
    fn the_error_names_the_variable_the_value_and_the_alternatives() {
        // An operator reading this in a crash loop should not need the
        // source to fix it.
        let err = unknown_flavor_message("krb5pp");
        assert!(err.contains("FLINT_NFS_MIN_SEC"), "{err}");
        assert!(err.contains("krb5pp"), "{err}");
        for &level in &SecLevel::ALL {
            assert!(err.contains(level.name()), "{err} omits {}", level.name());
        }
    }
}
