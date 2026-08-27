//! `flint-passthrough` — an S3 prefix, mounted into a pod by
//! Mountpoint for S3, delivered by the same webhook mechanism
//! flint-lean uses.
//!
//! WHAT THIS IS NOT. There is no checkout, no manifest, no claim, no
//! lease, no publish boundary and no controller. Reads and writes go
//! straight to the bucket over HTTP, one object at a time, with the
//! FUSE client's own semantics and nothing else's. That is the entire
//! product, and it is why this module has three files and no formal
//! model: there is no distributed state here to get wrong.
//!
//! WHAT IT COSTS. Two things, both structural rather than fixable:
//!
//! 1. The sidecar is PRIVILEGED. `mountPropagation: Bidirectional` is
//!    the only way a mount made in a sidecar reaches the app
//!    container, and the API server allows it only on a privileged
//!    container. Namespaces enforcing PodSecurity `baseline` or
//!    `restricted` will reject the mutated pod. flint-lean's sidecar
//!    needs no privilege at all — it copies bytes rather than mounting
//!    — so this is a real difference between the two front ends, not a
//!    packaging detail.
//!
//! 2. A MOUNTER CRASH IS NOT RECOVERABLE IN PLACE. Every container
//!    already running in the pod holds a private copy of the FUSE
//!    filesystem, made when it started; when the mounter dies that copy
//!    goes ENOTCONN and stays there, and the replacement mounter's
//!    fresh mount does not reach it. The pod has to be recreated. This
//!    is the price of the start gate: a consumer that mounted BEFORE
//!    the FUSE mount existed would hold a propagation slave and would
//!    recover — and would also be free to start against an empty
//!    directory. The sidecar detects the case and reports itself NOT
//!    READY so the pod stops taking traffic rather than serving
//!    ENOTCONN quietly; see `inject::restart_detector`.
//!
//! 3. IT IS NOT POSIX AND DOES NOT PRETEND TO BE. There is exactly
//!    one mounter — Mountpoint for S3 — and its write model is
//!    sequential writes to whole objects: no rename, no append, no
//!    in-place modification, at any setting. `pip install`, `git` and
//!    sqlite do not work here and cannot be made to. That is a choice.
//!    The POSIX-emulating clients (s3fs, goofys) would buy those
//!    workloads a working tree with no coordination behind it —
//!    last-writer-wins between two pods, undetected — which is a worse
//!    answer than "use the front end that has a publish boundary".
//!
//! Reach for this when the workload wants to browse or stream a large
//! bucket lazily and only ever writes whole new objects. Reach for
//! flint-lean when the pod wants a real working tree with a publish
//! boundary — see `lean_operator`.

pub mod inject;
pub mod spec;
pub mod webhook;
