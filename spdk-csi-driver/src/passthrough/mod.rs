//! flint-passthrough: a FlintPassthroughMount names an S3 bucket
//! subtree a pod mounts with Mountpoint for S3. There is no controller
//! (a passthrough mount owns no state to converge) and, since the CSI
//! delivery, no webhook and no sidecar: the s3.flint.io node plugin
//! (`crate::s3csi`) reads the CR, performs the mount as root, hands the
//! FUSE fd to an unprivileged worker and binds the result into the pod.
//! Design of record: docs/plans/csi-node-mount-design.md.
//!
//! What stays here is the CR itself (`spec`) and the mount-s3 argument
//! vector (`mounter`) the worker execs.

pub mod mounter;
pub mod spec;
