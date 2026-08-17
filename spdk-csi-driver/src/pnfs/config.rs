//! pNFS Configuration
//!
//! This module handles parsing and validation of pNFS configuration from:
//! - YAML files
//! - Environment variables
//! - Kubernetes ConfigMaps (future)

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Top-level pNFS configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnfsConfig {
    /// Server mode
    #[serde(default)]
    pub mode: PnfsMode,

    /// Metadata server configuration (when mode = MDS)
    pub mds: Option<MdsConfig>,

    /// Data server configuration (when mode = DS)
    pub ds: Option<DsConfig>,

    /// NFS export configuration
    #[serde(default)]
    pub exports: Vec<ExportConfig>,

    /// Logging configuration
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Monitoring configuration
    #[serde(default)]
    pub monitoring: MonitoringConfig,
}

/// Server operating mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PnfsMode {
    /// Standalone NFS server (default, current behavior)
    Standalone,

    /// Metadata Server
    #[serde(rename = "mds")]
    MetadataServer,

    /// Data Server
    #[serde(rename = "ds")]
    DataServer,
}

impl Default for PnfsMode {
    fn default() -> Self {
        Self::Standalone
    }
}

/// Metadata Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MdsConfig {
    /// Network binding
    pub bind: BindConfig,

    /// Layout configuration
    pub layout: LayoutConfig,

    /// Data server registry
    #[serde(rename = "dataServers")]
    pub data_servers: Vec<DataServerInfo>,

    /// State persistence
    pub state: StateConfig,

    /// High availability
    #[serde(default)]
    pub ha: HaConfig,

    /// Failover configuration
    #[serde(default)]
    pub failover: FailoverConfig,

    /// F68b: before accepting a DS registration, dial the CLIENT-path
    /// endpoint (the one GETDEVICEINFO will advertise) and NACK if it
    /// does not accept. A device that registers before its endpoint
    /// routes (k8s: per-pod Service has no Ready endpoint yet) becomes
    /// grantable while unreachable — the first client connect fails and
    /// the kernel blacklists the deviceid for 120s
    /// (NFS4_DEVICE_ID_NEG_ENTRY, re-armed per retry), silently
    /// rerouting all I/O through the MDS fallback path.
    ///
    /// Default OFF: only enable where the MDS shares the clients'
    /// network path to the DSes (the k8s chart does — ClusterIP
    /// Services). On the lima rig the MDS cannot resolve
    /// host.lima.internal, so enabling it there would NACK forever.
    #[serde(rename = "verifyDsReachability", default)]
    pub verify_ds_reachability: bool,

    /// Flint-lite: run this MDS as a STANDALONE NFSv4.2 server — the
    /// same binary and state machinery with layouts off. The dispatcher
    /// gets no pNFS handler, so EXCHANGE_ID advertises a non-pNFS
    /// server, LAYOUTGET is refused NotSupp, and every byte serves
    /// through the MDS lane (the DS-proven no-handler path). Set from
    /// YAML directly or implied by top-level `mode: standalone`.
    /// Refuses to coexist with `dataServers` or `blockExport`.
    #[serde(default)]
    pub standalone: bool,

    /// pnfs-block (scsi layout): where this MDS reaches the spdk-tgt
    /// that serves block-class volumes, and the export coordinates it
    /// converges. Absent = this MDS refuses block-class CreateVolume
    /// (a granted client would have no target to connect to — the
    /// refusal names the missing config instead of provisioning a
    /// volume whose every I/O silently needs an MDS that cannot serve
    /// it).
    #[serde(rename = "blockExport", default)]
    pub block_export: Option<BlockExportConfig>,

    /// S3 tier (L2). Present + enabled ⇒ this hub captures mutations
    /// durably, claims the volume epoch at startup (A8 — fencing
    /// before daemon; the claim may WAIT out a dead holder's lease),
    /// and runs the flush pipeline. v1 scope: STANDALONE (flint-lite)
    /// posture ONLY — a pNFS MDS's flusher would publish its sparse
    /// stubs, so serve() refuses the combination.
    #[serde(default)]
    pub tier: Option<TierConfig>,
}

