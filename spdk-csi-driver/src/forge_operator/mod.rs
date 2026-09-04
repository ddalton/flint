//! flint forge's control plane: the `FlintRepo` CRD, and (phase 3) the
//! reconciler that renders a server pod for each one.
//!
//! Design of record: `docs/plans/flint-forge-design.md`. The syncer
//! that runs inside those pods is the `flint-forge` crate, deliberately
//! outside this one.

pub mod crd;
pub mod idle;
pub mod reconcile;
pub mod render;
