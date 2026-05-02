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
//!   2. Asks the MDS to create a sparse, sized file at
//!      `<export>/<volume_id>`.
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
    CreateVolumeRequest, DeleteVolumeRequest, MdsControlClient,
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
}

impl std::fmt::Display for PnfsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(m) => write!(f, "pNFS transport: {}", m),
            Self::Mds(m) => write!(f, "pNFS MDS: {}", m),
            Self::BadEndpoint(m) => write!(f, "pNFS bad endpoint: {}", m),
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
    /// rationale on the struct doc-comment.
    async fn dial(&self) -> Result<MdsControlClient<tonic::transport::Channel>, PnfsError> {
        let ep = tonic::transport::Endpoint::from_shared(self.endpoint.clone())
            .map_err(|e| PnfsError::BadEndpoint(format!("{}: {}", self.endpoint, e)))?
            .connect_timeout(self.timeout)
            .timeout(self.timeout);
        let channel = ep
            .connect()
            .await
            .map_err(|e| PnfsError::Transport(format!("connect {}: {}", self.endpoint, e)))?;
        Ok(MdsControlClient::new(channel))
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
        let mut client = self.dial().await?;
        let resp = client
            .create_volume(CreateVolumeRequest {
                volume_id: volume_id.to_string(),
                size_bytes,
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

        // Split the endpoint back into host and port for the
        // node-publish mount line. `endpoint` always carries a scheme
        // by construction.
        let (host, port) = parse_host_port(&self.endpoint)?;

        let mut ctx = HashMap::new();
        ctx.insert(ctx_keys::MDS_IP.into(), host);
        ctx.insert(ctx_keys::MDS_PORT.into(), port);
        ctx.insert(ctx_keys::EXPORT_PATH.into(), resp.export_path);
        ctx.insert(ctx_keys::VOLUME_FILE.into(), resp.volume_file);
        ctx.insert(ctx_keys::SIZE_BYTES.into(), size_bytes.to_string());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pnfs::grpc::{
        CreateVolumeResponse, DeleteVolumeResponse, MdsControl, MdsControlServer,
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
        last_create_volume_id: Mutex<Option<String>>,
        last_delete_volume_id: Mutex<Option<String>>,
    }

    impl MockMds {
        fn new(create: CreateVolumeResponse, delete: DeleteVolumeResponse) -> Self {
            Self {
                canned_create: Mutex::new(Some(create)),
                canned_delete: Mutex::new(Some(delete)),
                last_create_volume_id: Mutex::new(None),
                last_delete_volume_id: Mutex::new(None),
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
        async fn create_volume(
            &self, req: Request<CreateVolumeRequest>,
        ) -> Result<Response<CreateVolumeResponse>, Status> {
            *self.last_create_volume_id.lock().unwrap() = Some(req.into_inner().volume_id);
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

    #[tokio::test]
    async fn create_volume_returns_full_context() {
        let mock = std::sync::Arc::new(MockMds::new(
            CreateVolumeResponse {
                created: true,
                export_path: "/srv/pnfs".into(),
                volume_file: "pvc-abc".into(),
                message: String::new(),
            },
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
        assert!(ctx.get(ctx_keys::MDS_PORT).is_some());
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
            },
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
            },
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
}
