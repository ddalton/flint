//! The S3 cold tier's server-side machinery (L2).
//!
//! Design of record: docs/plans/s3-tier-l2-design-review.md — the
//! ultracode-reviewed, reshaped design. Build order and invariants live
//! there; nothing in this tree may contradict it.
//!
//! Step 1 (this module's first tenant): `capture` — mutation-complete
//! dirty capture, amendment A2.

pub mod capture;
