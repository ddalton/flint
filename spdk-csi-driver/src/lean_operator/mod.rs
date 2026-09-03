//! flint-lean's control plane: the FlintLeanWorkspace CRD (`crd`), the
//! claim/adopt/refuse reconcile (`reconcile`), the boundary-verb
//! validation (`boundary`), and the syncer environment (`sync_env`) the
//! s3.chert.us node plugin hands to a lean worker. The webhook and the
//! sidecar injector are gone: a workspace reaches a pod as ONE `csi:`
//! volume (docs/plans/csi-node-mount-design.md §3.5, §5).

pub mod boundary;
pub mod crd;
pub mod reconcile;
pub mod sync_env;
