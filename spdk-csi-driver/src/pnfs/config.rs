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

/// Data server information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataServerInfo {
    /// Device ID (unique identifier)
    #[serde(rename = "deviceId")]
    pub device_id: String,

    /// Primary endpoint (IP:port or DNS name)
    pub endpoint: String,

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

/// State persistence backend
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StateBackend {
    /// In-memory only (dev/testing)
    Memory,

    /// Kubernetes ConfigMap
    Kubernetes,

    /// etcd distributed consensus
    Etcd,
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

    #[serde(rename = "heartbeatInterval", default = "default_heartbeat_interval")]
    pub heartbeat_interval: u64,

    #[serde(rename = "registrationRetry", default = "default_registration_retry")]
    pub registration_retry: u64,

    #[serde(rename = "maxRetries", default)]
    pub max_retries: u32,
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
        serde_yaml::from_str(&contents)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
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
}


