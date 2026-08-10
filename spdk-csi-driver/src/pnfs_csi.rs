//! pNFS CSI integration — driver-side client to the MDS.
//!
//! This module is the bridge between the CSI driver's gRPC handlers
//! (in `main.rs`) and the pNFS MDS's gRPC control surface (the
//! `CreateVolume` / `DeleteVolume` verbs added in
//! `pnfs/grpc.rs`). It is *isolated* — nothing in the SPDK code path
//! imports from here, and `main.rs` only constructs a `PnfsCsi` if
//! `FLINT_PNFS_MDS_ENDPOINT` is set in the environment.
//!
//! When a `StorageClass` carries `parameters.layout: pnfs`, the
//! controller's `CreateVolume` calls
//! [`PnfsCsi::create_volume`], which:
//!   1. Talks to the MDS over gRPC at the operator-configured endpoint.
//!   2. Asks the MDS to create the volume's directory subtree at
//!      `<export>/<volume_id>/` (directory-per-volume; each PVC is an
//!      isolated shared namespace that NodePublish mounts as
//!      `MDS:/<volume_id>`).
//!   3. Returns a `volume_context` map carrying every key the
//!      `NodePublishVolume` path (PR 3) needs to mount the volume.
//!
//! On `DeleteVolume`, the symmetric path runs.
//!
//! The `pnfs.flint.io/*` namespace was chosen over `nfs.flint.io/*` so
//! it visibly does not collide with the existing single-server-NFS
//! `nfs.flint.io/*` keys written by `rwx_nfs.rs`. Future tooling can
//! tell the two volume shapes apart by which prefix is present.

use std::collections::HashMap;
use std::time::Duration;

use crate::pnfs::grpc::{
    CreateVolumeRequest, DeleteVolumeRequest,
};

/// Volume context keys written by [`PnfsCsi::create_volume`] and
/// expected by `node_publish_volume`. Centralising them here keeps the
/// producer and consumer in sync.
pub mod ctx_keys {
    pub const MDS_IP: &str = "pnfs.flint.io/mds-ip";
    pub const MDS_PORT: &str = "pnfs.flint.io/mds-port";
    pub const EXPORT_PATH: &str = "pnfs.flint.io/export-path";
    pub const VOLUME_FILE: &str = "pnfs.flint.io/volume-file";
    pub const SIZE_BYTES: &str = "pnfs.flint.io/size-bytes";
    /// "dir" for directory-per-volume PVs (NodePublish mounts the
    /// `MDS:/<volume>` subtree), "file" for legacy sparse-file PVs
    /// (NodePublish mounts the export root). Absent on PVs provisioned
    /// before this key existed — treat as "file".
    pub const VOLUME_MODE: &str = "pnfs.flint.io/volume-mode";
    /// The stripe geometry the MDS actually recorded, stamped onto the
    /// PV so it is visible with `kubectl get pv -o yaml`. Observability
    /// only — nothing reads these back, because no RPC exists to assert
    /// geometry on an existing volume. They answer "what is this volume
    /// actually striped at?", which otherwise requires reading MDS logs.
    pub const STRIPE_SIZE: &str = "pnfs.flint.io/stripe-size";
    pub const STRIPE_WIDTH: &str = "pnfs.flint.io/stripe-width";
    /// The volume-context DISCRIMINATOR (design doc §7): "block" for a
    /// pnfs-block (scsi-layout) volume; absent = files layout. Every
    /// downstream classifier keys on `mds-ip` presence or the `~m`
    /// shard suffix, and a block volume reuses both — without this key
    /// NodeUnstage cannot tell the classes apart, and a block volume's
    /// unstage must deref nvme sessions where a file volume's must not.
    pub const LAYOUT: &str = "pnfs.flint.io/layout";
}

/// StorageClass parameter names understood by the pNFS provisioning
/// path. Everything under `pnfs.flint.io/` that is NOT in this list is
/// REJECTED at CreateVolume — a typo in a parameter name would
/// otherwise be indistinguishable from success, and the mistake only
/// becomes visible as a performance or layout surprise much later.
pub mod sc_params {
    pub const STRIPE_SIZE: &str = "pnfs.flint.io/stripeSize";
    pub const STRIPE_WIDTH: &str = "pnfs.flint.io/stripeWidth";
    pub const DIR_GID: &str = "pnfs.flint.io/dirGid";
    pub const DIR_MODE: &str = "pnfs.flint.io/dirMode";

    pub const ALL: &[&str] = &[STRIPE_SIZE, STRIPE_WIDTH, DIR_GID, DIR_MODE];
}

/// Per-volume options carried from StorageClass parameters into
/// `CreateVolume`. All-zero means "MDS defaults", which is exactly the
/// behaviour of every volume provisioned before these existed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VolumeOptions {
    /// Stripe unit in bytes; 0 = MDS default.
    pub stripe_size: u64,
    /// Max data servers a file is pinned across; 0 = all active.
    pub stripe_width: u32,
    /// Group owner for the volume root; 0 = leave alone.
    pub dir_gid: u32,
    /// Permission bits for the volume root; 0 = the historical 0777.
    pub dir_mode: u32,
    /// Layout class from the StorageClass `layout` parameter: File for
    /// `pnfs` (and everything historical), Scsi for `pnfs-block`. Not a
    /// `pnfs.flint.io/*` parameter — the top-level `layout` key is the
    /// class selector, dispatched in main.rs before this struct exists.
    pub layout_class: crate::pnfs::mds::layout::LayoutClass,
}

impl VolumeOptions {
    /// Parse from StorageClass `parameters`, rejecting anything wrong.
    ///
    /// Errors are returned as human-readable strings destined for a
    /// FAILED_PRECONDITION — they surface on the PVC as an event, which
    /// is the only place a StorageClass author will look.
    pub fn from_parameters(
        params: &std::collections::HashMap<String, String>,
    ) -> Result<Self, String> {
        // Unknown keys in our namespace are a hard error, not a warning.
        for key in params.keys() {
            if key.starts_with("pnfs.flint.io/") && !sc_params::ALL.contains(&key.as_str()) {
                return Err(format!(
                    "unknown StorageClass parameter '{}'; supported: {}",
                    key,
                    sc_params::ALL.join(", ")
                ));
            }
        }

        let num = |key: &str| -> Result<Option<u64>, String> {
            match params.get(key) {
                None => Ok(None),
                Some(raw) => {
                    let t = raw.trim();
                    // Modes are octal by universal convention ("0770");
                    // everything else is decimal.
                    let parsed = if key == sc_params::DIR_MODE {
                        u64::from_str_radix(t.trim_start_matches("0o"), 8)
                    } else {
                        t.parse::<u64>()
                    };
                    parsed
                        .map(Some)
                        .map_err(|_| format!("{}: '{}' is not a valid number", key, raw))
                }
            }
        };

        let mut o = Self::default();

        if let Some(v) = num(sc_params::STRIPE_SIZE)? {
            // Bounds: below one page striping is pure RPC overhead, and
            // the unit must divide evenly into the layout arithmetic
            // (offset / stripe_size), so a power of two keeps the
            // per-DS extents aligned with client readahead.
            if v < 4096 || v > 1024 * 1024 * 1024 {
                return Err(format!(
                    "{}: {} out of range (4 KiB .. 1 GiB)",
                    sc_params::STRIPE_SIZE,
                    v
                ));
            }
            if !v.is_power_of_two() {
                return Err(format!(
                    "{}: {} must be a power of two",
                    sc_params::STRIPE_SIZE,
                    v
                ));
            }
            o.stripe_size = v;
        }

        if let Some(v) = num(sc_params::STRIPE_WIDTH)? {
            if v == 0 || v > 4096 {
                return Err(format!(
                    "{}: {} out of range (1 .. 4096; omit the parameter for 'all data servers')",
                    sc_params::STRIPE_WIDTH,
                    v
                ));
            }
            o.stripe_width = v as u32;
        }

        if let Some(v) = num(sc_params::DIR_GID)? {
            if v == 0 || v > u32::MAX as u64 {
                return Err(format!("{}: {} is not a usable gid", sc_params::DIR_GID, v));
            }
            o.dir_gid = v as u32;
        }

        if let Some(v) = num(sc_params::DIR_MODE)? {
            if v == 0 || v > 0o7777 {
                return Err(format!(
                    "{}: '{}' is not a valid octal mode",
                    sc_params::DIR_MODE,
                    params.get(sc_params::DIR_MODE).unwrap()
                ));
            }
            o.dir_mode = v as u32;
        }

        // A non-default mode without a group is a foot-gun: 0770 with no
        // gid means the volume is writable only by the server's own
        // group, which no pod is in, and every write fails with EACCES
        // at first use rather than at provision time.
        if o.dir_mode != 0 && o.dir_mode & 0o007 == 0 && o.dir_gid == 0 {
            return Err(format!(
                "{} = {:o} denies access to other users but no {} was set —                  the volume would be unwritable by any pod",
                sc_params::DIR_MODE,
                o.dir_mode,
                sc_params::DIR_GID
            ));
        }

        Ok(o)
    }

    /// Compare what the MDS says it recorded against what we asked for.
    fn check_echo(&self, eff_size: u64, eff_width: u32) -> Result<(), String> {
        let asked_for_geometry = self.stripe_size != 0 || self.stripe_width != 0;
        if !asked_for_geometry {
            return Ok(());
        }
        if eff_size == 0 && eff_width == 0 {
            return Err(format!(
                "this MDS does not support per-volume stripe geometry                  (it echoed nothing for {}/{}). Upgrade the pNFS server image                  to match the CSI driver, or remove the parameters from the                  StorageClass",
                sc_params::STRIPE_SIZE,
                sc_params::STRIPE_WIDTH,
            ));
        }
        if self.stripe_size != 0 && eff_size != self.stripe_size {
            return Err(format!(
                "MDS recorded stripe size {} but {} asked for {}",
                eff_size,
                sc_params::STRIPE_SIZE,
                self.stripe_size
            ));
        }
        if self.stripe_width != 0 && eff_width != self.stripe_width {
            return Err(format!(
                "MDS recorded stripe width {} but {} asked for {}",
                eff_width,
                sc_params::STRIPE_WIDTH,
                self.stripe_width
            ));
        }
        Ok(())
    }
}

