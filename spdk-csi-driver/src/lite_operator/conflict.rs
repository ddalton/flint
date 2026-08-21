//! Fleet uniqueness: at most one live share per bucket subtree.
//!
//! # Why this is a controller's job and not the schema's
//!
//! CEL and ValidatingAdmissionPolicy evaluate ONE object; "no other
//! FlintShare owns this prefix" is a statement about the others. So
//! the check lives here, over the controller's cache, and re-runs on
//! every change to any share.
//!
//! # Why it matters more than "two pods wasting money"
//!
//! Two hubs on one prefix do not merely duplicate work. The store-side
//! epoch is a LEASE: while both live, the loser waits (it crash-loops
//! before its listener ever binds, so no client sees it) — but the
//! moment the holder dies for a lease window, the other hub judges it
//! dead and TAKES OVER. It then imports that prefix and serves those
//! bytes at ITS OWN address, under ITS OWN name, to whoever mounts it.
//! On routine pod churn, that is cross-tenant data exposure. The epoch
//! protocol is defense-in-depth against a mistake, not a licence to
//! make one.
//!
//! # The predicate
//!
//! Overlap is SUBTREE overlap, not equality: `tenant-a/` and
//! `tenant-a/sub/` are the same volume's data seen from two depths,
//! and the sweeps and `.flint/` control objects of the outer one span
//! the inner one. Comparison is deliberately conservative — a prefix
//! without a trailing slash is compared raw, so `tenant-a` collides
//! with `tenant-abc` exactly as it would in S3.
//!
//! Known limit, stated rather than hidden: two SPELLINGS of one
//! endpoint (`http://minio:9000` vs an IP, or a differently-cased
//! host) are not recognized as the same store. The epoch protocol
//! still fences those; this layer catches the mistakes people
//! actually make.

use super::crd::{ConflictRelation, ConflictWith, FlintShare};

/// The storage a share owns, normalized for comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareKey {
    pub endpoint: String,
    pub bucket: String,
    pub prefix: String,
}

/// A share as the arbiter sees it: identity, age, and what it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub namespace: String,
    pub name: String,
    pub uid: String,
    /// Creation time in unix seconds. A CR the API server has not
    /// stamped yet sorts LAST (`i64::MAX`) — an unstamped newcomer
    /// must never outrank a share that is already serving.
    pub created: i64,
    /// `None` for a tier-off share: it owns no shared storage, so it
    /// conflicts with nothing and nothing conflicts with it.
    pub key: Option<ShareKey>,
}

impl Candidate {
    pub fn of(share: &FlintShare) -> Self {
        let key = share.spec.bucket.as_deref().filter(|b| !b.is_empty()).map(|b| ShareKey {
            endpoint: share.spec.endpoint_key(),
            bucket: b.to_string(),
            prefix: share.spec.prefix().to_string(),
        });
        Candidate {
            namespace: share.metadata.namespace.clone().unwrap_or_default(),
            name: share.metadata.name.clone().unwrap_or_default(),
            uid: share.metadata.uid.clone().unwrap_or_default(),
            created: share
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|t| t.0.as_second())
                .unwrap_or(i64::MAX),
            key,
        }
    }

    /// `namespace/name` — how a conflict names the other share, since
    /// the fleet spans namespaces.
    pub fn r#ref(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

/// Do these two shares own overlapping storage?
pub fn overlaps(a: &ShareKey, b: &ShareKey) -> bool {
    a.endpoint == b.endpoint
        && a.bucket == b.bucket
        && (a.prefix.starts_with(&b.prefix) || b.prefix.starts_with(&a.prefix))
}

/// Set by the front door on a share it wants cleared away.
///
/// `keyPrefix` is CEL-immutable, so a share that loses arbitration
/// cannot be edited into a legal one — the only fix is delete and
/// recreate. But the CR name is derived from the project id, so
/// `create` returns AlreadyExists against the refused object, and the
/// front-door role deliberately has no `delete` ("project deletion is a
/// decision, and this role is held by a web service handling untrusted
/// input"). Without a path out, one typo'd prefix wedges that project
/// id until a cluster-admin runs kubectl.
///
/// So the front door asks, with a verb it already holds (`patch`), and
/// the operator — which can actually check what the share owns —
/// decides.
pub const ANN_ABANDON: &str = "flint.io/abandon";

/// What to do about an abandon request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbandonVerdict {
    /// Not asked for. The overwhelmingly common case.
    NotRequested,
    /// Asked for and refused; the string is why, for an event.
    Refused(String),
    /// Asked for, and this share owns nothing. Delete the CR.
    Delete,
}