/// S3 tier settings (design of record:
/// docs/plans/s3-tier-l2-design-review.md; knobs per A11 are
/// REQUIREMENTS, not tuning — the defaults are the economics gate's
/// assumptions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierConfig {
    /// Kill switch that keeps the section in the file: `enabled: false`
    /// parses and does nothing.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Bucket name. Credentials/region come from the ambient AWS
    /// environment (IRSA, env vars, profile).
    pub bucket: String,

    /// Key prefix under the bucket (e.g. "vol1/"). `.flint/` under it
    /// is RESERVED for tier control objects (the epoch; manifests) —
    /// client files there are refused tiering.
    #[serde(rename = "keyPrefix", default)]
    pub key_prefix: String,

    /// Custom endpoint (MinIO rigs; forces path-style). None = real S3.
    #[serde(default)]
    pub endpoint: Option<String>,

    /// A11: per-file flush-interval floor — caps a hot file's request
    /// bill. The economics gate priced 60 s.
    #[serde(rename = "flushFloorSecs", default = "default_tier_flush_floor")]
    pub flush_floor_secs: u64,

    /// A11: quiescence guard — files noted more recently are skipped.
    #[serde(rename = "quiesceSecs", default = "default_tier_quiesce")]
    pub quiesce_secs: u64,

    /// Flush loop cadence.
    #[serde(rename = "tickSecs", default = "default_tier_tick")]
    pub tick_secs: u64,

    /// Below this, a generation publishes as ONE conditional PUT.
    #[serde(rename = "wholePutMaxBytes", default = "default_tier_whole_put_max")]
    pub whole_put_max_bytes: u64,

    /// A11 part-grid floor (clamped up to the backend minimum).
    #[serde(rename = "partFloorBytes", default = "default_tier_part_floor")]
    pub part_floor_bytes: u64,

    /// A8: epoch heartbeat interval.
    #[serde(rename = "epochHeartbeatSecs", default = "default_tier_heartbeat")]
    pub epoch_heartbeat_secs: u64,

    /// A8: consecutive missed heartbeats before a successor may judge
    /// this holder dead (lease TTL ≈ heartbeat × misses).
    #[serde(rename = "epochLeaseMisses", default = "default_tier_lease_misses")]
    pub epoch_lease_misses: u32,

    /// A10: admission headroom — WRITE/CREATE answer NFS4ERR_NOSPC
    /// while `avail − reserve` cannot cover them (NOSPC before
    /// hard-full; F55: EIO makes postgres PANIC).
    #[serde(rename = "reserveBytes", default = "default_tier_reserve")]
    pub reserve_bytes: u64,

    /// A10: eviction-trigger watermark, percent used (step 10 consumes
    /// it; today it WARNs).
    #[serde(rename = "watermarkPct", default = "default_tier_watermark")]
    pub watermark_pct: u8,

    /// A10: preallocated ballast next to state.db, released at
    /// critical fullness so the durable bookkeeping keeps committing.
    /// 0 disables. Requires the sqlite backend (memory has no db file
    /// to protect).
    #[serde(rename = "ballastBytes", default = "default_tier_ballast")]
    pub ballast_bytes: u64,

    /// A5: in-RPC park bound while a hydration runs — one DELAY per
    /// hold instead of ten per second (well below timeo).
    #[serde(rename = "hydrateHoldSecs", default = "default_tier_hydrate_hold")]
    pub hydrate_hold_secs: u64,

    /// Concurrent restores (a +1 slot stays reserved for
    /// WRITE-triggered hydrations — step 9's hung-task finding).
    #[serde(rename = "hydrateConcurrency", default = "default_tier_hydrate_concurrency")]
    pub hydrate_concurrency: usize,

    /// Step 12: run import-refresh at startup when the tier state is
    /// FRESH and the bucket holds content (the DR restore / bucket
    /// adopt path), and always to resume a crashed import. `false`
    /// leaves the bucket unimported (flush-only posture).
    #[serde(rename = "importOnStart", default = "default_true")]
    pub import_on_start: bool,
}

fn default_tier_flush_floor() -> u64 {
    60
}
fn default_tier_quiesce() -> u64 {
    10
}
fn default_tier_tick() -> u64 {
    10
}
fn default_tier_whole_put_max() -> u64 {
    64 * 1024 * 1024
}
fn default_tier_part_floor() -> u64 {
    16 * 1024 * 1024
}
fn default_tier_heartbeat() -> u64 {
    10
}
fn default_tier_lease_misses() -> u32 {
    6
}
fn default_tier_reserve() -> u64 {
    256 * 1024 * 1024
}
fn default_tier_watermark() -> u8 {
    85
}
fn default_tier_ballast() -> u64 {
    64 * 1024 * 1024
}
fn default_tier_hydrate_hold() -> u64 {
    15
}
fn default_tier_hydrate_concurrency() -> usize {
    4
}

