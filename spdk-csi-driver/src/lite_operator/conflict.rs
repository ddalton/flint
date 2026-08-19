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

use super::crd::FlintShare;

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