/// All errors the `pnfs_csi` surface can produce. Each maps to a CSI
/// gRPC `Status` at the call site in `main.rs`; we don't depend on
/// `tonic::Status` here so the module stays testable in isolation.
#[derive(Debug)]
pub enum PnfsError {
    /// gRPC connect or call failed (MDS unreachable, TLS issue, etc.).
    Transport(String),
    /// The MDS returned a structured error (e.g. size mismatch on
    /// re-create, path-traversal volume_id).
    Mds(String),
    /// The endpoint string is malformed or empty.
    BadEndpoint(String),
    /// The volume_id's shard suffix points at a shard this controller
    /// is not configured with (mds.count scaled below a shard that
    /// still owns volumes) — a configuration error, not a retryable
    /// condition.
    ShardRouting(String),
}

impl std::fmt::Display for PnfsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(m) => write!(f, "pNFS transport: {}", m),
            Self::Mds(m) => write!(f, "pNFS MDS: {}", m),
            Self::BadEndpoint(m) => write!(f, "pNFS bad endpoint: {}", m),
            Self::ShardRouting(m) => write!(f, "pNFS shard routing: {}", m),
        }
    }
}
impl std::error::Error for PnfsError {}

/// Driver-side handle to the MDS gRPC service.
///
/// One instance is constructed at driver startup (when the
/// `FLINT_PNFS_MDS_ENDPOINT` env var is set) and stashed on the
/// controller's state struct. Cloning it is cheap (the inner
/// configuration is just two strings) and each call dials gRPC fresh
/// — we don't yet pool the channel because volume create/delete
/// happens on a human-action timescale, not a hot path. If that ever
/// becomes a bottleneck, this is the only file that has to change.
#[derive(Clone, Debug)]
pub struct PnfsCsi {
    /// Tonic-style URI, e.g. `http://flint-pnfs-mds:50051`. Always
    /// includes the scheme so `tonic::Endpoint::from_shared` succeeds
    /// without ambiguity.
    endpoint: String,
    /// Per-call timeout. Volume operations on the MDS are local
    /// (file create / unlink); 10 s is generous and keeps a wedged
    /// MDS from stalling the CSI provisioner indefinitely.
    timeout: Duration,
}

impl PnfsCsi {
    /// Construct a `PnfsCsi` from the `FLINT_PNFS_MDS_ENDPOINT` env
    /// var. Returns `None` if the var is unset or empty — that's the
    /// signal to `main.rs` that pNFS support is *not* enabled on this
    /// driver build, and any `parameters.layout: pnfs` request should
    /// be rejected with a clear error rather than silently running on
    /// the SPDK path.
    ///
    /// Accepted forms:
    /// * `flint-pnfs-mds:50051` — bare host:port; we add `http://`.
    /// * `http://flint-pnfs-mds:50051` — explicit scheme.
    /// * `https://...` — explicit TLS (the gRPC channel honours it,
    ///   though we don't ship cluster TLS yet).
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var("FLINT_PNFS_MDS_ENDPOINT").ok()?;
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        Some(Self::new(raw))
    }

    /// Direct constructor (used by tests, and by `from_env`).
    pub fn new(endpoint: impl Into<String>) -> Self {
        let raw = endpoint.into();
        let with_scheme = if raw.starts_with("http://") || raw.starts_with("https://") {
            raw
        } else {
            format!("http://{}", raw)
        };
        Self {
            endpoint: with_scheme,
            timeout: Duration::from_secs(10),
        }
    }

    /// Override the per-call timeout. Tests use this; production
    /// leaves it at the 10 s default.
    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Endpoint reported back for logging / volume_context.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Dial the MDS. Each volume op opens a fresh channel — see the
    /// rationale on the struct doc-comment. The client attaches
    /// FLINT_PNFS_CONTROL_TOKEN when configured.
    async fn dial(&self) -> Result<crate::pnfs::grpc::AuthedMdsControlClient, PnfsError> {
        let ep = tonic::transport::Endpoint::from_shared(self.endpoint.clone())
            .map_err(|e| PnfsError::BadEndpoint(format!("{}: {}", self.endpoint, e)))?
            .connect_timeout(self.timeout)
            .timeout(self.timeout);
        let channel = ep
            .connect()
            .await
            .map_err(|e| PnfsError::Transport(format!("connect {}: {}", self.endpoint, e)))?;
        Ok(crate::pnfs::grpc::authed_mds_control_client(channel))
    }

    /// Provision a pNFS volume: tell the MDS to create the metadata
    /// file, then return a `volume_context` map the node-publish path
    /// will use to mount.
    ///
    /// Idempotent: if the MDS already holds a volume with this name
    /// at the requested size, this is a success and we return the
    /// existing volume's context (so a retry from a flaky CSI
    /// provisioner doesn't fail).
    pub async fn create_volume(
        &self,
        volume_id: &str,
        size_bytes: u64,
    ) -> Result<HashMap<String, String>, PnfsError> {
        self.create_volume_with(volume_id, size_bytes, &VolumeOptions::default())
            .await
    }

    /// `create_volume` with per-volume options from the StorageClass.
    pub async fn create_volume_with(
        &self,
        volume_id: &str,
        size_bytes: u64,
        opts: &VolumeOptions,
    ) -> Result<HashMap<String, String>, PnfsError> {
        let mut client = self.dial().await?;
        let resp = client
            .create_volume(CreateVolumeRequest {
                volume_id: volume_id.to_string(),
                size_bytes,
                stripe_size: opts.stripe_size,
                stripe_width: opts.stripe_width,
                dir_gid: opts.dir_gid,
                dir_mode: opts.dir_mode,
                layout_class: opts.layout_class.as_str().to_string(),
            })
            .await
            .map_err(|e| PnfsError::Transport(format!("CreateVolume: {}", e)))?
            .into_inner();

        if !resp.created {
            return Err(PnfsError::Mds(if resp.message.is_empty() {
                "MDS rejected CreateVolume (no message)".into()
            } else {
                resp.message
            }));
        }

        // Version-skew gate. The chart pins the pNFS server image
        // independently of the driver image, and proto3 silently drops
        // fields an older peer does not understand — so an MDS that
        // predates stripe geometry would happily return `created: true`
        // having ignored it, and the PVC would come up striped at the
        // MDS default with nothing anywhere saying so. Fail the
        // provision instead: a PVC stuck Pending with this message is
        // recoverable, a volume silently built to the wrong geometry is
        // not (placements are pinned at first layout grant and never
        // re-striped).
        if let Err(msg) = opts.check_echo(resp.effective_stripe_size, resp.effective_stripe_width) {
            return Err(PnfsError::Mds(msg));
        }

        // Same version-skew gate for the layout class, and higher
        // stakes: an MDS predating the block layout echoes "" here
        // (proto3 drops the unknown field both ways) while happily
        // creating a FILES-class volume — a PV the workload believes is
        // extent-backed, served stripe layouts forever. Fail the
        // provision; never downgrade silently.
        if opts.layout_class == crate::pnfs::mds::layout::LayoutClass::Scsi
            && resp.effective_layout_class != "scsi"
        {
            return Err(PnfsError::Mds(format!(
                "this MDS does not support the pnfs-block layout class \
                 (echoed layout_class {:?}; expected \"scsi\") — upgrade the \
                 pNFS server image to match the driver",
                resp.effective_layout_class,
            )));
        }

        // The mount host is the same name we dialed for gRPC (in the
        // chart both live behind the flint-pnfs-mds Service), but the
        // mount PORT is not: we dialed the gRPC port, and stamping it
        // into the context sent the kernel's NFS mount to the gRPC
        // listener (found live on runn, 2026-07-06). The MDS reports
        // its NFS bind port in the response; 0 means an older MDS that
        // predates the field, where the standard 2049 is the best bet.
        let (host, _grpc_port) = parse_host_port(&self.endpoint)?;

        // Resolve the host to an IPv4 HERE, at provision time, where we
        // run as a normal pod with ClusterFirst DNS. The kernel mount
        // executes in the NODE's network context, whose resolver knows
        // nothing about *.svc.cluster.local (mount.nfs4 "Failed to
        // resolve server", found live on runn 2026-07-06). A Service
        // ClusterIP is stable for the Service's lifetime — the same
        // argument that lets kernel clients cache the per-pod DS
        // Service IPs from GETDEVICEINFO — and the RWX-NFS path already
        // publishes raw IPs for the same reason. Unresolvable names
        // (dev rigs mounting from inside a VM, e.g. host.lima.internal)
        // pass through unchanged.
        let host = resolve_mount_host(&host);
        let nfs_port = if resp.nfs_port > 0 {
            resp.nfs_port.to_string()
        } else {
            "2049".to_string()
        };

        let mut ctx = HashMap::new();
        ctx.insert(ctx_keys::MDS_IP.into(), host);
        ctx.insert(ctx_keys::MDS_PORT.into(), nfs_port);
        ctx.insert(ctx_keys::EXPORT_PATH.into(), resp.export_path);
        ctx.insert(ctx_keys::VOLUME_FILE.into(), resp.volume_file);
        ctx.insert(ctx_keys::SIZE_BYTES.into(), size_bytes.to_string());
        ctx.insert(
            ctx_keys::VOLUME_MODE.into(),
            if resp.directory { "dir" } else { "file" }.into(),
        );
        // Stamp the EFFECTIVE geometry, not what was asked for — they are
        // equal by now (check_echo above would have failed otherwise), and
        // recording the server's answer is what makes the PV a truthful
        // record of the volume rather than of the request.
        if resp.effective_stripe_size != 0 {
            ctx.insert(
                ctx_keys::STRIPE_SIZE.into(),
                resp.effective_stripe_size.to_string(),
            );
            ctx.insert(
                ctx_keys::STRIPE_WIDTH.into(),
                resp.effective_stripe_width.to_string(),
            );
        }
        // The class discriminator, present only for block-class volumes
        // (absent = files layout, which keeps every pre-existing PV
        // truthful without rewriting). Guarded by the echo check above,
        // so what lands here is what the MDS actually recorded.
        if opts.layout_class == crate::pnfs::mds::layout::LayoutClass::Scsi {
            ctx.insert(ctx_keys::LAYOUT.into(), "block".into());
        }
        Ok(ctx)
    }

    /// Tear down a pNFS volume. Idempotent on the MDS side — deleting
    /// an absent volume returns success.
    pub async fn delete_volume(&self, volume_id: &str) -> Result<(), PnfsError> {
        let mut client = self.dial().await?;
        let resp = client
            .delete_volume(DeleteVolumeRequest {
                volume_id: volume_id.to_string(),
            })
            .await
            .map_err(|e| PnfsError::Transport(format!("DeleteVolume: {}", e)))?
            .into_inner();

        if !resp.deleted {
            return Err(PnfsError::Mds(if resp.message.is_empty() {
                "MDS rejected DeleteVolume (no message)".into()
            } else {
                resp.message
            }));
        }
        Ok(())
    }

    /// Grow a pNFS volume's recorded capacity.
    ///
    /// Directory volumes hold no per-volume size on the MDS — capacity
    /// is pool-side at the data servers — so this is a metadata-only
    /// acknowledgement. It exists anyway because refusing the CSI
    /// ControllerExpandVolume call leaves the PVC permanently in
    /// `Resizing` with a FAILED_PRECONDITION event, for an operation
    /// that had nothing to do in the first place. Legacy sparse-file
    /// volumes are grown in place by the MDS.
    ///
    /// Returns the size the MDS now records.
    pub async fn expand_volume(
        &self,
        volume_id: &str,
        size_bytes: u64,
    ) -> Result<u64, PnfsError> {
        let mut client = self.dial().await?;
        let resp = client
            .expand_volume(crate::pnfs::grpc::ExpandVolumeRequest {
                volume_id: volume_id.to_string(),
                size_bytes,
            })
            .await
            .map_err(|e| PnfsError::Transport(format!("ExpandVolume: {}", e)))?
            .into_inner();

        if !resp.expanded {
            return Err(PnfsError::Mds(if resp.message.is_empty() {
                "MDS rejected ExpandVolume (no message)".into()
            } else {
                resp.message
            }));
        }
        Ok(resp.size_bytes)
    }

    /// Per-node host admission for a block-class volume
    /// (ControllerPublish). The MDS admits the node's NQN onto the
    /// export allow-list and returns the nvme session coordinates the
    /// node stages with. An MDS predating the verb answers
    /// UNIMPLEMENTED, which surfaces here as a Transport error — the
    /// attach fails loudly instead of the node connecting into a
    /// refusal it cannot diagnose.
    pub async fn attach_block_node(
        &self,
        volume_id: &str,
        node_name: &str,
    ) -> Result<BlockAttach, PnfsError> {
        let mut client = self.dial().await?;
        let resp = client
            .attach_block_node(crate::pnfs::grpc::AttachBlockNodeRequest {
                volume_id: volume_id.to_string(),
                node_name: node_name.to_string(),
            })
            .await
            .map_err(|e| PnfsError::Transport(format!("AttachBlockNode: {}", e)))?
            .into_inner();
        if !resp.attached {
            return Err(PnfsError::Mds(if resp.message.is_empty() {
                "MDS rejected AttachBlockNode (no message)".into()
            } else {
                resp.message
            }));
        }
        // A converged attach with empty coordinates cannot be staged —
        // refuse here, where the message still names the cause.
        if resp.traddr.is_empty()
            || resp.trsvcid == 0
            || resp.trsvcid > u16::MAX as u32
            || resp.subnqn.is_empty()
            || resp.nguid.is_empty()
            || resp.host_nqn.is_empty()
        {
            return Err(PnfsError::Mds(format!(
                "AttachBlockNode answered attached=true with unusable session \
                 coordinates (traddr={:?} trsvcid={} subnqn={:?}) — MDS bug or \
                 version skew",
                resp.traddr, resp.trsvcid, resp.subnqn,
            )));
        }
        Ok(BlockAttach {
            traddr: resp.traddr,
            trsvcid: resp.trsvcid as u16,
            subnqn: resp.subnqn,
            nguid: resp.nguid,
            host_nqn: resp.host_nqn,
        })
    }

    /// The inverse (ControllerUnpublish). Idempotent on the MDS.
    pub async fn detach_block_node(
        &self,
        volume_id: &str,
        node_name: &str,
    ) -> Result<String, PnfsError> {
        let mut client = self.dial().await?;
        let resp = client
            .detach_block_node(crate::pnfs::grpc::DetachBlockNodeRequest {
                volume_id: volume_id.to_string(),
                node_name: node_name.to_string(),
            })
            .await
            .map_err(|e| PnfsError::Transport(format!("DetachBlockNode: {}", e)))?
            .into_inner();
        if !resp.detached {
            return Err(PnfsError::Mds(if resp.message.is_empty() {
                "MDS rejected DetachBlockNode (no message)".into()
            } else {
                resp.message
            }));
        }
        Ok(resp.message)
    }
}