/// Block-export reconciler settings (design doc §5, phase 1: one tgt per
/// MDS shard — allocation is per-volume inside the volume's own lvol,
/// the volume pins to its shard, so the shard's tgt is the volume's tgt).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockExportConfig {
    /// spdk-tgt JSON-RPC unix socket path (the MDS is colocated with
    /// its tgt in this phase — lima rig and kind tier shapes).
    #[serde(rename = "spdkSocket")]
    pub spdk_socket: String,

    /// lvolstore name backing per-volume lvols (`<lvstore>/<volume>`).
    pub lvstore: String,

    /// Listener address kernel initiators dial (node-reachable, NOT
    /// advertised over NFS — RFC 8154 device addresses carry
    /// designators; clients connect out of band).
    pub traddr: String,

    /// Listener port; 4420 is the NVMe-oF IANA default.
    #[serde(default = "default_nvmf_trsvcid")]
    pub trsvcid: u16,

    /// Directory ON THE TGT HOST for per-namespace reservation PTPL
    /// files. Mandatory: the ≥6.x kernel blocklayout client registers
    /// its reservation key with CPTPL=PERSIST unconditionally, and SPDK
    /// refuses that on a namespace without a ptpl_file — no PTPL, no
    /// client I/O at all (rig-proven). Point at a path that survives
    /// tgt restarts in production; the default suits the lima rig.
    #[serde(rename = "ptplDir", default = "default_ptpl_dir")]
    pub ptpl_dir: String,
}

fn default_nvmf_trsvcid() -> u16 {
    4420
}

fn default_ptpl_dir() -> String {
    "/var/tmp".to_string()
}

/// Data Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DsConfig {
    /// Network binding
    pub bind: BindConfig,

    /// Unique device identifier
    #[serde(rename = "deviceId")]
    pub device_id: String,

    /// MDS to register with
    pub mds: MdsEndpointConfig,

    /// Block devices to serve
    pub bdevs: Vec<BdevConfig>,

    /// Resource limits
    #[serde(default)]
    pub resources: ResourceConfig,

    /// Performance tuning
    #[serde(default)]
    pub performance: PerformanceConfig,
}

/// Network binding configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindConfig {
    pub address: String,
    pub port: u16,

    /// DS only: port for the DsControl gRPC listener (MDS → DS
    /// commands, e.g. synchronous stripe-file truncation). None (or
    /// absent in YAML) disables the listener — size-changing SETATTRs
    /// on striped files then park the file truncate-dirty until the
    /// MDS can reach a listener, so production DS configs should
    /// always set it.
    #[serde(rename = "controlPort", default)]
    pub control_port: Option<u16>,
}

/// Layout configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutConfig {
    /// Layout type: file, block, object
    #[serde(rename = "type")]
    pub layout_type: LayoutType,

    /// Stripe size in bytes
    #[serde(rename = "stripeSize")]
    pub stripe_size: u64,

    /// Layout policy
    pub policy: LayoutPolicy,
}

/// Layout type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayoutType {
    File,
    Block,
    Object,
}

/// Layout policy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayoutPolicy {
    /// Simple round-robin across all DSs
    RoundRobin,

    /// Interleaved striping for parallel I/O
    Stripe,

    /// Prefer DS on same node as client
    Locality,
}

/// Split a comma-separated endpoint list into (primary, extras).
/// Every consumer of `DataServerInfo::endpoint` MUST go through this —
/// registering the raw comma string as a primary endpoint produces an
/// unparseable netaddr4 in GETDEVICEINFO.
pub fn split_endpoint_list(s: &str) -> (String, Vec<String>) {
    let mut parts = s
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from);
    let primary = parts.next().unwrap_or_default();
    (primary, parts.collect())
}

