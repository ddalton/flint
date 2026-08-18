//! The S3 cold tier's server-side machinery (L2).
//!
//! Design of record: docs/plans/s3-tier-l2-design-review.md — the
//! ultracode-reviewed, reshaped design. Build order and invariants live
//! there; nothing in this tree may contradict it.
//!
//! Step 1: `capture` — mutation-complete dirty capture (A2).
//! Step 2: `durable` — the durable dirty bit's marshalling (A3), with
//! the schema and flush-intent rows (A6) in the state backend.
//! Step 3: `gate` — the per-file write gate, exclusion, and flush
//! single-flight (A4, A5's cache primitive).
//! Step 4: `store` — the object store behind the A13 trait (S3 backend
//! plus in-memory test double); `arbitrate` — A6's HEAD-based 412
//! arbitration; `meter` — the A12 counter skeleton.

//! Step 5: `flush` — the pipeline (A11 knobs as requirements): part
//! planner, flush floor + quiescence, intents, arbitration wiring.

//! Step 6: `identity` — rename/remove events, tombstones, the A7
//! identity-keyed generation rows (backend) and re-key flushes.

//! Step 7: `epoch` — the volume epoch (A8): lease heartbeat,
//! self-recognition, takeover MPU abort-sweep, self-fencing; the
//! flusher re-verifies it before every publish.

//! Step 8: `space` — PVC space model (A10): NOSPC admission before
//! hard-full, statvfs-backed SPACE_* gauge, eviction watermark, and
//! the state.db ballast.

//! Step 10: `evict` — the marker-before-truncate eviction state
//! machine (A5/C2), the A4 precondition set, and the startup
//! reconciler. The automatic (watermark-driven) trigger wires up with
//! hydration in step 11 — an evicted file cannot be read back until
//! then.

//! Step 11: `hydrate` — in-place restore into the marker inode
//! (A5/C6), the durable hydrating flag (crashed restores truncate
//! back, never serve partials), write-pending priority + RPC parking
//! (the step-9 gate findings), S3-wins foreign adopts, and the A10
//! admission wait. The watermark-driven `evict_pass` goes live with
//! it.

//! Step 12: `manifest` — the A12 DR manifest at every flush barrier
//! (metadata checkpoint + RPO record + restore driver, with the
//! declined round-trips enumerated); `import` — the A7-consuming
//! import-refresh verb: manifest-driven tree restore + bucket sweep,
//! materializing evicted stubs that hydrate on first touch, never
//! resurrecting a tombstoned key. DR = CAS + manifest-driven restore
//! + consumer remount.

pub mod arbitrate;
pub mod capture;
pub mod durable;
pub mod epoch;
pub mod evict;
pub mod flush;
pub mod gate;
pub mod hydrate;
pub mod identity;
pub mod import;
pub mod manifest;
pub mod meter;
pub mod reporter;
pub mod space;
pub mod store;