/// nvme-tcp session coordinates for a staged block volume, as the MDS
/// answered them. The producer half of the `pnfs.flint.io/nvme-*`
/// publish-context keys (`block_ctx_keys`); `from_publish_context` is
/// the consumer half, used by NodeStage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockAttach {
    pub traddr: String,
    pub trsvcid: u16,
    pub subnqn: String,
    pub nguid: String,
    pub host_nqn: String,
}

/// Publish-context keys ControllerPublish stamps for block volumes and
/// NodeStage reads back. Distinct from `ctx_keys` (volume_context,
/// stamped at provision): these are per-(volume, node) session facts
/// that only exist once the attach admitted the node.
pub mod block_ctx_keys {
    pub const TRADDR: &str = "pnfs.flint.io/nvme-traddr";
    pub const TRSVCID: &str = "pnfs.flint.io/nvme-trsvcid";
    pub const SUBNQN: &str = "pnfs.flint.io/nvme-subnqn";
    pub const NGUID: &str = "pnfs.flint.io/nvme-nguid";
    pub const HOST_NQN: &str = "pnfs.flint.io/nvme-host-nqn";
}

impl BlockAttach {
    /// Stamp into a publish_context map (ControllerPublish).
    pub fn stamp(&self, ctx: &mut HashMap<String, String>) {
        ctx.insert(block_ctx_keys::TRADDR.into(), self.traddr.clone());
        ctx.insert(block_ctx_keys::TRSVCID.into(), self.trsvcid.to_string());
        ctx.insert(block_ctx_keys::SUBNQN.into(), self.subnqn.clone());
        ctx.insert(block_ctx_keys::NGUID.into(), self.nguid.clone());
        ctx.insert(block_ctx_keys::HOST_NQN.into(), self.host_nqn.clone());
    }

    /// Read back from NodeStage's publish_context. `None` when the keys
    /// are absent (a files-class volume, or an attach that predates the
    /// stamp); `Err` when they are present but unusable (a truncated or
    /// hand-edited VolumeAttachment) — staging must fail loudly then,
    /// not silently skip the session and degrade every I/O to MDS
    /// proxying.
    pub fn from_publish_context(
        ctx: &HashMap<String, String>,
    ) -> Result<Option<Self>, String> {
        if !ctx.contains_key(block_ctx_keys::SUBNQN) {
            return Ok(None);
        }
        let get = |key: &str| -> Result<String, String> {
            match ctx.get(key).map(|s| s.trim()) {
                Some(v) if !v.is_empty() => Ok(v.to_string()),
                _ => Err(format!("publish_context is missing {}", key)),
            }
        };
        let trsvcid: u16 = get(block_ctx_keys::TRSVCID)?
            .parse()
            .map_err(|_| format!("{} is not a port number", block_ctx_keys::TRSVCID))?;
        Ok(Some(Self {
            traddr: get(block_ctx_keys::TRADDR)?,
            trsvcid,
            subnqn: get(block_ctx_keys::SUBNQN)?,
            nguid: get(block_ctx_keys::NGUID)?,
            host_nqn: get(block_ctx_keys::HOST_NQN)?,
        }))
    }
}

/// The MDS shard set (mds-sharding-plan.md Phase 1).
///
/// N independent MDSes; each volume is pinned to one shard at
/// CreateVolume and carries the pin in its volume_id forever
/// (`<name>~m<shard>`). Routing is therefore stateless: create picks
/// by hash of the CSI name, everything else parses the suffix.
///
/// Hash-of-name, NOT least-loaded, and deliberately so: the CSI
/// provisioner retries CreateVolume by name, and a load-based pick
/// could choose a different shard on retry — provisioning the volume
/// twice on two shards. hash(name) % N is retry-stable with zero
/// cross-shard state. (N changing between an attempt and its retry is
/// a helm-upgrade-mid-provision race we accept and document.)
#[derive(Clone, Debug)]
pub struct PnfsShards {
    shards: Vec<PnfsCsi>,
}

/// The shard-pin marker in CSI volume_ids. `~` cannot appear in the
/// K8s-generated `pvc-<uuid>` names the provisioner sends, so the
/// suffix is unambiguous; a volume_id WITHOUT it is a pre-sharding
/// volume, which by upgrade construction lives on shard 0.
const SHARD_SUFFIX_MARK: &str = "~m";