/// Data server information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataServerInfo {
    /// Device ID (unique identifier)
    #[serde(rename = "deviceId")]
    pub device_id: String,

    /// Client-reachable endpoint(s), comma-separated (`host:port` or
    /// DNS names). The first entry is the primary; any further entries
    /// are multipath extras — each becomes an additional netaddr4 in
    /// this DS's GETDEVICEINFO multipath_list4, and the Linux client
    /// opens one trunked transport per extra (they must resolve to
    /// DISTINCT IPs; the kernel dedupes trunk candidates by address).
    /// Equivalent to listing the extras in `multipath`.
    pub endpoint: String,

    /// MDS-reachable DsControl endpoint override ("host:port"). The
    /// default derivation pairs the CLIENT-reachable `endpoint`'s host
    /// with the DS-reported control port — right whenever MDS and
    /// clients share a network path to the DS (k8s per-pod Services),
    /// wrong when they don't (the lima rig: clients reach DSes at
    /// host.lima.internal, the MDS at 127.0.0.1).
    #[serde(rename = "controlEndpoint", default)]
    pub control_endpoint: Option<String>,

    /// Additional endpoints for multipath/RDMA
    #[serde(default)]
    pub multipath: Vec<String>,

    /// Block devices this DS serves
    pub bdevs: Vec<String>,
}

/// State persistence configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateConfig {
    /// Backend type: memory, kubernetes, etcd
    pub backend: StateBackend,

    /// Backend-specific configuration (key-value map)
    #[serde(default)]
    pub config: std::collections::HashMap<String, String>,
}

/// State persistence backend kind for the MDS. Selects which
/// `state_backend::StateBackend` impl `MetadataServer` constructs at
/// startup. The `Memory` variant is the dev/test default; `Sqlite` is
/// what production should use for restart survival (Phase B.2 +
/// B.3 + B.4).
///
/// The previous `Kubernetes` and `Etcd` variants were never wired up
/// (no impls existed in `state_backend/`); B.4 drops them rather than
/// carrying dead config syntax. Operator-visible breaking change is
/// intentional and gated by the schema-version canary in
/// `state_backend::sqlite`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StateBackend {
    /// In-memory only (dev/testing). No restart survival.
    Memory,

    /// Single-file SQLite, durable across restart. The DB path is
    /// taken from `StateConfig.config["path"]`; if absent, defaults
    /// to `/var/lib/flint-pnfs/state.db`.
    Sqlite,
}

/// High availability configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HaConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default = "default_replicas")]
    pub replicas: u32,

    #[serde(rename = "leaderElection", default = "default_true")]
    pub leader_election: bool,

    #[serde(rename = "leaseDuration", default = "default_lease_duration")]
    pub lease_duration: u64,

    #[serde(rename = "renewDeadline", default = "default_renew_deadline")]
    pub renew_deadline: u64,

    #[serde(rename = "retryPeriod", default = "default_retry_period")]
    pub retry_period: u64,
}

impl Default for HaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            replicas: default_replicas(),
            leader_election: true,
            lease_duration: default_lease_duration(),
            renew_deadline: default_renew_deadline(),
            retry_period: default_retry_period(),
        }
    }
}

/// Failover configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailoverConfig {
    #[serde(rename = "heartbeatTimeout", default = "default_heartbeat_timeout")]
    pub heartbeat_timeout: u64,

    #[serde(default)]
    pub policy: FailoverPolicy,

    #[serde(rename = "gracePeriod", default = "default_grace_period")]
    pub grace_period: u64,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self {
            heartbeat_timeout: default_heartbeat_timeout(),
            policy: FailoverPolicy::RecallAffected,
            grace_period: default_grace_period(),
        }
    }
}

/// Failover policy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailoverPolicy {
    /// Recall all layouts immediately
    RecallAll,

    /// Recall only layouts using failed DS
    RecallAffected,

    /// Let clients discover failure
    Lazy,
}

impl Default for FailoverPolicy {
    fn default() -> Self {
        Self::RecallAffected
    }
}

/// MDS endpoint configuration (for DS)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MdsEndpointConfig {
    pub endpoint: String,

    /// MDS shard endpoints (mds-sharding-plan.md Phase 2). When
    /// non-empty the DS registers and heartbeats with EVERY listed
    /// shard, each on its own independent client/failure state;
    /// `endpoint` above is then ignored. Empty (the default, and what
    /// pre-sharding configs deserialize to) = single-MDS behavior via
    /// `endpoint`.
    #[serde(default)]
    pub endpoints: Vec<String>,

    #[serde(rename = "heartbeatInterval", default = "default_heartbeat_interval")]
    pub heartbeat_interval: u64,

    #[serde(rename = "registrationRetry", default = "default_registration_retry")]
    pub registration_retry: u64,

    #[serde(rename = "maxRetries", default)]
    pub max_retries: u32,
}