/// Decide an abandon request.
///
/// # Why a claim is the whole gate
///
/// There are two kinds of loser and they are not interchangeable.
///
/// A share that lost on its FIRST reconcile owns nothing at all — the
/// conflict check runs before any child is created, so there is no
/// Deployment, no claim, and no epoch (it never started, so it never
/// claimed one). Deleting it removes a row and nothing else.
///
/// A share DEMOTED LATER — its endpoint converged onto an older
/// share's, the one case `spec.endpoint` is deliberately mutable for —
/// has a PVC holding local data, and has already published into the
/// bucket. Deleting that is a decision about data, which is exactly
/// what the front-door role is documented as not being allowed to make.
///
/// So: refuse if anything exists. A missing claim is a stronger signal
/// than any condition, because conditions are a snapshot of the last
/// reconcile while the claim is the thing that would actually be lost.
pub fn abandon_plan(
    share: &FlintShare,
    rejected: bool,
    claim_exists: bool,
    deployment_exists: bool,
) -> AbandonVerdict {
    let asked = share
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(ANN_ABANDON))
        .is_some_and(|v| v == "true");
    if !asked {
        return AbandonVerdict::NotRequested;
    }
    if !rejected {
        return AbandonVerdict::Refused(
            "only a share refused for a bucket-subtree conflict can be abandoned, and this \
             one is not in conflict — remove the annotation"
                .into(),
        );
    }
    if claim_exists || deployment_exists {
        return AbandonVerdict::Refused(format!(
            "this share has run: PersistentVolumeClaim {} and/or its Deployment still \
             exist, so it may hold the only local copy of data and has already published \
             to the bucket. Deleting it is a decision about data — make it deliberately, \
             not through the {} annotation",
            super::render::names(share).claim,
            ANN_ABANDON
        ));
    }
    AbandonVerdict::Delete
}

/// What the loser is told about the winner, as a field instead of a
/// sentence.
///
/// Two decisions live here, both of which want a test rather than a
/// comment at the call site:
///
/// **Whether a redirect is possible at all.** `overlaps` is symmetric,
/// so losing says nothing about which way the two prefixes nest. A
/// winner ABOVE this share already serves these bytes and a consumer
/// can be pointed at it with a sub-path; a winner BELOW it serves only
/// part of what was asked for, and there is nothing to point at. The
/// caller cannot infer that from the fact of losing.
///
/// **Whether the address may be disclosed.** Same namespace only. See
/// [`ConflictWith`] — a hub's export has no per-client authentication,
/// so an address is a capability, and this field must never tell a
/// reader something they could not already read for themselves.
pub fn redirect(loser: &FlintShare, winner: &FlintShare) -> ConflictWith {
    let mine = loser.spec.prefix();
    let theirs = winner.spec.prefix();
    let same_ns = loser.metadata.namespace == winner.metadata.namespace;

    let relation = if mine == theirs {
        ConflictRelation::Same
    } else if mine.starts_with(theirs) {
        ConflictRelation::Ancestor
    } else {
        ConflictRelation::Descendant
    };

    // Only for a winner ABOVE us, and only when its prefix ends at a
    // path boundary. `overlaps` compares raw strings on purpose (so
    // `tenant-a` collides with `tenant-abc` exactly as it would in S3),
    // which means a prefix that does not end in `/` can "contain"
    // another mid-segment. CEL refuses that shape on the way in, but a
    // sub-path derived from it would be a wrong path rather than an
    // absent one, and this field is meant to be acted on.
    let sub_path = match relation {
        ConflictRelation::Ancestor if theirs.is_empty() || theirs.ends_with('/') => {
            mine.strip_prefix(theirs).map(str::to_string)
        }
        _ => None,
    };

    ConflictWith {
        namespace: winner.metadata.namespace.clone().unwrap_or_default(),
        name: winner.metadata.name.clone().unwrap_or_default(),
        prefix: theirs.to_string(),
        relation,
        sub_path,
        address: if same_ns {
            winner.status.as_ref().and_then(|s| s.address.clone())
        } else {
            None
        },
    }
}

/// What the reconcile should do about this share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Nobody else owns this storage — reconcile normally.
    Admitted,
    /// Another share owns it. The hub is scaled to zero (an
    /// already-running loser must STOP, not merely be skipped: it may
    /// be the takeover risk) and the CR carries a `Conflict`
    /// condition naming the winner.
    Rejected { winner: String, message: String },
}