impl PnfsShards {
    /// Construct from the environment.
    ///
    /// * `FLINT_PNFS_MDS_SHARD_ENDPOINTS` — comma-separated, ordered:
    ///   index = shard id. Rendered by the chart from
    ///   `pnfs.server.mds.count`.
    /// * `FLINT_PNFS_MDS_ENDPOINT` — legacy single-MDS var; used as a
    ///   one-shard set when the list is absent (older charts).
    ///
    /// `None` ⇒ pNFS is not enabled on this driver.
    pub fn from_env() -> Option<Self> {
        if let Ok(raw) = std::env::var("FLINT_PNFS_MDS_SHARD_ENDPOINTS") {
            let eps: Vec<&str> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if !eps.is_empty() {
                return Some(Self {
                    shards: eps.into_iter().map(PnfsCsi::new).collect(),
                });
            }
        }
        PnfsCsi::from_env().map(|single| Self { shards: vec![single] })
    }

    /// Test/direct constructor.
    pub fn new(endpoints: Vec<String>) -> Self {
        assert!(!endpoints.is_empty(), "PnfsShards needs >= 1 endpoint");
        Self { shards: endpoints.into_iter().map(PnfsCsi::new).collect() }
    }

    pub fn count(&self) -> usize {
        self.shards.len()
    }

    /// Pick the shard for a NEW volume: FNV-1a of the CSI name mod N.
    pub fn pick_for_create(&self, name: &str) -> (usize, &PnfsCsi) {
        let shard = (fnv1a(name) % self.shards.len() as u64) as usize;
        (shard, &self.shards[shard])
    }

    /// Stamp the shard pin into the CSI volume_id returned for `name`.
    pub fn shard_volume_id(&self, name: &str, shard: usize) -> String {
        format!("{}{}{}", name, SHARD_SUFFIX_MARK, shard)
    }

    /// Route an existing volume_id to its owning shard. Returns
    /// (shard, client, bare MDS-side volume name). No suffix ⇒
    /// shard 0 (pre-sharding volume).
    pub fn route<'a, 'b>(
        &'a self,
        volume_id: &'b str,
    ) -> Result<(usize, &'a PnfsCsi, &'b str), PnfsError> {
        let (bare, shard) = match parse_shard_suffix(volume_id) {
            Some((bare, shard)) => (bare, shard),
            None => (volume_id, 0),
        };
        let client = self.shards.get(shard).ok_or_else(|| {
            PnfsError::ShardRouting(format!(
                "volume {} is pinned to MDS shard {} but only {} shard(s) are configured — \
                 was pnfs.server.mds.count scaled below a shard that still owns volumes?",
                volume_id,
                shard,
                self.shards.len()
            ))
        })?;
        Ok((shard, client, bare))
    }

    /// Endpoint string of shard `i` (logging).
    pub fn endpoint_of(&self, shard: usize) -> &str {
        self.shards[shard].endpoint()
    }
}

/// Parse `<bare>~m<shard>` — `None` for pre-sharding ids. Only a
/// trailing, all-digits suffix counts; anything else is part of the
/// name.
pub fn parse_shard_suffix(volume_id: &str) -> Option<(&str, usize)> {
    let at = volume_id.rfind(SHARD_SUFFIX_MARK)?;
    let digits = &volume_id[at + SHARD_SUFFIX_MARK.len()..];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((&volume_id[..at], digits.parse().ok()?))
}

/// FNV-1a, dependency-free and stable across releases — the shard pick
/// for a given name must never change under us.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Resolve `host` to a dotted-quad IPv4 string for the kernel NFS
/// mount line. Already-numeric hosts and names the local resolver
/// can't answer (VM-only rig names) are returned unchanged — the
/// caller stamps the result into the PV, so "best name we have" is
/// the right degradation.
fn resolve_mount_host(host: &str) -> String {
    use std::net::ToSocketAddrs;
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host.to_string();
    }
    match (host, 0u16).to_socket_addrs() {
        Ok(addrs) => addrs
            .filter(|a| a.is_ipv4())
            .map(|a| a.ip().to_string())
            .next()
            .unwrap_or_else(|| host.to_string()),
        Err(_) => host.to_string(),
    }
}

/// Pull host + port from a `http(s)://host:port` URI. The MDS gRPC
/// surface is plain HTTP/2 today; the parse is straightforward but
/// kept in a helper for reuse + testability.
fn parse_host_port(endpoint: &str) -> Result<(String, String), PnfsError> {
    let after_scheme = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .ok_or_else(|| PnfsError::BadEndpoint(format!("missing scheme: {}", endpoint)))?;
    // Stop at the first '/' so a trailing path doesn't end up in the
    // port. The MDS endpoint never has a path today, but defending
    // against it is free.
    let host_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    let (h, p) = host_port
        .rsplit_once(':')
        .ok_or_else(|| PnfsError::BadEndpoint(format!("missing port: {}", endpoint)))?;
    if h.is_empty() || p.is_empty() {
        return Err(PnfsError::BadEndpoint(format!("empty host or port: {}", endpoint)));
    }
    Ok((h.into(), p.into()))
}