impl MdsEndpointConfig {
    /// The effective registration set: `endpoints` when non-empty,
    /// else the single legacy `endpoint`.
    pub fn effective_endpoints(&self) -> Vec<String> {
        if self.endpoints.is_empty() {
            vec![self.endpoint.clone()]
        } else {
            self.endpoints.clone()
        }
    }
}

/// Block device configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BdevConfig {
    /// Logical name of the block device
    pub name: String,
    
    /// Mount point where the SPDK volume is mounted (via ublk)
    /// Example: /mnt/pnfs-data
    /// 
    /// The SPDK logical volume should be:
    /// 1. Created with SPDK RAID (for redundancy/performance)
    /// 2. Exposed via ublk as /dev/ublkb<N>
    /// 3. Formatted with a filesystem (ext4, xfs, etc.)
    /// 4. Mounted at this path
    #[serde(alias = "path")]
    pub mount_point: String,
    
    /// SPDK volume name (for reference/monitoring)
    #[serde(default)]
    pub spdk_volume: Option<String>,
}

/// Resource configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceConfig {
    #[serde(rename = "maxConnections", default = "default_max_connections")]
    pub max_connections: u32,

    #[serde(rename = "ioQueueDepth", default = "default_io_queue_depth")]
    pub io_queue_depth: u32,

    #[serde(rename = "ioBufferSize", default = "default_io_buffer_size")]
    pub io_buffer_size: u64,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            max_connections: default_max_connections(),
            io_queue_depth: default_io_queue_depth(),
            io_buffer_size: default_io_buffer_size(),
        }
    }
}

/// Performance configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceConfig {
    #[serde(rename = "useSpdkIo", default = "default_true")]
    pub use_spdk_io: bool,

    #[serde(rename = "ioThreads", default = "default_io_threads")]
    pub io_threads: u32,

    #[serde(rename = "zeroCopy", default = "default_true")]
    pub zero_copy: bool,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            use_spdk_io: true,
            io_threads: default_io_threads(),
            zero_copy: true,
        }
    }
}

/// NFS export configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportConfig {
    pub path: String,
    pub fsid: u32,

    #[serde(default)]
    pub options: Vec<String>,

    #[serde(default)]
    pub access: Vec<AccessConfig>,
}

/// Access control configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessConfig {
    pub network: String,
    pub permissions: String,
}

/// Logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,

    #[serde(default = "default_log_format")]
    pub format: String,

    #[serde(default)]
    pub components: std::collections::HashMap<String, String>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: default_log_format(),
            components: std::collections::HashMap::new(),
        }
    }
}

/// Monitoring configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitoringConfig {
    #[serde(default)]
    pub prometheus: PrometheusConfig,

    #[serde(default)]
    pub health: HealthConfig,
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            prometheus: PrometheusConfig::default(),
            health: HealthConfig::default(),
        }
    }
}

/// Prometheus configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrometheusConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default = "default_prometheus_port")]
    pub port: u16,

    #[serde(default = "default_prometheus_path")]
    pub path: String,
}

impl Default for PrometheusConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_prometheus_port(),
            path: default_prometheus_path(),
        }
    }
}

/// Health check configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default = "default_health_port")]
    pub port: u16,

    #[serde(default = "default_health_path")]
    pub path: String,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_health_port(),
            path: default_health_path(),
        }
    }
}

// Default value functions
fn default_replicas() -> u32 { 1 }
fn default_true() -> bool { true }
fn default_lease_duration() -> u64 { 15 }
fn default_renew_deadline() -> u64 { 10 }
fn default_retry_period() -> u64 { 2 }
fn default_heartbeat_timeout() -> u64 { 30 }
fn default_grace_period() -> u64 { 60 }
fn default_heartbeat_interval() -> u64 { 10 }
fn default_registration_retry() -> u64 { 5 }
fn default_max_connections() -> u32 { 1000 }
fn default_io_queue_depth() -> u32 { 128 }
fn default_io_buffer_size() -> u64 { 1048576 }
fn default_io_threads() -> u32 { 4 }
fn default_log_level() -> String { "info".to_string() }
fn default_log_format() -> String { "json".to_string() }
fn default_prometheus_port() -> u16 { 9090 }
fn default_prometheus_path() -> String { "/metrics".to_string() }
fn default_health_port() -> u16 { 8080 }
fn default_health_path() -> String { "/health".to_string() }