/// Decide whether `me` may run, given every share in the fleet.
///
/// Greedy over age (oldest first, uid as tiebreak because
/// creationTimestamp has one-second granularity and a `kubectl apply
/// -f dir/` stamps a batch identically): a share is admitted unless it
/// overlaps a share already admitted. Two properties this buys —
///
/// - **Deterministic and cache-only.** Every replica of the decision,
///   on every share's reconcile, sorts the same list and reaches the
///   same answer; no leader, no annotations, no ordering races.
/// - **A loser does not take innocents with it.** If a broad prefix
///   loses to one narrow share, other narrow shares that only ever
///   overlapped the broad one keep running — they are judged against
///   the ADMITTED set, not against everyone who ever conflicted.
///
/// Deleting the winner promotes the survivor on the next reconcile,
/// which is why the caller re-queues the whole overlap set on any
/// change.
pub fn admit(fleet: &[Candidate], me: &Candidate) -> Admission {
    let Some(my_key) = me.key.as_ref() else {
        return Admission::Admitted; // tier off: owns no shared storage
    };

    let mut ordered: Vec<&Candidate> = fleet.iter().filter(|c| c.key.is_some()).collect();
    ordered.sort_by(|a, b| {
        a.created
            .cmp(&b.created)
            .then_with(|| a.uid.cmp(&b.uid))
            .then_with(|| a.r#ref().cmp(&b.r#ref()))
    });

    // Identity by uid when there is one (a delete/recreate of the same
    // name is a DIFFERENT share), by namespace/name before the API
    // server has assigned one.
    let is_me = |c: &Candidate| {
        if me.uid.is_empty() {
            c.r#ref() == me.r#ref()
        } else {
            c.uid == me.uid
        }
    };

    let mut admitted: Vec<&Candidate> = Vec::new();
    for c in ordered {
        let key = c.key.as_ref().expect("filtered");
        match admitted
            .iter()
            .find(|a| overlaps(a.key.as_ref().expect("filtered"), key))
        {
            None => {
                if is_me(c) {
                    return Admission::Admitted;
                }
                admitted.push(c);
            }
            Some(winner) => {
                if is_me(c) {
                    let w = winner.key.as_ref().expect("filtered");
                    return Admission::Rejected {
                        winner: winner.r#ref(),
                        message: format!(
                            "bucket subtree s3://{}/{} (endpoint {}) is already owned by \
                             FlintShare {}, which owns s3://{}/{} and is older; refusing to run a \
                             second hub on it — when one hub dies for a lease window the other \
                             takes the prefix over and serves its data",
                            my_key.bucket,
                            my_key.prefix,
                            if my_key.endpoint.is_empty() { "aws" } else { &my_key.endpoint },
                            winner.r#ref(),
                            w.bucket,
                            w.prefix,
                        ),
                    };
                }
            }
        }
    }

    // `me` was not in the fleet list (a stale cache, or the very first
    // reconcile of a brand-new CR): judge it against what was admitted.
    match admitted
        .iter()
        .find(|a| overlaps(a.key.as_ref().expect("filtered"), my_key))
    {
        None => Admission::Admitted,
        Some(winner) => Admission::Rejected {
            winner: winner.r#ref(),
            message: format!(
                "bucket subtree s3://{}/{} is already owned by FlintShare {}",
                my_key.bucket,
                my_key.prefix,
                winner.r#ref()
            ),
        },
    }
}

/// The same verdicts as [`admit`], computed once for a whole fleet.
///
/// # Why this exists
///
/// [`admit`] is O(rank²) in the caller's age-rank and runs on EVERY
/// reconcile, re-deriving an answer that only changes when the fleet
/// changes. Measured at N=3000: ~13 ms for the median share and ~51 ms
/// for the newest, which is ~0.17 of a core at the steady reconcile
/// rate and 3.5 cores at the (legal) fastest requeue setting — against
/// a chart CPU request of 50m. The whole-fleet sweep is N³/6 ≈ 4.5e9
/// `overlaps` calls.
///
/// # The lemma, and the half of it that is FALSE
///
/// `overlaps` IS the prefix order, and the admitted set is prefix-free
/// by construction (a candidate is only admitted if it overlaps nothing
/// already admitted). So for a prefix `p`:
///
/// - **Ancestor direction — at most one, and it is the immediate lex
///   predecessor.** Two strict prefixes of `p` would be prefixes of each
///   other, so both cannot be admitted. And no admitted `x` can sit
///   strictly between an ancestor `a` and `p`: `a < x < p` forces `x` to
///   start with `a` (differ before `len(a)` and `x` falls outside the
///   interval), so `x` would overlap `a`. A `range(..p).next_back()` is
///   therefore exact.
/// - **Descendant direction — there can be MANY.** Two siblings under
///   `p` are not comparable to each other, so both are admitted, and a
///   later `p` overlaps both. **This is why a "first successor" lookup
///   is wrong**: [`admit`] scans `admitted` in AGE order and returns the
///   OLDEST overlap, while the first successor is the LEXICOGRAPHICALLY
///   smallest. Worked case: admitted `tenant-b/` (older) and
///   `tenant-a/` (newer), candidate `tenant-` — `admit` names
///   `tenant-b/`, first-successor names `tenant-a/`.
///
/// So the descendant side takes the MINIMUM AGE RANK over the whole
/// range `[p, p+)`, not the first key in it. Because `admitted` is
/// built in age order, the age rank IS the index, so "oldest overlap"
/// is "smallest index" and no re-comparison of timestamps is needed.
pub struct AdmitTable {
    /// Admitted shares in AGE ORDER. The index is the age rank.
    admitted: Vec<(String, ShareKey)>,
    /// (endpoint, bucket) → prefix → index into `admitted`.
    groups: std::collections::HashMap<(String, String), std::collections::BTreeMap<String, usize>>,
    /// Verdict per fleet member, keyed the two ways `admit` identifies
    /// a candidate: by uid, and by namespace/name for a candidate the
    /// API server has not stamped yet.
    by_uid: std::collections::HashMap<String, Option<usize>>,
    by_ref: std::collections::HashMap<String, Option<usize>>,
}

impl AdmitTable {
    /// Build the table. One pass in the same age order [`admit`] uses.
    pub fn build(fleet: &[Candidate]) -> Self {
        let mut ordered: Vec<&Candidate> = fleet.iter().filter(|c| c.key.is_some()).collect();
        ordered.sort_by(|a, b| {
            a.created
                .cmp(&b.created)
                .then_with(|| a.uid.cmp(&b.uid))
                .then_with(|| a.r#ref().cmp(&b.r#ref()))
        });

        let mut t = AdmitTable {
            admitted: Vec::new(),
            groups: std::collections::HashMap::new(),
            by_uid: std::collections::HashMap::new(),
            by_ref: std::collections::HashMap::new(),
        };

        for c in ordered {
            let key = c.key.as_ref().expect("filtered");
            let verdict = t.oldest_overlap(key);
            if !c.uid.is_empty() {
                t.by_uid.insert(c.uid.clone(), verdict);
            }
            t.by_ref.insert(c.r#ref(), verdict);
            if verdict.is_none() {
                let idx = t.admitted.len();
                t.admitted.push((c.r#ref(), key.clone()));
                t.groups
                    .entry((key.endpoint.clone(), key.bucket.clone()))
                    .or_default()
                    .insert(key.prefix.clone(), idx);
            }
        }
        t
    }

    /// The age rank of the OLDEST admitted share overlapping `key`, or
    /// `None` if nothing does. See the lemma on [`AdmitTable`] for why
    /// the descendant side is a range minimum and not a first hit.
    fn oldest_overlap(&self, key: &ShareKey) -> Option<usize> {
        let group = self.groups.get(&(key.endpoint.clone(), key.bucket.clone()))?;
        let p = key.prefix.as_str();
        let mut best: Option<usize> = None;

        // Ancestor: exactly one candidate, the immediate predecessor.
        if let Some((a, &idx)) = group.range(..p.to_string()).next_back() {
            if p.starts_with(a.as_str()) {
                best = Some(idx);
            }
        }
        // Descendants (and an exact duplicate): the whole range, minimum
        // rank. Bounded by the number of admitted shares beneath `p`,
        // which is 0 for the overwhelmingly common disjoint case.
        for (_, &idx) in group.range(p.to_string()..).take_while(|(k, _)| k.starts_with(p)) {
            best = Some(best.map_or(idx, |b| b.min(idx)));
        }
        best
    }

    /// The same answer [`admit`] would give, including the named winner
    /// and the byte-identical message.
    pub fn verdict(&self, me: &Candidate) -> Admission {
        let Some(my_key) = me.key.as_ref() else {
            return Admission::Admitted; // tier off: owns no shared storage
        };
        let looked_up = if me.uid.is_empty() {
            self.by_ref.get(&me.r#ref()).copied()
        } else {
            self.by_uid.get(&me.uid).copied()
        };
        match looked_up {
            Some(None) => Admission::Admitted,
            Some(Some(idx)) => {
                let (wref, w) = &self.admitted[idx];
                Admission::Rejected {
                    winner: wref.clone(),
                    message: format!(
                        "bucket subtree s3://{}/{} (endpoint {}) is already owned by \
                         FlintShare {}, which owns s3://{}/{} and is older; refusing to run a \
                         second hub on it — when one hub dies for a lease window the other \
                         takes the prefix over and serves its data",
                        my_key.bucket,
                        my_key.prefix,
                        if my_key.endpoint.is_empty() { "aws" } else { &my_key.endpoint },
                        wref,
                        w.bucket,
                        w.prefix,
                    ),
                }
            }
            // `me` was not in the fleet list (a stale cache, or the very
            // first reconcile of a brand-new CR): judge it against what
            // was admitted, with the shorter message `admit` uses there.
            None => match self.oldest_overlap(my_key) {
                None => Admission::Admitted,
                Some(idx) => {
                    let (wref, _) = &self.admitted[idx];
                    Admission::Rejected {
                        winner: wref.clone(),
                        message: format!(
                            "bucket subtree s3://{}/{} is already owned by FlintShare {}",
                            my_key.bucket, my_key.prefix, wref
                        ),
                    }
                }
            },
        }
    }
}

/// Every share whose decision could change when `me` changes — the
/// re-queue set. Deleting a winner has to wake its losers, or they sit
/// in `Failed` forever while nothing owns the prefix.
pub fn overlap_set<'a>(fleet: &'a [Candidate], me: &Candidate) -> Vec<&'a Candidate> {
    let Some(my_key) = me.key.as_ref() else {
        return Vec::new();
    };
    fleet
        .iter()
        .filter(|c| c.uid != me.uid)
        .filter(|c| c.key.as_ref().is_some_and(|k| overlaps(k, my_key)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The measurement behind the change. `--ignored` because it is a
    /// timing report, not an assertion — but it FAILS if the index is
    /// not at least 100x faster on a full-fleet sweep at the design
    /// target, so it cannot silently stop being the point.
    ///
    ///   cargo test --release --lib conflict -- --ignored --nocapture
    #[test]
    #[ignore]
    fn measure_admit_against_the_index_at_fleet_scale() {
        use std::time::Instant;

        for n in [300usize, 1000, 3000] {
            // The design-target shape: disjoint prefixes, ages shuffled
            // so lex order and age order disagree.
            let fleet: Vec<Candidate> = (0..n)
                .map(|i| cand(&format!("s{i}"), &format!("tenant-{i:05}/"), ((i * 37) % n) as i64))
                .collect();

            let median = &fleet[n / 2];
            let newest = fleet.iter().max_by_key(|c| c.created).unwrap();

            let t0 = Instant::now();
            let _ = admit(&fleet, median);
            let d_med = t0.elapsed();

            let t0 = Instant::now();
            let _ = admit(&fleet, newest);
            let d_new = t0.elapsed();

            // A FULL SWEEP is the honest comparison: the operator does
            // one admit per reconcile per share, so a fleet-wide pass
            // is N of them.
            let t0 = Instant::now();
            for c in &fleet {
                let _ = admit(&fleet, c);
            }
            let d_sweep = t0.elapsed();

            let t0 = Instant::now();
            let table = AdmitTable::build(&fleet);
            let d_build = t0.elapsed();

            let t0 = Instant::now();
            for c in &fleet {
                let _ = table.verdict(c);
            }
            let d_lookups = t0.elapsed();

            let indexed = d_build + d_lookups;
            let speedup = d_sweep.as_secs_f64() / indexed.as_secs_f64();
            println!(
                "N={n:>5}  admit: median {:>9.3?}  newest {:>9.3?}  FULL SWEEP {:>10.3?}\n\
                 {:>12}index: build {:>9.3?}  {n} lookups {:>9.3?}  TOTAL {:>10.3?}  ({speedup:.0}x)",
                d_med, d_new, d_sweep, "", d_build, d_lookups, indexed
            );

            if n == 3000 {
                assert!(
                    speedup > 100.0,
                    "the index is only {speedup:.1}x faster on a full sweep at N=3000 — \
                     the whole point of AdmitTable is that arbitration stops being the \
                     operator's dominant CPU term"
                );
            }
        }
    }

    // ---------------------------------------------------------------
    // AdmitTable: it must answer EXACTLY what `admit` answers, winner
    // and message included. `admit` is O(rank^2) per reconcile and runs
    // on a fleet whose answer only changes when the fleet changes; the
    // table replaces it. Equivalence is the whole safety argument, so
    // it is tested against the real function rather than restated.
    // ---------------------------------------------------------------

    /// Every candidate in the fleet, plus a stranger, must agree.
    fn assert_equivalent(fleet: &[Candidate], extra: &[Candidate]) {
        let t = AdmitTable::build(fleet);
        for c in fleet.iter().chain(extra.iter()) {
            assert_eq!(
                t.verdict(c),
                admit(fleet, c),
                "AdmitTable disagreed with admit() for {} (prefix {:?})",
                c.r#ref(),
                c.key.as_ref().map(|k| &k.prefix),
            );
        }
    }

    /// THE CASE THAT KILLS A "FIRST SUCCESSOR" INDEX.
    ///
    /// Two siblings are not comparable, so both are admitted; a later
    /// broad prefix overlaps BOTH. `admit` names the OLDEST. A BTreeSet
    /// successor lookup names the lexicographically first, which is a
    /// different share whenever the lex-first is not the oldest — and
    /// the winner is published in the rejection message and in
    /// `status`, so naming the wrong one is a user-visible lie.
    // --- abandon_plan() ----------------------------------------------

    fn with_abandon(mut s: FlintShare, v: &str) -> FlintShare {
        s.metadata
            .annotations
            .get_or_insert_with(Default::default)
            .insert(ANN_ABANDON.into(), v.into());
        s
    }

    #[test]
    fn an_unannotated_share_is_never_abandoned() {
        let s = share_at("ws", "mine", "t/", None);
        assert_eq!(abandon_plan(&s, true, false, false), AbandonVerdict::NotRequested);
        // ...and neither is one whose annotation is not the literal "true".
        let s2 = with_abandon(share_at("ws", "mine", "t/", None), "yes");
        assert_eq!(abandon_plan(&s2, true, false, false), AbandonVerdict::NotRequested);
    }

    #[test]
    fn a_refused_share_that_owns_nothing_is_deleted() {
        let s = with_abandon(share_at("ws", "mine", "t/", None), "true");
        assert_eq!(abandon_plan(&s, true, false, false), AbandonVerdict::Delete);
    }

    #[test]
    fn a_healthy_share_is_never_abandoned_however_annotated() {
        // The annotation is written by a web service. If it could delete
        // a share that is serving, the front door would hold exactly the
        // power its role is documented as withholding.
        let s = with_abandon(share_at("ws", "mine", "t/", None), "true");
        match abandon_plan(&s, false, false, false) {
            AbandonVerdict::Refused(why) => assert!(
                why.contains("not in conflict"),
                "the refusal must say why: {why}"
            ),
            other => panic!("a share that did not lose arbitration was {other:?}"),
        }
    }

    #[test]
    fn a_loser_that_has_already_run_is_refused_because_it_owns_data() {
        // The dangerous case: a share demoted LATER by an endpoint
        // change has a PVC holding local data and has already published
        // into the bucket. Deleting that is a decision about data.
        let s = with_abandon(share_at("ws", "mine", "t/", None), "true");
        for (claim, dep) in [(true, false), (false, true), (true, true)] {
            match abandon_plan(&s, true, claim, dep) {
                AbandonVerdict::Refused(why) => assert!(
                    why.contains("has run"),
                    "the refusal must name the hazard: {why}"
                ),
                other => panic!("a share owning claim={claim} dep={dep} was {other:?}"),
            }
        }
    }

    // --- redirect() ------------------------------------------------

    /// Built by deserialization rather than by struct literal: the
    /// spec has no `Default` on purpose (a new field must make every
    /// construction site think), and this way the helper also exercises
    /// the same serde path the API server feeds the controller.
    fn share_at(ns: &str, name: &str, prefix: &str, addr: Option<&str>) -> FlintShare {
        let mut v = serde_json::json!({
            "apiVersion": "flint.io/v1alpha1",
            "kind": "FlintShare",
            "metadata": { "namespace": ns, "name": name },
            "spec": { "bucket": "shared", "persistence": { "size": "1Gi" } },
        });
        if !prefix.is_empty() {
            v["spec"]["keyPrefix"] = serde_json::Value::String(prefix.into());
        }
        if let Some(a) = addr {
            v["status"] = serde_json::json!({ "address": a });
        }
        serde_json::from_value(v).expect("test share")
    }

    #[test]
    fn a_winner_above_us_is_a_redirect_with_a_sub_path() {
        let loser = share_at("ws", "mine", "tenant-x/nested/", None);
        let winner = share_at("ws", "theirs", "tenant-x/", Some("theirs.ws.svc:2049"));
        let r = redirect(&loser, &winner);
        assert_eq!(r.relation, ConflictRelation::Ancestor);
        assert_eq!(r.sub_path.as_deref(), Some("nested/"));
        assert_eq!(r.address.as_deref(), Some("theirs.ws.svc:2049"));
        assert_eq!(r.name, "theirs");
        assert_eq!(r.prefix, "tenant-x/");
    }

    #[test]
    fn a_winner_below_us_is_not_a_redirect_at_all() {
        // We asked for tenant-x/; they serve only tenant-x/nested/.
        // There is no hub covering the difference, and saying "mount
        // theirs" would quietly hand over a SUBSET of what was asked
        // for — the one failure a consumer would not notice.
        let loser = share_at("ws", "mine", "tenant-x/", None);
        let winner = share_at("ws", "theirs", "tenant-x/nested/", Some("theirs.ws.svc:2049"));
        let r = redirect(&loser, &winner);
        assert_eq!(r.relation, ConflictRelation::Descendant);
        assert_eq!(r.sub_path, None, "there is nothing to descend into");
    }

    #[test]
    fn an_equal_prefix_redirects_to_the_export_root() {
        let loser = share_at("ws", "mine", "tenant-x/", None);
        let winner = share_at("ws", "theirs", "tenant-x/", Some("theirs.ws.svc:2049"));
        let r = redirect(&loser, &winner);
        assert_eq!(r.relation, ConflictRelation::Same);
        assert_eq!(r.sub_path, None, "the root needs no sub-path");
        assert_eq!(r.address.as_deref(), Some("theirs.ws.svc:2049"));
    }

    #[test]
    fn a_cross_namespace_winner_is_named_but_never_addressed() {
        // The address is a capability: the hub's NFS export has no
        // per-client authentication. Naming the owner is already in the
        // condition message; handing over a mount target is not.
        let loser = share_at("team-b", "mine", "tenant-x/nested/", None);
        let winner = share_at("team-a", "theirs", "tenant-x/", Some("theirs.team-a.svc:2049"));
        let r = redirect(&loser, &winner);
        assert_eq!(r.namespace, "team-a");
        assert_eq!(r.name, "theirs");
        assert_eq!(r.relation, ConflictRelation::Ancestor);
        assert_eq!(
            r.address, None,
            "a cross-namespace address would answer a typo'd prefix with a pointer at \
             another tenant's live data"
        );
        // ...and the rest is still useful: you know who to ask.
        assert_eq!(r.sub_path.as_deref(), Some("nested/"));
    }

    #[test]
    fn a_whole_bucket_winner_still_yields_a_usable_sub_path() {
        let loser = share_at("ws", "mine", "tenant-x/", None);
        let winner = share_at("ws", "theirs", "", Some("theirs.ws.svc:2049"));
        let r = redirect(&loser, &winner);
        assert_eq!(r.relation, ConflictRelation::Ancestor);
        assert_eq!(r.sub_path.as_deref(), Some("tenant-x/"));
    }

    #[test]
    fn a_mid_segment_containment_yields_no_sub_path() {
        // `overlaps` compares raw strings on purpose, so `tenant-a`
        // contains `tenant-abc`. CEL refuses a prefix without a
        // trailing slash on the way in, but if it ever arrives, a
        // sub-path derived from it would be a WRONG path rather than an
        // absent one — and this field is meant to be acted on.
        let loser = share_at("ws", "mine", "tenant-abc/", None);
        let winner = share_at("ws", "theirs", "tenant-a", Some("theirs.ws.svc:2049"));
        let r = redirect(&loser, &winner);
        assert_eq!(r.relation, ConflictRelation::Ancestor);
        assert_eq!(r.sub_path, None, "\"bc/\" is not a path beneath anything");
    }

    #[test]
    fn a_broad_prefix_is_rejected_by_the_oldest_of_several_descendants() {
        // `tenant-b/` is OLDER; `tenant-a/` sorts first lexically.
        let b = cand("b", "tenant-b/", 10);
        let a = cand("a", "tenant-a/", 20);
        let broad = cand("broad", "tenant-", 30);
        let fleet = vec![b.clone(), a.clone(), broad.clone()];

        match admit(&fleet, &broad) {
            Admission::Rejected { ref winner, .. } => {
                assert_eq!(winner, "ws/b", "admit() names the OLDEST overlap");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert_equivalent(&fleet, &[]);
    }

    /// The ancestor direction really is bounded to one, and it is the
    /// immediate lexicographic predecessor — the half of the lemma that
    /// IS true, pinned so a future refactor cannot quietly widen it.
    #[test]
    fn a_deep_prefix_is_rejected_by_its_only_possible_ancestor() {
        let outer = cand("outer", "t/", 10);
        let sibling = cand("sib", "t/a", 20); // rejected: under `t/`
        let deep = cand("deep", "t/b/c/", 30);
        let fleet = vec![outer, sibling, deep];
        assert_equivalent(&fleet, &[]);
    }

    /// `overlaps` is raw `starts_with`, so a prefix without a trailing
    /// slash collides with a longer NAME, exactly as it would in S3.
    /// The index must reproduce that, not a slash-aware version of it.
    #[test]
    fn non_slash_terminated_prefixes_collide_the_same_way_in_the_index() {
        let fleet = vec![
            cand("a", "tenant-a", 10),
            cand("abc", "tenant-abc", 20), // starts_with("tenant-a") => overlaps
            cand("z", "zzz/", 30),
        ];
        assert_equivalent(&fleet, &[]);
    }

    /// An empty prefix owns the whole bucket and overlaps everything —
    /// the pathological range for the descendant scan.
    #[test]
    fn an_empty_prefix_is_handled_from_both_directions() {
        let early = vec![cand("root", "", 5), cand("a", "a/", 10), cand("b", "b/", 20)];
        assert_equivalent(&early, &[]);
        let late = vec![cand("a", "a/", 10), cand("b", "b/", 20), cand("root", "", 30)];
        assert_equivalent(&late, &[]);
    }

    /// A candidate the table never saw takes `admit`'s other branch,
    /// which uses a SHORTER message. Both must match.
    #[test]
    fn a_candidate_absent_from_the_fleet_gets_the_short_message() {
        let fleet = vec![cand("a", "tenant-a/", 10)];
        let stranger = cand("ghost", "tenant-a/deep/", 99);
        let clean = cand("clean", "other/", 99);
        assert_equivalent(&fleet, &[stranger, clean]);
    }

    /// Tier-off shares own nothing, and separate buckets/endpoints are
    /// separate universes. Neither may leak into the other's group.
    #[test]
    fn tier_off_and_foreign_stores_never_collide() {
        let mut off = cand("off", "tenant-a/", 10);
        off.key = None;
        let mut other_bucket = cand("ob", "tenant-a/", 20);
        other_bucket.key.as_mut().unwrap().bucket = "elsewhere".into();
        let mut other_ep = cand("oe", "tenant-a/", 30);
        other_ep.key.as_mut().unwrap().endpoint = "http://minio:9000".into();
        let fleet = vec![off, other_bucket, other_ep, cand("mine", "tenant-a/", 40)];
        assert_equivalent(&fleet, &[]);
    }

    /// Ties in `created` fall through to uid then to namespace/name, and
    /// the table must break them the same way or the winner drifts.
    #[test]
    fn creation_time_ties_break_identically() {
        let fleet = vec![
            cand("c", "tenant-a/", 10),
            cand("a", "tenant-a/", 10),
            cand("b", "tenant-a/", 10),
        ];
        assert_equivalent(&fleet, &[]);
    }

    /// A fleet with the shape the design target actually has — many
    /// disjoint prefixes plus deliberate nesting, siblings, duplicates
    /// and unstamped newcomers — driven deterministically so a failure
    /// reproduces. Guards against an index that is only right on the
    /// hand-written cases above.
    #[test]
    fn the_index_agrees_with_admit_across_a_hostile_fleet() {
        let shapes: [&dyn Fn(usize) -> String; 4] = [
            &|i| format!("tenant-{i:04}/"),          // disjoint
            &|i| format!("tenant-{:04}/sub/", i / 3), // nested under a disjoint one
            &|i| format!("tenant-{:04}", i / 7),      // non-slash, collides by name
            &|i| if i % 11 == 0 { String::new() } else { format!("g{}/x/", i % 5) },
        ];
        let mut fleet = Vec::new();
        for i in 0..240usize {
            let prefix = shapes[i % shapes.len()](i);
            // Ages deliberately NOT in insertion order, so "oldest"
            // and "lexicographically first" disagree constantly.
            let created = ((i * 37) % 240) as i64;
            let mut c = cand(&format!("s{i}"), &prefix, created);
            if i % 23 == 0 {
                c.uid = String::new(); // not yet stamped by the API server
            }
            fleet.push(c);
        }
        let strangers = vec![
            cand("ghost-deep", "tenant-0003/sub/deeper/", 1),
            cand("ghost-root", "", 2),
            cand("ghost-clean", "nothing-like-this/", 3),
        ];
        assert_equivalent(&fleet, &strangers);
    }

    fn cand(name: &str, prefix: &str, created: i64) -> Candidate {
        Candidate {
            namespace: "ws".into(),
            name: name.into(),
            uid: format!("uid-{name}"),
            created,
            key: Some(ShareKey {
                endpoint: String::new(),
                bucket: "team".into(),
                prefix: prefix.into(),
            }),
        }
    }

    #[test]
    fn nesting_is_overlap_but_sibling_names_are_not() {
        let k = |p: &str| ShareKey {
            endpoint: String::new(),
            bucket: "team".into(),
            prefix: p.into(),
        };
        assert!(overlaps(&k("tenant-a/"), &k("tenant-a/")));
        assert!(overlaps(&k("tenant-a/"), &k("tenant-a/sub/")), "nesting is overlap");
        assert!(overlaps(&k("tenant-a/sub/"), &k("tenant-a/")), "and it is symmetric");
        assert!(overlaps(&k(""), &k("anything/")), "the whole bucket contains everything");
        assert!(
            !overlaps(&k("tenant-a/"), &k("tenant-ab/")),
            "a trailing slash is what keeps siblings apart"
        );
        // Conservative on purpose: without the slash, S3 itself would
        // match the sibling, so we refuse rather than reason about it.
        assert!(overlaps(&k("tenant-a"), &k("tenant-abc/")));
    }

    #[test]
    fn a_different_bucket_or_endpoint_is_different_storage() {
        let a = ShareKey {
            endpoint: String::new(),
            bucket: "team".into(),
            prefix: "x/".into(),
        };
        let b = ShareKey {
            bucket: "other".into(),
            ..a.clone()
        };
        let c = ShareKey {
            endpoint: "http://minio:9000".into(),
            ..a.clone()
        };
        assert!(!overlaps(&a, &b));
        assert!(!overlaps(&a, &c));
    }

    #[test]
    fn the_oldest_share_wins_and_the_younger_one_is_told_who() {
        let old = cand("first", "tenant-a/", 100);
        let new = cand("second", "tenant-a/sub/", 200);
        let fleet = vec![new.clone(), old.clone()]; // cache order is not age order

        assert_eq!(admit(&fleet, &old), Admission::Admitted);
        match admit(&fleet, &new) {
            Admission::Rejected { winner, message } => {
                assert_eq!(winner, "ws/first");
                assert!(message.contains("tenant-a/sub/"), "{message}");
                assert!(message.contains("ws/first"), "{message}");
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    /// creationTimestamp has one-second granularity and `kubectl apply
    /// -f dir/` stamps a batch identically — without a tiebreak the
    /// two shares could each conclude they won.
    #[test]
    fn same_second_creations_still_have_exactly_one_winner() {
        let a = cand("alpha", "p/", 100);
        let b = cand("bravo", "p/", 100);
        let fleet = vec![a.clone(), b.clone()];
        let winners = [admit(&fleet, &a), admit(&fleet, &b)]
            .iter()
            .filter(|v| **v == Admission::Admitted)
            .count();
        assert_eq!(winners, 1, "exactly one of a same-second pair may run");

        // ... and the answer does not depend on cache ordering.
        let reversed = vec![b.clone(), a.clone()];
        assert_eq!(admit(&fleet, &a), admit(&reversed, &a));
    }

    /// A broad prefix that loses must not drag down narrow shares that
    /// never conflicted with the winner.
    #[test]
    fn a_loser_does_not_block_the_shares_it_alone_overlapped() {
        let narrow = cand("narrow", "x/", 100); // oldest
        let broad = cand("broad", "", 200); // whole bucket, overlaps everyone
        let other = cand("other", "y/", 300); // only ever overlapped `broad`
        let fleet = vec![narrow.clone(), broad.clone(), other.clone()];

        assert_eq!(admit(&fleet, &narrow), Admission::Admitted);
        assert!(matches!(admit(&fleet, &broad), Admission::Rejected { .. }));
        assert_eq!(
            admit(&fleet, &other),
            Admission::Admitted,
            "`other` only overlapped a share that is not running"
        );
    }

    /// Deleting the winner promotes the survivor — the reason the
    /// caller re-queues the overlap set instead of only the changed CR.
    #[test]
    fn removing_the_winner_promotes_the_loser() {
        let old = cand("first", "p/", 100);
        let new = cand("second", "p/", 200);
        assert!(matches!(
            admit(&[old.clone(), new.clone()], &new),
            Admission::Rejected { .. }
        ));
        assert_eq!(admit(&[new.clone()], &new), Admission::Admitted);
    }

    #[test]
    fn a_tier_off_share_neither_conflicts_nor_is_conflicted() {
        let plain = Candidate {
            key: None,
            ..cand("plain", "", 1)
        };
        let owner = cand("owner", "", 100);
        assert_eq!(admit(&[owner.clone(), plain.clone()], &plain), Admission::Admitted);
        assert_eq!(admit(&[owner.clone(), plain.clone()], &owner), Admission::Admitted);
        assert!(overlap_set(&[owner], &plain).is_empty());
    }

    /// Cross-namespace is the case CEL could never see, and the one a
    /// bucket-per-tenant fleet actually hits.
    #[test]
    fn conflicts_are_found_across_namespaces() {
        let a = Candidate {
            namespace: "team-a".into(),
            ..cand("share", "shared/", 100)
        };
        let b = Candidate {
            namespace: "team-b".into(),
            uid: "uid-b".into(),
            ..cand("share", "shared/", 200)
        };
        match admit(&[a, b.clone()], &b) {
            Admission::Rejected { winner, .. } => assert_eq!(winner, "team-a/share"),
            other => panic!("expected a cross-namespace conflict, got {other:?}"),
        }
    }

    /// The very first reconcile can run against a cache that has not
    /// yet seen the CR itself; it must still be judged, not waved
    /// through.
    #[test]
    fn a_share_missing_from_the_cache_is_still_judged() {
        let old = cand("first", "p/", 100);
        let me = cand("second", "p/", 200);
        match admit(&[old], &me) {
            Admission::Rejected { winner, .. } => assert_eq!(winner, "ws/first"),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    /// An unstamped CR must not outrank a share that is already
    /// serving — otherwise creating a duplicate would depose the
    /// original for as long as the cache lacked a timestamp.
    #[test]
    fn an_unstamped_newcomer_never_outranks_a_live_share() {
        let live = cand("live", "p/", 100);
        let fresh = Candidate {
            created: i64::MAX,
            ..cand("fresh", "p/", 0)
        };
        assert_eq!(admit(&[live.clone(), fresh.clone()], &live), Admission::Admitted);
        assert!(matches!(
            admit(&[live, fresh.clone()], &fresh),
            Admission::Rejected { .. }
        ));
    }

    #[test]
    fn the_requeue_set_is_everyone_who_could_change_their_mind() {
        let me = cand("me", "x/", 100);
        let nested = cand("nested", "x/deep/", 200);
        let unrelated = cand("unrelated", "y/", 300);
        let fleet = vec![me.clone(), nested.clone(), unrelated];
        let set = overlap_set(&fleet, &me);
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].name, "nested");
    }
}