/// Build the option string for the kernel NFS mount that backs a pNFS PV.
///
/// `user_flags` are the operator's `mount_flags` (StorageClass
/// `mountOptions` → PV `spec.mountOptions` → `VolumeCapability`). They are
/// appended LAST so they win the kernel's last-one-wins parse, and if they
/// pin a protocol version the built-in default is omitted entirely —
/// emitting both spellings is a conflicting-option mount failure, not a
/// silent override.
///
/// The default is `minorversion=2`. pNFS needs at least 4.1 (the kernel
/// won't issue LAYOUTGET on 4.0) but nothing needs the CEILING at 4.1,
/// where this used to be pinned. That pin was load-bearing only by
/// accident: until the opcode/minor-version gate landed server-side, the
/// mount option was the ONLY thing keeping 4.2 opcodes away from the MDS,
/// so the safety of the pNFS path rested on a Linux client convention.
/// 4.2 against the MDS was then measured (2026-08-01, `b903e43`):
/// COPY/CLONE/SEEK/ALLOCATE on a striped file each answer NFS4ERR_NOTSUPP
/// exactly once — 2 packets, no retry storm, no mount-wide capability
/// loss — and every client-side fallback produced correct bytes. The RWX
/// path has mounted `vers=4.2` all along; this stops pNFS volumes being a
/// minor version behind it.
///
/// The rest: `nconnect=4` (the knob that turned the bench-sweep 1.6x win
/// into the steady-state result), `noresvport` (needed when running
/// unprivileged, matching the rwx_nfs path), and `rsize=wsize=1M` — which
/// we ASK for and do not get.
///
/// The mount actually negotiates `rsize=1047672,wsize=1047532` (measured
/// on a real 6.1 client, runaw 2026-08-01). The shortfalls are exactly 904
/// and 1044 bytes: Linux's `nfs41_maxread_overhead` /
/// `nfs41_maxwrite_overhead`, which `nfs4_session_set_rwsize()` subtracts
/// from the session fore-channel limits the SERVER advertised. Those come
/// from `SERVER_MAX_REQUEST` / `SERVER_MAX_RESPONSE` in
/// `nfs::v4::operations::session`, both pinned at exactly `1024 * 1024`,
/// so the client can never reach the 1 MiB this string requests.
///
/// The cost is a doubled RPC count: every 1 MiB O_DIRECT I/O splits into
/// one full-size RPC plus a runt, and on writes that runt is a sub-page,
/// non-page-aligned tail. Fixing it means raising those two session
/// constants above 1 MiB, which changes buffer sizing for EVERY NFS
/// client and not just pNFS — so it is deliberately not done here. See
/// `mount_opts_tests::the_session_cap_holds_the_client_below_the_rsize_we_ask_for`,
/// which pins the arithmetic so the fix can be verified when it lands.
/// An operator option REPLACES the driver default in the same family rather
/// than joining it: the assembled string never contains a family twice, so
/// nothing here depends on the kernel's option precedence.
///
/// It used to. Only the version family and `sec=` suppressed their default;
/// everything else was emitted unconditionally and the override was left to
/// "the kernel takes the last one" — a claim written down as fact and never
/// measured. On runax (2026-08-02) a class carrying `nconnect=16`
/// propagated correctly to `PV.spec.mountOptions` and the kernel still
/// mounted `nconnect=4`. The two options that worked were exactly the two
/// that never produced a duplicate, which is why this now eliminates
/// duplicates instead of reasoning about who wins.
///
/// `sec=sys` remains a DEFAULT, not a forced option. It is load-bearing —
/// without it the client negotiates AUTH_NONE (this server's SECINFO lists
/// it first), `Auth::unix_uid_gid` returns None, the creator-ownership
/// stamp in OPEN/CREATE is skipped, and every file lands owned by the
/// server process (root); ownership-sensitive workloads such as postgres
/// then refuse to start. But an operator asking for `sec=krb5` means it.
///
/// NOTE THE REMAINING LIMIT, which this function cannot fix: `nconnect` is
/// a property of the client's shared `nfs_client`, and every pNFS PVC on a
/// node mounts the same MDS ip:port. A second mount to a server the node
/// already talks to may inherit the first mount's connection count no
/// matter what this string says. Verify with `/proc/mounts`, never by
/// reading the StorageClass.
pub fn build_pnfs_mount_opts(mds_port: &str, readonly: bool, user_flags: &[String]) -> String {
    let defaults: Vec<String> = vec![
        "minorversion=2".to_string(),
        "sec=sys".to_string(),
        "proto=tcp".to_string(),
        format!("port={}", mds_port),
        "nconnect=4".to_string(),
        "rsize=1048576".to_string(),
        "wsize=1048576".to_string(),
        "noresvport".to_string(),
    ];
    // `ro` is FORCED, not a default: a read-only publish is a CSI decision.
    // It used to be emitted before the operator's flags, so under
    // last-one-wins an operator `rw` would have silently defeated it.
    let forced: Vec<String> = if readonly {
        vec!["ro".to_string()]
    } else {
        Vec::new()
    };
    crate::mount_opts::merge(&defaults, user_flags, &forced)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::pnfs::grpc::{
        CreateVolumeResponse, DeleteVolumeResponse,
        ExpandVolumeRequest, ExpandVolumeResponse,
        MdsControl, MdsControlServer,
        RegisterRequest, RegisterResponse,
        HeartbeatRequest, HeartbeatResponse, CapacityUpdate, CapacityResponse,
        UnregisterRequest, UnregisterResponse,
    };
    use std::sync::Mutex;
    use tokio::net::TcpListener;
    use tonic::{transport::Server, Request, Response, Status};

    /// Minimal MdsControl server for tests. Only `create_volume` and
    /// `delete_volume` are interesting; the DS-management verbs are
    /// stubbed because tonic requires the full trait surface.
    struct MockMds {
        canned_create: Mutex<Option<CreateVolumeResponse>>,
        canned_delete: Mutex<Option<DeleteVolumeResponse>>,
        canned_expand: Mutex<Option<ExpandVolumeResponse>>,
        canned_attach: Mutex<Option<crate::pnfs::grpc::AttachBlockNodeResponse>>,
        last_create_volume_id: Mutex<Option<String>>,
        last_delete_volume_id: Mutex<Option<String>>,
        last_create_request: Mutex<Option<CreateVolumeRequest>>,
        last_attach: Mutex<Option<(String, String)>>,
        last_detach: Mutex<Option<(String, String)>>,
    }

    impl MockMds {
        fn new(create: CreateVolumeResponse, delete: DeleteVolumeResponse) -> Self {
            Self {
                canned_create: Mutex::new(Some(create)),
                canned_delete: Mutex::new(Some(delete)),
                canned_expand: Mutex::new(None),
                canned_attach: Mutex::new(None),
                last_create_request: Mutex::new(None),
                last_create_volume_id: Mutex::new(None),
                last_delete_volume_id: Mutex::new(None),
                last_attach: Mutex::new(None),
                last_detach: Mutex::new(None),
            }
        }
    }

    #[tonic::async_trait]
    impl MdsControl for MockMds {
        async fn register_data_server(
            &self, _: Request<RegisterRequest>,
        ) -> Result<Response<RegisterResponse>, Status> {
            unimplemented!("not exercised in pnfs_csi tests")
        }
        async fn heartbeat(
            &self, _: Request<HeartbeatRequest>,
        ) -> Result<Response<HeartbeatResponse>, Status> {
            unimplemented!()
        }
        async fn update_capacity(
            &self, _: Request<CapacityUpdate>,
        ) -> Result<Response<CapacityResponse>, Status> {
            unimplemented!()
        }
        async fn unregister_data_server(
            &self, _: Request<UnregisterRequest>,
        ) -> Result<Response<UnregisterResponse>, Status> {
            unimplemented!()
        }
        async fn expand_volume(
            &self, req: Request<ExpandVolumeRequest>,
        ) -> Result<Response<ExpandVolumeResponse>, Status> {
            let req = req.into_inner();
            let canned = self.canned_expand.lock().unwrap().clone();
            Ok(Response::new(canned.unwrap_or(ExpandVolumeResponse {
                expanded: true,
                size_bytes: req.size_bytes,
                message: String::new(),
            })))
        }
        async fn create_volume(
            &self, req: Request<CreateVolumeRequest>,
        ) -> Result<Response<CreateVolumeResponse>, Status> {
            let req = req.into_inner();
            *self.last_create_request.lock().unwrap() = Some(req.clone());
            *self.last_create_volume_id.lock().unwrap() = Some(req.volume_id);
            let canned = self.canned_create.lock().unwrap().clone()
                .expect("canned_create not set");
            Ok(Response::new(canned))
        }
        async fn delete_volume(
            &self, req: Request<DeleteVolumeRequest>,
        ) -> Result<Response<DeleteVolumeResponse>, Status> {
            *self.last_delete_volume_id.lock().unwrap() = Some(req.into_inner().volume_id);
            let canned = self.canned_delete.lock().unwrap().clone()
                .expect("canned_delete not set");
            Ok(Response::new(canned))
        }
        async fn fence_block_client(
            &self, _: Request<crate::pnfs::grpc::FenceBlockClientRequest>,
        ) -> Result<Response<crate::pnfs::grpc::FenceBlockClientResponse>, Status> {
            unimplemented!("not exercised in pnfs_csi tests")
        }
        async fn unfence_block_client(
            &self, _: Request<crate::pnfs::grpc::UnfenceBlockClientRequest>,
        ) -> Result<Response<crate::pnfs::grpc::UnfenceBlockClientResponse>, Status> {
            unimplemented!("not exercised in pnfs_csi tests")
        }
        async fn attach_block_node(
            &self, req: Request<crate::pnfs::grpc::AttachBlockNodeRequest>,
        ) -> Result<Response<crate::pnfs::grpc::AttachBlockNodeResponse>, Status> {
            let req = req.into_inner();
            *self.last_attach.lock().unwrap() = Some((req.volume_id, req.node_name));
            let canned = self.canned_attach.lock().unwrap().clone()
                .expect("canned_attach not set");
            Ok(Response::new(canned))
        }
        async fn detach_block_node(
            &self, req: Request<crate::pnfs::grpc::DetachBlockNodeRequest>,
        ) -> Result<Response<crate::pnfs::grpc::DetachBlockNodeResponse>, Status> {
            let req = req.into_inner();
            *self.last_detach.lock().unwrap() = Some((req.volume_id, req.node_name));
            Ok(Response::new(crate::pnfs::grpc::DetachBlockNodeResponse {
                detached: true,
                message: String::new(),
            }))
        }
    }

    /// Spin up a tonic server on an ephemeral port and return the
    /// `host:port` string the test should hand to `PnfsCsi::new`.
    /// The server task runs in the background and is dropped when the
    /// test exits.
    async fn start_mock_mds(mock: std::sync::Arc<MockMds>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // SAFETY: each test owns its own listener; we leak the spawn
        // handle on purpose since #[tokio::test] tears down the
        // runtime on return.
        let svc = MdsControlServer::from_arc(mock);
        tokio::spawn(async move {
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            let _ = Server::builder()
                .add_service(svc)
                .serve_with_incoming(incoming)
                .await;
        });
        // Give the server a tick to start accepting.
        tokio::time::sleep(Duration::from_millis(50)).await;
        format!("127.0.0.1:{}", addr.port())
    }

    #[test]
    fn resolve_mount_host_passthrough_and_resolution() {
        // Numeric hosts never touch the resolver.
        assert_eq!(resolve_mount_host("10.0.0.5"), "10.0.0.5");
        // Unresolvable names degrade to themselves (dev-rig names that
        // only resolve inside the client VM).
        assert_eq!(
            resolve_mount_host("definitely-not-a-real-host.invalid"),
            "definitely-not-a-real-host.invalid",
        );
        // A resolvable name becomes an IPv4 literal.
        assert_eq!(resolve_mount_host("localhost"), "127.0.0.1");
    }

    #[tokio::test]
    async fn create_volume_returns_full_context() {
        let mock = std::sync::Arc::new(MockMds::new(
            CreateVolumeResponse {
                created: true,
                export_path: "/srv/pnfs".into(),
                volume_file: "pvc-abc".into(),
                message: String::new(),
                nfs_port: 20490,
                directory: true, effective_stripe_size: 0, effective_stripe_width: 0, effective_layout_class: String::new(), },
            DeleteVolumeResponse { deleted: true, message: String::new() },
        ));
        let addr = start_mock_mds(mock.clone()).await;

        let p = PnfsCsi::new(&addr);
        let ctx = p.create_volume("pvc-abc", 1024 * 1024 * 1024).await
            .expect("create_volume should succeed");

        // The MDS saw the right volume_id.
        assert_eq!(
            mock.last_create_volume_id.lock().unwrap().as_deref(),
            Some("pvc-abc"),
        );
        // The volume_context carries every key the node-publish path
        // needs. If a key is renamed or dropped, this catches it.
        assert_eq!(ctx.get(ctx_keys::MDS_IP).map(String::as_str), Some("127.0.0.1"));
        // The mount port is the MDS's reported NFS port — NOT the gRPC
        // port we dialed (the original bug this test now pins down).
        assert_eq!(ctx.get(ctx_keys::MDS_PORT).map(String::as_str), Some("20490"));
        assert_eq!(ctx.get(ctx_keys::EXPORT_PATH).map(String::as_str), Some("/srv/pnfs"));
        assert_eq!(ctx.get(ctx_keys::VOLUME_FILE).map(String::as_str), Some("pvc-abc"));
        assert_eq!(
            ctx.get(ctx_keys::SIZE_BYTES).map(String::as_str),
            Some(&*format!("{}", 1024 * 1024 * 1024)),
        );
    }

    #[tokio::test]
    async fn create_volume_propagates_mds_error() {
        let mock = std::sync::Arc::new(MockMds::new(
            CreateVolumeResponse {
                created: false,
                export_path: String::new(),
                volume_file: String::new(),
                message: "size mismatch: existing 4096, requested 8192".into(),
                nfs_port: 0,
                directory: false, effective_stripe_size: 0, effective_stripe_width: 0, effective_layout_class: String::new(), },
            DeleteVolumeResponse { deleted: true, message: String::new() },
        ));
        let addr = start_mock_mds(mock).await;
        let p = PnfsCsi::new(&addr);

        let err = p.create_volume("pvc-xyz", 8192).await.unwrap_err();
        match err {
            PnfsError::Mds(m) => assert!(m.contains("size mismatch")),
            other => panic!("expected Mds error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn delete_volume_round_trip() {
        let mock = std::sync::Arc::new(MockMds::new(
            CreateVolumeResponse {
                created: true,
                export_path: "/srv".into(),
                volume_file: "v".into(),
                message: String::new(),
                nfs_port: 2049,
                directory: true, effective_stripe_size: 0, effective_stripe_width: 0, effective_layout_class: String::new(), },
            DeleteVolumeResponse { deleted: true, message: String::new() },
        ));
        let addr = start_mock_mds(mock.clone()).await;
        let p = PnfsCsi::new(&addr);

        p.delete_volume("pvc-todelete").await.expect("delete should succeed");
        assert_eq!(
            mock.last_delete_volume_id.lock().unwrap().as_deref(),
            Some("pvc-todelete"),
        );
    }

    #[tokio::test]
    async fn attach_block_node_round_trips_and_stamps_the_publish_context() {
        let mock = std::sync::Arc::new(MockMds::new(
            CreateVolumeResponse {
                created: true, export_path: "/srv".into(), volume_file: "v".into(),
                message: String::new(), nfs_port: 2049, directory: true,
                effective_stripe_size: 0, effective_stripe_width: 0,
                effective_layout_class: String::new(),
            },
            DeleteVolumeResponse { deleted: true, message: String::new() },
        ));
        *mock.canned_attach.lock().unwrap() = Some(crate::pnfs::grpc::AttachBlockNodeResponse {
            attached: true,
            message: String::new(),
            traddr: "10.0.0.9".into(),
            trsvcid: 4420,
            subnqn: "nqn.2024-11.com.flint:block:pvc-b".into(),
            nguid: "aabbccdd".into(),
            host_nqn: "nqn.2024-11.com.flint:node:w1".into(),
        });
        let addr = start_mock_mds(mock.clone()).await;
        let p = PnfsCsi::new(&addr);

        let attach = p.attach_block_node("pvc-b", "w1").await.expect("attach");
        assert_eq!(
            mock.last_attach.lock().unwrap().as_ref(),
            Some(&("pvc-b".to_string(), "w1".to_string())),
            "the MDS sees the BARE volume id and the node name"
        );

        // Producer → consumer round trip: what ControllerPublish stamps
        // is exactly what NodeStage reads back.
        let mut ctx = HashMap::new();
        attach.stamp(&mut ctx);
        let read = BlockAttach::from_publish_context(&ctx)
            .expect("stamped context must parse")
            .expect("stamped context must classify as block");
        assert_eq!(read, attach);

        // A files-class publish_context (no keys) is None, not an error.
        assert_eq!(BlockAttach::from_publish_context(&HashMap::new()).unwrap(), None);
        // A truncated context (subnqn present, port missing) is an ERROR
        // — staging must fail loudly, not silently skip the session.
        let mut broken = ctx.clone();
        broken.remove(block_ctx_keys::TRSVCID);
        assert!(BlockAttach::from_publish_context(&broken).is_err());

        // Detach round trip.
        p.detach_block_node("pvc-b", "w1").await.expect("detach");
        assert_eq!(
            mock.last_detach.lock().unwrap().as_ref(),
            Some(&("pvc-b".to_string(), "w1".to_string())),
        );
    }

    #[tokio::test]
    async fn attach_with_unusable_coordinates_is_refused_client_side() {
        // attached=true with no traddr — an MDS bug or version skew.
        // The client must refuse HERE, where the message names the
        // cause, instead of letting NodeStage fail on `nvme connect ''`.
        let mock = std::sync::Arc::new(MockMds::new(
            CreateVolumeResponse {
                created: true, export_path: "/srv".into(), volume_file: "v".into(),
                message: String::new(), nfs_port: 2049, directory: true,
                effective_stripe_size: 0, effective_stripe_width: 0,
                effective_layout_class: String::new(),
            },
            DeleteVolumeResponse { deleted: true, message: String::new() },
        ));
        *mock.canned_attach.lock().unwrap() = Some(crate::pnfs::grpc::AttachBlockNodeResponse {
            attached: true,
            message: String::new(),
            traddr: String::new(),
            trsvcid: 0,
            subnqn: String::new(),
            nguid: String::new(),
            host_nqn: String::new(),
        });
        let addr = start_mock_mds(mock).await;
        let err = PnfsCsi::new(&addr).attach_block_node("pvc-b", "w1").await.unwrap_err();
        match err {
            PnfsError::Mds(m) => assert!(m.contains("unusable"), "{m}"),
            other => panic!("expected Mds error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn from_env_handles_missing_and_empty() {
        let key = "FLINT_PNFS_MDS_ENDPOINT";

        // Unset case.
        std::env::remove_var(key);
        assert!(PnfsCsi::from_env().is_none());

        // Empty case.
        std::env::set_var(key, "");
        assert!(PnfsCsi::from_env().is_none());

        // Whitespace-only case.
        std::env::set_var(key, "   ");
        assert!(PnfsCsi::from_env().is_none());

        // Valid case.
        std::env::set_var(key, "mds.example:50051");
        let p = PnfsCsi::from_env().expect("should construct");
        assert_eq!(p.endpoint(), "http://mds.example:50051");
        std::env::remove_var(key);
    }

    #[test]
    fn parse_host_port_round_trip() {
        let cases = [
            ("http://localhost:50051", Some(("localhost", "50051"))),
            ("https://mds.example.com:443", Some(("mds.example.com", "443"))),
            ("http://10.0.0.1:50051/some/path", Some(("10.0.0.1", "50051"))),
            ("localhost:50051", None),         // no scheme
            ("http://no-port", None),           // no port
            ("http://:50051", None),            // empty host
            ("http://host:", None),             // empty port
        ];
        for (input, expect) in cases {
            let got = parse_host_port(input).ok();
            let expected = expect.map(|(h, p)| (h.to_string(), p.to_string()));
            assert_eq!(got, expected, "input: {}", input);
        }
    }

    #[test]
    fn shard_suffix_round_trip_and_rejects() {
        let shards = PnfsShards::new(vec!["h0:1".into(), "h1:1".into(), "h2:1".into()]);
        let id = shards.shard_volume_id("pvc-abc-123", 2);
        assert_eq!(id, "pvc-abc-123~m2");
        assert_eq!(parse_shard_suffix(&id), Some(("pvc-abc-123", 2)));

        // Pre-sharding / non-pin shapes are NOT pins.
        assert_eq!(parse_shard_suffix("pvc-abc-123"), None);
        assert_eq!(parse_shard_suffix("pvc-abc~m"), None); // no digits
        assert_eq!(parse_shard_suffix("pvc-abc~mx1"), None); // non-digit
        assert_eq!(parse_shard_suffix("pvc-abc~m1z"), None); // trailing junk
    }

    #[test]
    fn fnv1a_matches_published_vectors() {
        // The shard pick for a name must never change across releases —
        // an implementation drift would re-route existing names. Pin
        // the hash to the published FNV-1a test vectors.
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a("foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn pick_for_create_stable_and_covers_shards() {
        let shards = PnfsShards::new(vec!["h0:1".into(), "h1:1".into(), "h2:1".into(), "h3:1".into()]);
        let mut seen = std::collections::HashSet::new();
        for i in 0..32 {
            let name = format!("pvc-{i:032x}");
            let (a, _) = shards.pick_for_create(&name);
            let (b, _) = shards.pick_for_create(&name);
            assert_eq!(a, b, "pick must be deterministic");
            seen.insert(a);
        }
        assert!(seen.len() > 1, "32 names must spread over >1 of 4 shards");
    }

    #[test]
    fn route_legacy_to_shard0_and_out_of_range_errors() {
        let shards = PnfsShards::new(vec!["h0:1".into(), "h1:1".into()]);

        // Bare id = pre-sharding volume = shard 0, bare name unchanged.
        let (shard, _, bare) = shards.route("pvc-legacy").unwrap();
        assert_eq!((shard, bare), (0, "pvc-legacy"));

        // Pinned id routes to its shard with the suffix stripped.
        let (shard, _, bare) = shards.route("pvc-new~m1").unwrap();
        assert_eq!((shard, bare), (1, "pvc-new"));

        // A pin beyond the configured set is a configuration error.
        let err = shards.route("pvc-orphan~m7").unwrap_err();
        assert!(matches!(err, PnfsError::ShardRouting(_)), "got: {err}");
    }

    #[test]
    fn create_pick_and_delete_route_agree_on_shard() {
        // The load-bearing invariant of the whole scheme: a volume created
        // on the shard chosen by `pick_for_create` must, via the pin
        // stamped into its volume_id by `shard_volume_id`, `route` back to
        // that SAME shard with the bare name recovered on delete/expand.
        // The controller wires these three calls across separate RPCs
        // (create stamps, delete routes) so no live path ever asserts the
        // composition — if `shard_volume_id` and `parse_shard_suffix`/
        // `route` drifted, volumes would be created on one shard and
        // deleted against another (silent orphans). Prove it over many
        // names against the real FNV pick, not a hardcoded shard.
        let shards = PnfsShards::new(vec![
            "h0:1".into(), "h1:1".into(), "h2:1".into(),
            "h3:1".into(), "h4:1".into(),
        ]);
        for i in 0..256 {
            let name = format!("pvc-{i:040x}");
            let (picked, _) = shards.pick_for_create(&name);
            let pinned = shards.shard_volume_id(&name, picked);
            let (routed, _, bare) = shards.route(&pinned).unwrap();
            assert_eq!(routed, picked, "create/delete shard disagree for {name}");
            assert_eq!(bare, name, "route must recover the bare name for {name}");
        }
    }

    #[tokio::test]
    async fn sharded_ops_route_to_owning_shard() {
        let canned_create = CreateVolumeResponse {
            created: true,
            message: String::new(),
            export_path: "/data/exports".into(),
            volume_file: "pvc-routed".into(),
            nfs_port: 2049,
            directory: true, effective_stripe_size: 0, effective_stripe_width: 0, effective_layout_class: String::new(), };
        let canned_delete = DeleteVolumeResponse { deleted: true, message: String::new() };
        let mock0 = std::sync::Arc::new(MockMds::new(canned_create.clone(), canned_delete.clone()));
        let mock1 = std::sync::Arc::new(MockMds::new(canned_create, canned_delete));
        let ep0 = start_mock_mds(std::sync::Arc::clone(&mock0)).await;
        let ep1 = start_mock_mds(std::sync::Arc::clone(&mock1)).await;
        let shards = PnfsShards::new(vec![ep0, ep1]);

        // Deleting an explicitly shard-1-pinned id must reach shard 1
        // with the BARE name, and never touch shard 0.
        let (shard, client, bare) = shards.route("pvc-routed~m1").unwrap();
        assert_eq!(shard, 1);
        client.delete_volume(bare).await.unwrap();
        assert_eq!(
            mock1.last_delete_volume_id.lock().unwrap().as_deref(),
            Some("pvc-routed"),
        );
        assert!(mock0.last_delete_volume_id.lock().unwrap().is_none());

        // Create through the hash pick lands on exactly the picked
        // shard, with the bare name on the wire.
        let name = "pvc-create-route";
        let (picked, client) = shards.pick_for_create(name);
        client.create_volume(name, 1 << 20).await.unwrap();
        let (m_hit, m_miss) = if picked == 0 { (&mock0, &mock1) } else { (&mock1, &mock0) };
        assert_eq!(
            m_hit.last_create_volume_id.lock().unwrap().as_deref(),
            Some(name),
        );
        assert!(m_miss.last_create_volume_id.lock().unwrap().is_none());
    }
}


#[cfg(test)]
mod mount_opts_tests {
    use super::build_pnfs_mount_opts;

    fn flags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// EVERY OTHER TEST IN THIS MODULE ASSERTS THE STRING WE SEND. None
    /// asserts what the mount actually gets, and that gap is why a 1 MiB
    /// rsize request silently becoming 1047672 survived unnoticed until a
    /// `mount` output was read on a live cluster (runaw, 2026-08-01).
    ///
    /// This pins the arithmetic. The driver asks for rsize=wsize=1 MiB; the
    /// Linux client derives its real rsize/wsize by subtracting its COMPOUND
    /// overhead from the fore-channel caps the SERVER advertises, which we
    /// pin at exactly 1 MiB — so the request is unsatisfiable by
    /// construction and every 1 MiB direct I/O splits into a full RPC plus
    /// a runt.
    ///
    /// This test documents a KNOWN DEFECT rather than a desired property.
    /// When `SERVER_MAX_REQUEST`/`SERVER_MAX_RESPONSE` are raised above
    /// 1 MiB the `assert!(negotiated < requested)` lines below will fail —
    /// that failure is the fix landing, and the assertions should then flip
    /// to `assert_eq!(negotiated, REQUESTED)`. Do not silence it by deleting
    /// it.
    #[test]
    fn the_session_cap_holds_the_client_below_the_rsize_we_ask_for() {
        use crate::nfs::v4::operations::session::{
            LINUX_NFS41_MAXREAD_OVERHEAD, LINUX_NFS41_MAXWRITE_OVERHEAD, SERVER_MAX_REQUEST,
            SERVER_MAX_RESPONSE,
        };
        const REQUESTED: u32 = 1024 * 1024;

        // The mount string really does ask for the full 1 MiB.
        let opts = build_pnfs_mount_opts("2049", false, &[]);
        assert!(opts.contains(&format!("rsize={REQUESTED}")), "{opts}");
        assert!(opts.contains(&format!("wsize={REQUESTED}")), "{opts}");

        // What a Linux client will actually settle on, given our caps.
        let negotiated_rsize = SERVER_MAX_RESPONSE - LINUX_NFS41_MAXREAD_OVERHEAD;
        let negotiated_wsize = SERVER_MAX_REQUEST - LINUX_NFS41_MAXWRITE_OVERHEAD;

        // Byte-exact against a real 6.1 client on runaw, 2026-08-01.
        assert_eq!(negotiated_rsize, 1_047_672, "rsize arithmetic drifted");
        assert_eq!(negotiated_wsize, 1_047_532, "wsize arithmetic drifted");

        // The defect itself: we cannot get what we ask for.
        assert!(
            negotiated_rsize < REQUESTED,
            "server cap now permits the requested rsize — flip this test to assert_eq!",
        );
        assert!(
            negotiated_wsize < REQUESTED,
            "server cap now permits the requested wsize — flip this test to assert_eq!",
        );
    }

    /// The default must be NFSv4.2, not 4.1. pNFS volumes were pinned a
    /// minor version behind the RWX path (which has always mounted
    /// `vers=4.2`) for no measured reason — 4.2 against the MDS was
    /// exercised on a striped file and every 4.2 op fell back cleanly.
    #[test]
    fn the_default_mount_is_nfs_v4_2() {
        let opts = build_pnfs_mount_opts("2049", false, &[]);
        assert!(opts.contains("minorversion=2"), "{opts}");
        assert!(!opts.contains("minorversion=1"), "{opts}");
        assert!(opts.contains("port=2049"), "{opts}");
        assert!(opts.contains("nconnect=4"), "{opts}");
    }

    /// AUTH_SYS must be the default. Without `sec=sys` the client negotiates
    /// AUTH_NONE (this server's SECINFO lists AUTH_NONE first), no uid reaches
    /// the server, and every created file lands owned by root — the exact
    /// failure the RWX mount path documents and avoids.
    #[test]
    fn the_default_mount_requests_auth_sys() {
        let opts = build_pnfs_mount_opts("2049", false, &[]);
        assert!(opts.split(',').any(|o| o == "sec=sys"), "{opts}");
    }

    /// An operator asking for Kerberos must not also get sec=sys — the
    /// kernel takes the last one, but emitting both is a lie about intent
    /// and masks a typo in the operator's flavour name.
    #[test]
    fn an_operator_security_flavour_replaces_the_default() {
        for pin in ["sec=krb5", "sec=krb5p", "sec=none"] {
            let opts = build_pnfs_mount_opts("2049", false, &flags(&[pin]));
            assert!(opts.contains(pin), "{pin} missing from {opts}");
            assert!(!opts.contains("sec=sys"), "default survived beside {pin}: {opts}");
        }
    }

    /// A version pin must not suppress the security default, nor vice
    /// versa — they are independent knobs and an early version of this
    /// guard shared one flag between them.
    #[test]
    fn the_version_and_security_defaults_are_independent() {
        let v = build_pnfs_mount_opts("2049", false, &flags(&["vers=4.1"]));
        assert!(v.split(',').any(|o| o == "sec=sys"), "sec dropped by version pin: {v}");
        let s = build_pnfs_mount_opts("2049", false, &flags(&["sec=krb5"]));
        assert!(s.contains("minorversion=2"), "version dropped by sec pin: {s}");
    }

    /// An operator pinning the version must not get BOTH their option and
    /// ours — the kernel rejects conflicting protocol versions, so emitting
    /// both is a mount failure rather than an override.
    #[test]
    fn an_operator_pinned_version_replaces_the_default_rather_than_joining_it() {
        for pin in ["vers=4.1", "nfsvers=4.2", "minorversion=1"] {
            let opts = build_pnfs_mount_opts("2049", false, &flags(&[pin]));
            assert!(opts.contains(pin), "{pin} missing from {opts}");
            assert!(
                !opts.contains("minorversion=2") || pin == "minorversion=2",
                "default survived alongside operator pin {pin}: {opts}"
            );
        }
    }

    /// A pin inside a comma-joined flag string counts too — kubelet passes
    /// PV `spec.mountOptions` through as separate entries, but nothing stops
    /// an operator writing one entry containing several options.
    #[test]
    fn a_version_pin_inside_a_comma_joined_flag_is_detected() {
        let opts = build_pnfs_mount_opts("2049", false, &flags(&["hard,vers=4.1,timeo=600"]));
        assert!(!opts.contains("minorversion=2"), "{opts}");
        assert!(opts.contains("vers=4.1"), "{opts}");
    }

    /// (operator spelling, the driver default it must REMOVE)
    ///
    /// This replaces `operator_options_are_appended_after_the_defaults`,
    /// which asserted only that the operator's option came LAST in our own
    /// string. That test passed for the entire life of the defect: it was
    /// checking the driver's string concatenation, and was structurally
    /// incapable of failing when the kernel ignored the second occurrence —
    /// which is exactly what shipped.
    const OVERRIDE_CASES: &[(&str, &str)] = &[
        ("vers=4.1", "minorversion=2"),
        ("nfsvers=4.1", "minorversion=2"),
        ("minorversion=1", "minorversion=2"),
        ("sec=krb5", "sec=sys"),
        ("proto=rdma", "proto=tcp"),
        ("port=20490", "port=2049"),
        ("nconnect=16", "nconnect=4"),
        ("rsize=262144", "rsize=1048576"),
        ("wsize=262144", "wsize=1048576"),
        ("resvport", "noresvport"),
    ];

    /// THE TEST THAT WOULD HAVE CAUGHT THE DEFECT. Against the pre-fix code
    /// it fails on 6 of these 10 rows — proto, port, nconnect, rsize, wsize,
    /// noresvport — and passes on the 4 version/sec rows, reproducing
    /// exactly the asymmetry measured on runax.
    #[test]
    fn every_driver_default_is_replaceable_by_an_operator_option() {
        for (theirs, ours) in OVERRIDE_CASES {
            let opts = build_pnfs_mount_opts("2049", false, &flags(&[theirs]));
            assert!(
                opts.split(',').any(|o| o == *theirs),
                "operator option {theirs} missing: {opts}"
            );
            assert!(
                !opts.split(',').any(|o| o == *ours),
                "driver default {ours} survived beside operator {theirs}: {opts}"
            );
        }
    }

    /// Special-casing is how this bug was born — `pinned()` covered exactly
    /// two options and every other default was unconditional. This makes an
    /// uncovered default a build failure instead of a cluster discovery.
    #[test]
    fn the_override_cases_cover_every_default_the_driver_emits() {
        use std::collections::HashSet;
        let covered: HashSet<&str> = OVERRIDE_CASES.iter().map(|(_, ours)| *ours).collect();
        for opt in build_pnfs_mount_opts("2049", false, &[]).split(',') {
            assert!(
                covered.contains(opt),
                "driver default `{opt}` has no override case — add one to OVERRIDE_CASES"
            );
        }
    }

    /// The invariant that makes the fix independent of kernel precedence.
    #[test]
    fn the_assembled_string_never_repeats_an_option_family() {
        use std::collections::HashSet;
        let cases = [
            flags(&[]),
            flags(&["nconnect=16"]),
            flags(&["nconnect=8", "nconnect=16"]),
            flags(&["hard,vers=4.1,timeo=600,nconnect=16"]),
            flags(&["rw"]),
            flags(&["tcp"]),
            flags(&["resvport"]),
            flags(&["", "  ", "hard"]),
        ];
        for readonly in [false, true] {
            for f in &cases {
                let opts = build_pnfs_mount_opts("2049", readonly, f);
                let mut seen = HashSet::new();
                for o in opts.split(',') {
                    let fam = crate::mount_opts::family_of(o);
                    assert!(seen.insert(fam), "family `{fam}` emitted twice in {opts}");
                }
            }
        }
    }

    /// No existing volume's mount may change. pNFS PVs remount on every pod
    /// restart, so a refactor that shifted the default string would retune
    /// the whole fleet silently.
    #[test]
    fn the_default_string_is_byte_identical_to_the_pre_merge_driver() {
        assert_eq!(
            build_pnfs_mount_opts("2049", false, &[]),
            "minorversion=2,sec=sys,proto=tcp,port=2049,nconnect=4,rsize=1048576,wsize=1048576,noresvport"
        );
        assert_eq!(
            build_pnfs_mount_opts("2049", true, &[]),
            "minorversion=2,sec=sys,proto=tcp,port=2049,nconnect=4,rsize=1048576,wsize=1048576,noresvport,ro"
        );
    }

    /// The one intentional behaviour change. `ro` used to be emitted BEFORE
    /// the operator's flags, so under last-one-wins an operator `rw` would
    /// have silently defeated a read-only publish.
    #[test]
    fn a_read_only_publish_refuses_an_operator_rw() {
        let opts = build_pnfs_mount_opts("2049", true, &flags(&["rw", "nconnect=16"]));
        assert!(opts.split(',').any(|o| o == "ro"), "{opts}");
        assert!(
            !opts.split(',').any(|o| o == "rw"),
            "operator rw defeated the CSI readOnly publish: {opts}"
        );
        assert!(
            opts.split(',').any(|o| o == "nconnect=16"),
            "refusing rw must not refuse everything else: {opts}"
        );
    }

    #[test]
    fn a_repeated_operator_option_is_emitted_once_with_the_last_value() {
        let opts = build_pnfs_mount_opts("2049", false, &flags(&["nconnect=8", "nconnect=16"]));
        assert!(opts.split(',').any(|o| o == "nconnect=16"), "{opts}");
        assert!(!opts.split(',').any(|o| o == "nconnect=8"), "{opts}");
    }

    #[test]
    fn a_comma_joined_operator_entry_is_split_into_separate_options() {
        let opts = build_pnfs_mount_opts(
            "2049",
            false,
            &flags(&["hard,vers=4.1,timeo=600,nconnect=16"]),
        );
        for want in ["hard", "vers=4.1", "timeo=600", "nconnect=16"] {
            assert!(opts.split(',').any(|o| o == want), "{want} missing: {opts}");
        }
        assert!(!opts.split(',').any(|o| o == "minorversion=2"), "{opts}");
        assert!(!opts.split(',').any(|o| o == "nconnect=4"), "{opts}");
    }

    /// Read-only must survive the presence of operator options.
    #[test]
    fn readonly_is_still_applied_alongside_operator_options() {
        let opts = build_pnfs_mount_opts("2049", true, &flags(&["hard"]));
        assert!(opts.split(',').any(|o| o == "ro"), "{opts}");
        assert!(opts.split(',').any(|o| o == "hard"), "{opts}");
    }

    /// Empty entries must not produce a stray comma — `mount` rejects the
    /// resulting empty option.
    #[test]
    fn empty_operator_entries_do_not_produce_an_empty_option() {
        let opts = build_pnfs_mount_opts("2049", false, &flags(&["", "  ", "hard"]));
        assert!(!opts.split(',').any(|o| o.trim().is_empty()), "{opts}");
    }
}


#[cfg(test)]
mod volume_options_tests {
    use super::*;
    use std::collections::HashMap;

    fn params(kv: &[(&str, &str)]) -> HashMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// No parameters must mean exactly the old behaviour: every field
    /// zero, which the MDS reads as "your defaults".
    #[test]
    fn no_parameters_means_mds_defaults() {
        let o = VolumeOptions::from_parameters(&params(&[("layout", "pnfs")])).unwrap();
        assert_eq!(o, VolumeOptions::default());
    }

    /// A typo in a parameter name must FAIL the provision. Ignoring it
    /// would produce a healthy-looking PVC with none of the geometry the
    /// author asked for — and geometry is fixed at create, so there is
    /// no later point at which the mistake surfaces or can be fixed.
    #[test]
    fn an_unknown_pnfs_parameter_is_refused() {
        let e = VolumeOptions::from_parameters(&params(&[
            ("layout", "pnfs"),
            ("pnfs.flint.io/stripesize", "1048576"), // wrong case
        ]))
        .unwrap_err();
        assert!(e.contains("unknown StorageClass parameter"), "{e}");
        assert!(e.contains("pnfs.flint.io/stripeSize"), "error should list the real names: {e}");
    }

    /// Parameters outside our namespace belong to other provisioners
    /// and to the SPDK path; we must not police them.
    #[test]
    fn parameters_outside_the_pnfs_namespace_are_left_alone() {
        VolumeOptions::from_parameters(&params(&[
            ("layout", "pnfs"),
            ("numReplicas", "3"),
            ("csi.storage.k8s.io/fstype", "ext4"),
        ]))
        .expect("foreign parameters must not be rejected");
    }

    #[test]
    fn stripe_size_must_be_a_power_of_two_in_range() {
        for bad in ["0", "1024", "3145728", "2147483648"] {
            let e = VolumeOptions::from_parameters(&params(&[(sc_params::STRIPE_SIZE, bad)]))
                .unwrap_err();
            assert!(e.contains(sc_params::STRIPE_SIZE), "{bad}: {e}");
        }
        let o = VolumeOptions::from_parameters(&params(&[(sc_params::STRIPE_SIZE, "1048576")]))
            .unwrap();
        assert_eq!(o.stripe_size, 1024 * 1024);
    }

    /// Width 0 is spelled "omit the parameter", not "0" — an explicit 0
    /// most likely means the author thought it meant "unlimited", and
    /// silently accepting it would give them the opposite of a narrow
    /// stripe.
    #[test]
    fn an_explicit_zero_stripe_width_is_refused() {
        let e = VolumeOptions::from_parameters(&params(&[(sc_params::STRIPE_WIDTH, "0")]))
            .unwrap_err();
        assert!(e.contains("omit the parameter"), "{e}");
    }

    /// Modes are octal — "0770" must not be read as seven hundred and
    /// seventy, which would be 0o1402 and nonsense.
    #[test]
    fn dir_mode_is_parsed_as_octal() {
        let o = VolumeOptions::from_parameters(&params(&[
            (sc_params::DIR_MODE, "0770"),
            (sc_params::DIR_GID, "2000"),
        ]))
        .unwrap();
        assert_eq!(o.dir_mode, 0o770);
        assert_eq!(o.dir_gid, 2000);
    }

    /// A mode that denies "other" without a group is unwritable by any
    /// pod — catch it at provision, not at the app's first write.
    #[test]
    fn a_restrictive_mode_without_a_group_is_refused() {
        let e = VolumeOptions::from_parameters(&params(&[(sc_params::DIR_MODE, "0750")]))
            .unwrap_err();
        assert!(e.contains(sc_params::DIR_GID), "{e}");
    }

    /// ...but a mode that still grants "other" is the author's call.
    #[test]
    fn a_permissive_mode_without_a_group_is_allowed() {
        let o = VolumeOptions::from_parameters(&params(&[(sc_params::DIR_MODE, "0775")])).unwrap();
        assert_eq!(o.dir_mode, 0o775);
    }

    /// An MDS too old to know about geometry echoes zeros. Asking for
    /// geometry and getting silence must fail the provision: proto3
    /// drops unknown fields, so the volume would otherwise be built at
    /// the MDS default with nothing anywhere saying so.
    #[test]
    fn an_mds_that_ignores_geometry_fails_the_provision() {
        let o = VolumeOptions { stripe_size: 1 << 20, ..Default::default() };
        let e = o.check_echo(0, 0).unwrap_err();
        assert!(e.contains("does not support per-volume stripe geometry"), "{e}");
    }

    /// An MDS that recorded something OTHER than what was asked is a
    /// worse failure than one that ignored it, and must also fail.
    #[test]
    fn a_geometry_mismatch_fails_the_provision() {
        let o = VolumeOptions { stripe_size: 1 << 20, stripe_width: 2, ..Default::default() };
        assert!(o.check_echo(8 << 20, 2).unwrap_err().contains("stripe size"));
        assert!(o.check_echo(1 << 20, 4).unwrap_err().contains("stripe width"));
        o.check_echo(1 << 20, 2).expect("exact echo must pass");
    }

    /// A volume that asked for NO geometry must not be tripped up by an
    /// old MDS's zero echo — that is the upgrade path for every volume
    /// provisioned before this feature.
    #[test]
    fn asking_for_no_geometry_tolerates_an_old_mds() {
        VolumeOptions::default().check_echo(0, 0).expect("must not fail");
    }
}
