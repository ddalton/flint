//! The flint-lean operator (plan §2.4, Phase 4) — a SEPARATE
//! controller in the shared operator image.
//!
//! Deliberately NOT part of `lite_operator`'s FlintShare reconcile:
//! FlintShare's loop is hub-lifecycle-shaped (Deployments, PVCs,
//! suspend/hibernate); lean's operator surface is webhook-and-claims-
//! shaped with NO long-running per-workspace resources at all — a lean
//! workspace at rest is bucket objects only. Keeping the controllers
//! separate keeps every future hub-side change (strict mode included)
//! out of lean's blast radius, and vice versa. The binary ships in the
//! flint-lite-operator image; the chart picks which binary a pod runs
//! (the flint-hub-gateway precedent).
//!
//! Duties (each in its own module):
//! - `crd`: the FlintLeanWorkspace CRD — the durable, USER-DECLARED
//!   project identity (never the CR UID), the subtree address, the
//!   flush/durability profile, and the budgets.
//! - `inject`: the mutating-webhook BRAIN as a pure function — native
//!   sidecar injection (initContainer with restartPolicy: Always,
//!   K8s ≥ 1.29) with the startupProbe budget DERIVED from the
//!   declared inventory × the 0b-measured rates, never a fleet
//!   constant. The admission HTTP/TLS wrapper is follow-up plumbing;
//!   everything it will do is here and unit-tested.
//! - `boundary`: the operator's half of the boundary-verbs contract —
//!   spec validation (gated needs a lag bound; retention must outlive
//!   one staging window; the derived drain must fit a spot reclaim),
//!   the bucket-side conformance probe and lifecycle posture, and the
//!   `BoundaryModeActive` comparison between what the CR asked for and
//!   what the RUNNING sidecar echoes into its lease cell.
//! - `reconcile`: claim stamping with BOTH adopt arms (equal declared
//!   identity ⇒ adopt — DR/GitOps recreate is a designed lifecycle;
//!   different ⇒ a refusing status, never on-the-fly adoption), the
//!   operator-principal bootstrap (versioning/lifecycle probes), and
//!   the operator-side MPU sweep (bucket-wide `list_uploads` is
//!   DENIED to a project-scoped sidecar by design — plan §2.4).

pub mod boundary;
pub mod crd;
pub mod inject;
pub mod reconcile;
pub mod webhook;
