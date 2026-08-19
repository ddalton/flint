//! The flint-lite operator — a fleet control plane for hub-per-volume
//! shares (plan of record: `docs/plans/flint-lite-operator-plan.md`).
//!
//! # What it is
//!
//! One `FlintShare` custom resource per volume; the controller renders
//! the same four objects the lite chart renders (ConfigMap, RWO PVC,
//! Service, single-replica Recreate Deployment) and keeps them
//! converged with server-side apply. The chart stays supported — the
//! render-parity golden test (`render::tests`) fails the build if the
//! two ever drift.
//!
//! # Why, in one line each
//!
//! - **No reusable release state.** Every reconcile re-renders from
//!   the CR plus operator defaults, which structurally kills the
//!   `--reuse-values` failure class (runbr) that helm-release-per-
//!   volume keeps alive.
//! - **The knobs are schema.** `spec.settings` is a typed mirror of
//!   [`crate::pnfs::config::TierKnobs`], so a typo is refused at
//!   admission instead of silently taking a default (the server's YAML
//!   parser ignores unknown keys — the chart's hand-written `$known`
//!   list exists for exactly this and is one more copy to drift).
//! - **Fleet operations become queries.** `kubectl get flintshares` is
//!   the fleet, and one controller can enforce cross-object invariants
//!   no per-release install can see (see [`conflict`]).
//!
//! # The three invariants
//!
//! 1. **The PVC never carries an ownerReference.** Owner GC does not
//!    know what `reclaim: Retain` means; for a tier-off share the PVC
//!    is the only copy of the data. Fail-safe by construction, not by
//!    reconcile correctness ([`reconcile`]).
//! 2. **The bucket is never touched.** No create, no delete, no
//!    lifecycle — the operator's blast radius stops at Kubernetes
//!    objects.
//! 3. **At most one share per (endpoint, bucket, prefix subtree).**
//!    Unarbitrated duplicates are not merely wasteful: when one hub
//!    dies for a lease window the other TAKES OVER the prefix and
//!    serves another tenant's bytes at its own address ([`conflict`]).

pub mod bootstrap;
pub mod conflict;
pub mod crd;
pub mod reconcile;
pub mod render;
