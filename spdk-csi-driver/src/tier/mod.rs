//! The S3 cold tier's server-side machinery (L2).
//!
//! Design of record: docs/plans/s3-tier-l2-design-review.md — the
//! ultracode-reviewed, reshaped design. Build order and invariants live
//! there; nothing in this tree may contradict it.
//!
//! Step 1: `capture` — mutation-complete dirty capture (A2).
//! Step 2: `durable` — the durable dirty bit's marshalling (A3), with
//! the schema and flush-intent rows (A6) in the state backend.

pub mod capture;
pub mod durable;