impl PnfsConfig {
    /// Load configuration from YAML file
    pub fn from_file<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let mut config: Self = serde_yaml::from_str(&contents)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        
        // Substitute environment variables in configuration
        config.substitute_env_vars();
        
        Ok(config)
    }
    
    /// Substitute environment variables in configuration strings
    /// 
    /// Replaces ${VAR_NAME} patterns with environment variable values.
    /// This is particularly useful for device IDs that need to be unique per node.
    fn substitute_env_vars(&mut self) {
        if let Some(ref mut ds_config) = self.ds {
            ds_config.device_id = Self::expand_env_vars(&ds_config.device_id);
        }
    }
    
    /// Expand environment variables in a string
    /// 
    /// Supports both ${VAR} and $VAR syntax.
    /// Returns the original string if the environment variable is not set.
    fn expand_env_vars(input: &str) -> String {
        use std::env;
        
        let mut result = input.to_string();
        
        // Match ${VAR_NAME} patterns
        while let Some(start) = result.find("${") {
            if let Some(end) = result[start..].find('}') {
                let var_name = &result[start + 2..start + end];
                let replacement = env::var(var_name).unwrap_or_else(|_| {
                    tracing::warn!("Environment variable '{}' not found, keeping original", var_name);
                    format!("${{{}}}", var_name)
                });
                result.replace_range(start..start + end + 1, &replacement);
            } else {
                break;
            }
        }
        
        result
    }

    /// Load configuration from environment variables
    pub fn from_env() -> std::io::Result<Self> {
        // Read PNFS_MODE environment variable
        let mode = std::env::var("PNFS_MODE")
            .unwrap_or_else(|_| "standalone".to_string())
            .to_lowercase();

        let mode = match mode.as_str() {
            "standalone" => PnfsMode::Standalone,
            "mds" => PnfsMode::MetadataServer,
            "ds" => PnfsMode::DataServer,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Invalid PNFS_MODE: {}", mode),
                ))
            }
        };

        // TODO: Parse other environment variables
        // For now, return minimal config

        Ok(PnfsConfig {
            mode,
            mds: None,
            ds: None,
            exports: vec![],
            logging: LoggingConfig::default(),
            monitoring: MonitoringConfig::default(),
        })
    }

    /// Construct a `StateBackend` trait object from a `StateConfig`.
    /// Used at MDS startup; the resulting `Arc` is shared by the
    /// `StateManager` and `LayoutManager`. Sqlite path defaults to
    /// `/var/lib/flint-pnfs/state.db`; B.4-and-later operators
    /// override via `state.config.path`.
    pub fn build_state_backend(
        cfg: &StateConfig,
    ) -> Result<std::sync::Arc<dyn crate::state_backend::StateBackend>, String> {
        match cfg.backend {
            StateBackend::Memory => Ok(crate::state_backend::memory_backend()),
            StateBackend::Sqlite => {
                let path = cfg
                    .config
                    .get("path")
                    .cloned()
                    .unwrap_or_else(|| "/var/lib/flint-pnfs/state.db".to_string());
                let parent = std::path::Path::new(&path).parent();
                if let Some(p) = parent {
                    if !p.as_os_str().is_empty() {
                        std::fs::create_dir_all(p).map_err(|e| {
                            format!("create dir {}: {}", p.display(), e)
                        })?;
                    }
                }
                // open_durable (synchronous=FULL), not open (NORMAL):
                // for block-class volumes this DB is not bookkeeping,
                // it is the volume's data map (§5 — extent rows lost
                // over live data = F67's silent zeros) and the fence's
                // only positive record. NORMAL's commits are durable at
                // the CHECKPOINT, not at commit — the unfence rig
                // power-cycled the MDS node moments after a fence and
                // the "durable" record was gone. FULL fsyncs the WAL
                // per commit batch; the writer thread's group commit
                // amortizes it (same trade the standalone server has
                // shipped since v1.7).
                //
                // FLINT_PNFS_STATE_SYNC=normal is the bench/A-B escape
                // hatch (and a deliberate operator choice for
                // files-ONLY fleets that accept checkpoint-granularity
                // durability). It is a foot-gun on block-class fleets
                // — say so loudly, once, at open.
                let sync = std::env::var("FLINT_PNFS_STATE_SYNC").unwrap_or_default();
                let backend = if sync.eq_ignore_ascii_case("normal") {
                    tracing::warn!(
                        "FLINT_PNFS_STATE_SYNC=normal: state commits are durable at \
                         CHECKPOINT, not commit — on a block-class fleet a power loss \
                         can drop committed extent rows (F67 silent zeros) and fence \
                         records (fence silently lifted). Bench/files-only use ONLY."
                    );
                    crate::state_backend::SqliteBackend::open(&path)
                } else {
                    crate::state_backend::SqliteBackend::open_durable(&path)
                }
                .map_err(|e| format!("open sqlite at {}: {}", path, e))?;
                Ok(std::sync::Arc::new(backend))
            }
        }
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<(), String> {
        match self.mode {
            PnfsMode::Standalone => {
                // No special validation needed
                Ok(())
            }
            PnfsMode::MetadataServer => {
                if self.mds.is_none() {
                    return Err("MDS mode requires 'mds' configuration".to_string());
                }
                // TODO: Validate MDS config
                Ok(())
            }
            PnfsMode::DataServer => {
                if self.ds.is_none() {
                    return Err("DS mode requires 'ds' configuration".to_string());
                }
                // TODO: Validate DS config
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_endpoints_prefers_shard_list() {
        // Pre-sharding YAML (no `endpoints` key) → the single legacy
        // endpoint. This is exactly what old configs deserialize to.
        let single: MdsEndpointConfig = serde_yaml::from_str(
            "endpoint: \"mds:50051\"\nheartbeatInterval: 10\n",
        )
        .unwrap();
        assert_eq!(single.effective_endpoints(), vec!["mds:50051".to_string()]);

        // Sharded YAML → the list, order preserved (index = shard id);
        // the legacy endpoint is ignored.
        let sharded: MdsEndpointConfig = serde_yaml::from_str(
            "endpoint: \"legacy:50051\"\nendpoints:\n  - \"mds-0:50051\"\n  - \"mds-1:50051\"\nheartbeatInterval: 10\n",
        )
        .unwrap();
        assert_eq!(
            sharded.effective_endpoints(),
            vec!["mds-0:50051".to_string(), "mds-1:50051".to_string()],
        );
    }

    #[test]
    fn tier_section_parses_with_defaults() {
        let t: TierConfig = serde_yaml::from_str("bucket: my-bucket\n").unwrap();
        assert!(t.enabled);
        assert_eq!(t.key_prefix, "");
        assert_eq!(t.endpoint, None);
        assert_eq!(t.flush_floor_secs, 60);
        assert_eq!(t.quiesce_secs, 10);
        assert_eq!(t.tick_secs, 10);
        assert_eq!(t.whole_put_max_bytes, 64 * 1024 * 1024);
        assert_eq!(t.part_floor_bytes, 16 * 1024 * 1024);
        assert_eq!(t.epoch_heartbeat_secs, 10);
        assert_eq!(t.epoch_lease_misses, 6);
        assert_eq!(t.reserve_bytes, 256 * 1024 * 1024);
        assert_eq!(t.watermark_pct, 85);
        assert_eq!(t.ballast_bytes, 64 * 1024 * 1024);
        assert_eq!(t.hydrate_hold_secs, 15);
        assert_eq!(t.hydrate_concurrency, 4);

        let t2: TierConfig = serde_yaml::from_str(
            "bucket: b\nenabled: false\nkeyPrefix: vol1/\nendpoint: \"http://minio:9000\"\nepochLeaseMisses: 3\n",
        )
        .unwrap();
        assert!(!t2.enabled);
        assert_eq!(t2.key_prefix, "vol1/");
        assert_eq!(t2.endpoint.as_deref(), Some("http://minio:9000"));
        assert_eq!(t2.epoch_lease_misses, 3);
    }

    #[test]
    fn test_default_config() {
        let config = PnfsConfig {
            mode: PnfsMode::Standalone,
            mds: None,
            ds: None,
            exports: vec![],
            logging: LoggingConfig::default(),
            monitoring: MonitoringConfig::default(),
        };

        assert_eq!(config.mode, PnfsMode::Standalone);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_mode_from_string() {
        // Test YAML deserialization
        let yaml = r#"
mode: mds
"#;
        let config: PnfsConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.mode, PnfsMode::MetadataServer);
    }

    /// Flint-lite: `mode: standalone` parses, an mds section with no
    /// `standalone:` key defaults the flag OFF (existing configs keep
    /// their meaning), and the flag parses when set explicitly.
    #[test]
    fn standalone_mode_and_flag_parse() {
        let yaml = r#"
mode: standalone
"#;
        let config: PnfsConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.mode, PnfsMode::Standalone);
        assert!(config.validate().is_ok());

        let mds_yaml = r#"
bind:
  address: "0.0.0.0"
  port: 20490
layout:
  type: file
  stripeSize: 8388608
  policy: stripe
dataServers: []
state:
  backend: memory
  config: {}
"#;
        let mds: MdsConfig = serde_yaml::from_str(mds_yaml).unwrap();
        assert!(!mds.standalone, "absent key must default to the full MDS posture");

        let lite: MdsConfig =
            serde_yaml::from_str(&format!("{}standalone: true\n", mds_yaml)).unwrap();
        assert!(lite.standalone);
        assert!(lite.data_servers.is_empty());
    }

    /// Phase B.4: the new `sqlite` variant of `state.backend`
    /// deserializes correctly from operator YAML and the path key
    /// shows up in the config sub-map. The smoke YAML still uses
    /// `memory`, so `Memory` must continue to round-trip too.
    #[test]
    fn test_state_backend_yaml_round_trip() {
        let yaml_sqlite = r#"
backend: sqlite
config:
  path: /var/lib/flint-pnfs/state.db
"#;
        let cfg: StateConfig = serde_yaml::from_str(yaml_sqlite).unwrap();
        assert_eq!(cfg.backend, StateBackend::Sqlite);
        assert_eq!(
            cfg.config.get("path").map(String::as_str),
            Some("/var/lib/flint-pnfs/state.db"),
        );

        let yaml_memory = r#"
backend: memory
config: {}
"#;
        let cfg: StateConfig = serde_yaml::from_str(yaml_memory).unwrap();
        assert_eq!(cfg.backend, StateBackend::Memory);

        // The Kubernetes / Etcd variants no longer parse — operator
        // catches the breaking change at startup, not at runtime.
        let yaml_old = r#"
backend: kubernetes
config: {}
"#;
        let parsed: Result<StateConfig, _> = serde_yaml::from_str(yaml_old);
        assert!(parsed.is_err(), "kubernetes variant must not parse anymore");
    }

    /// Phase B.4: `build_state_backend(&cfg)` returns the right
    /// `Arc<dyn StateBackend>` for each variant. Sqlite case writes
    /// + reads back through a fresh tempdir to prove the path
    /// resolution works end-to-end.
    #[tokio::test]
    async fn test_build_state_backend_dispatches() {
        let mem_cfg = StateConfig {
            backend: StateBackend::Memory,
            config: std::collections::HashMap::new(),
        };
        let backend = PnfsConfig::build_state_backend(&mem_cfg).unwrap();
        // Sanity: a freshly-built memory backend has counter=0.
        assert_eq!(backend.get_instance_counter().await.unwrap(), 0);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut sqlite_cfg = StateConfig {
            backend: StateBackend::Sqlite,
            config: std::collections::HashMap::new(),
        };
        sqlite_cfg
            .config
            .insert("path".to_string(), path.to_string_lossy().into_owned());
        let backend = PnfsConfig::build_state_backend(&sqlite_cfg).unwrap();
        assert_eq!(backend.increment_instance_counter().await.unwrap(), 1);
        // Re-open over the same path — the counter is durable.
        drop(backend);
        let backend2 = PnfsConfig::build_state_backend(&sqlite_cfg).unwrap();
        assert_eq!(backend2.get_instance_counter().await.unwrap(), 1);
        assert_eq!(backend2.increment_instance_counter().await.unwrap(), 2);
    }
}


