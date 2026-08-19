//! The hub's read-only status surface.
//!
//! Everything the hub knew about itself used to be log-only: the tier
//! reporter's gauges went to `tracing`, `ServerStats` was built by a
//! function nobody called, and the epoch/import/warm-fill reports were
//! printed and dropped. A controller deciding whether to suspend or
//! hibernate a volume cannot scrape logs, so this module publishes the
//! same facts as JSON on the health port.
//!
//! Two design rules, both load-bearing:
//!
//! - **The server binds this BEFORE the tier starts and before the NFS
//!   listener.** Claiming the epoch can wait out a dead holder's lease
//!   and a DR import can run for minutes — that whole window is
//!   pre-listener, and it is exactly when an operator most needs to see
//!   "importing, not wedged". So [`HubStatus`] carries a phase that
//!   moves through startup, and every live handle it reports through is
//!   optional until the subsystem that owns it exists.
//! - **The handler never touches the object store.** The RPO predicate
//!   costs two local database reads; the MPU gauges are whatever the
//!   reporter last collected. A status poll must not be able to spend
//!   S3 requests or block on a slow bucket.

use crate::tier::rpo::RpoStatus;
use std::sync::{Arc, OnceLock, RwLock};

/// Where the hub is in its lifecycle. Startup is not one state but
/// several, because the pre-listener window is minutes long and an
/// operator watching a startupProbe needs to tell progress from a wedge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HubPhase {
    /// Process is up; the tier has not started.
    Starting,
    /// Waiting on the epoch cell — possibly waiting out a dead
    /// holder's lease, which is bounded but slow.
    ClaimingEpoch,
    /// Rebuilding the namespace from the bucket.
    Importing,
    /// Reconciling local tier state (markers, crashed operations).
    Reconciling,
    /// The listener is up and clients are served.
    Serving,
    /// A foreign-key sweep is running behind the listener.
    Sweeping,
    /// SIGTERM received; draining and flushing.
    Draining,
    /// Drained cleanly and released the epoch — safe to reclaim.
    Released,
}

/// Live state the status handler reads. Fields the hub fills in as its
/// subsystems come up; readers must tolerate every one being absent.
#[derive(Default)]
pub struct HubStatus {
    phase: RwLock<Option<HubPhase>>,
    started_unix: OnceLock<u64>,
    import: RwLock<Option<crate::tier::import::ImportReport>>,
    warm_fill: RwLock<Option<crate::tier::hydrate::WarmFillReport>>,
    epoch: RwLock<Option<Arc<crate::tier::epoch::EpochGuard>>>,
    orchestrator: RwLock<Option<Arc<crate::tier::flush::FlushOrchestrator>>>,
    backend: RwLock<Option<Arc<dyn crate::state_backend::StateBackend>>>,
    leases: RwLock<Option<Arc<crate::nfs::v4::state::lease::LeaseManager>>>,
}

impl HubStatus {
    pub fn new() -> Self {
        let s = Self::default();
        let _ = s.started_unix.set(now_unix());
        *s.phase.write().unwrap() = Some(HubPhase::Starting);
        s
    }

    pub fn set_phase(&self, phase: HubPhase) {
        if let Ok(mut p) = self.phase.write() {
            *p = Some(phase);
        }
    }

    pub fn phase(&self) -> HubPhase {
        self.phase.read().ok().and_then(|p| *p).unwrap_or(HubPhase::Starting)
    }

    pub fn set_import(&self, report: crate::tier::import::ImportReport) {
        if let Ok(mut r) = self.import.write() {
            *r = Some(report);
        }
    }

    pub fn set_warm_fill(&self, report: crate::tier::hydrate::WarmFillReport) {
        if let Ok(mut r) = self.warm_fill.write() {
            *r = Some(report);
        }
    }

    pub fn attach_epoch(&self, guard: Arc<crate::tier::epoch::EpochGuard>) {
        if let Ok(mut e) = self.epoch.write() {
            *e = Some(guard);
        }
    }

    pub fn attach_orchestrator(&self, orch: Arc<crate::tier::flush::FlushOrchestrator>) {
        if let Ok(mut o) = self.orchestrator.write() {
            *o = Some(orch);
        }
    }

    pub fn attach_backend(&self, backend: Arc<dyn crate::state_backend::StateBackend>) {
        if let Ok(mut b) = self.backend.write() {
            *b = Some(backend);
        }
    }

    pub fn attach_leases(&self, leases: Arc<crate::nfs::v4::state::lease::LeaseManager>) {
        if let Ok(mut l) = self.leases.write() {
            *l = Some(leases);
        }
    }

    /// Assemble the document served at `/status`.
    pub async fn render(&self) -> StatusDoc {
        let started = self.started_unix.get().copied().unwrap_or(0);
        let epoch_guard = self.epoch.read().ok().and_then(|g| g.clone());
        let orch = self.orchestrator.read().ok().and_then(|o| o.clone());
        let backend = self.backend.read().ok().and_then(|b| b.clone());

        // The RPO answer needs a tier. A hub with no tier configured
        // has no bucket to be behind, so it reports None rather than a
        // misleading "clean" — the controller must never read
        // tier-off as "safe to delete the PVC", which for an untiered
        // share is the only copy of the data.
        let rpo = match (&epoch_guard, &backend) {
            (Some(guard), Some(backend)) => {
                Some(crate::tier::rpo::evaluate(backend.as_ref(), guard, orch.as_deref()).await)
            }
            _ => None,
        };

        StatusDoc {
            phase: self.phase(),
            started_unix: started,
            uptime_secs: now_unix().saturating_sub(started),
            epoch: epoch_guard.as_ref().map(|g| EpochDoc { held: g.current().is_some(), number: g.current() }),
            import: self.import.read().ok().and_then(|r| r.clone()),
            warm_fill: self.warm_fill.read().ok().and_then(|r| *r),
            tier: TierDoc {
                gauges: crate::tier::reporter::latest_gauges(),
                meters: crate::tier::meter::snapshot(),
            },
            nfs: NfsDoc {
                active_leases: self
                    .leases
                    .read()
                    .ok()
                    .and_then(|l| l.as_ref().map(|l| l.active_count())),
            },
            activity: crate::nfs::activity::snapshot(),
            rpo_clean: rpo.as_ref().map(|r| r.clean),
            rpo: rpo,
        }
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusDoc {
    pub phase: HubPhase,
    pub started_unix: u64,
    pub uptime_secs: u64,
    pub epoch: Option<EpochDoc>,
    pub import: Option<crate::tier::import::ImportReport>,
    pub warm_fill: Option<crate::tier::hydrate::WarmFillReport>,
    pub tier: TierDoc,
    pub nfs: NfsDoc,
    pub activity: crate::nfs::activity::ActivitySnapshot,
    /// The suspend/hibernate gate, hoisted to the top level because it
    /// is the single field a controller acts on. `None` = no tier.
    pub rpo_clean: Option<bool>,
    pub rpo: Option<RpoStatus>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EpochDoc {
    pub held: bool,
    pub number: Option<u64>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TierDoc {
    pub gauges: Option<crate::tier::reporter::PublishedGauges>,
    pub meters: crate::tier::meter::MeterSnapshot,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NfsDoc {
    pub active_leases: Option<usize>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The route table: a liveness probe and the status document.
pub fn routes(
    health_path: &str,
    status: Arc<HubStatus>,
) -> impl warp::Filter<Extract = (warp::reply::Response,), Error = warp::Rejection> + Clone {
    use warp::{Filter, Reply};
    let health_path = health_path.trim_start_matches('/').to_string();
    let health = warp::path(health_path)
        .and(warp::get())
        .map(|| "OK".into_response());
    let status_route = warp::path("status").and(warp::get()).then(move || {
        let status = status.clone();
        async move { warp::reply::json(&status.render().await).into_response() }
    });
    health.or(status_route).unify()
}

/// Serve `/health`, `/status` and — when configured — the file API on
/// the health port.
///
/// One listener for both, because they share an audience and a security
/// posture: ClusterIP-only, never the consumer-facing Service. Putting
/// a read-write file API on the Service that carries NFS would publish
/// the whole volume the first time someone made that Service a
/// LoadBalancer.
///
/// Failure to bind is logged and swallowed: status is telemetry, and
/// refusing to serve NFS because a diagnostic port is taken would
/// invert the priorities. The file API is a casualty of that choice —
/// noted loudly, because unlike status it is a surface someone is
/// waiting on.
pub fn spawn(
    cfg: &crate::pnfs::config::HealthConfig,
    status: Arc<HubStatus>,
    file_api: Option<(
        Arc<crate::pnfs::mds::fileapi::hubfs::HubFs>,
        crate::pnfs::mds::fileapi::ApiConfig,
    )>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.enabled {
        if file_api.is_some() {
            tracing::error!(
                "the file API is configured but monitoring.health is disabled — they \
                 share one listener, so the API will NOT be served"
            );
        }
        return None;
    }
    use warp::Filter;
    let addr: std::net::SocketAddr = ([0, 0, 0, 0], cfg.port).into();
    // Both tables are normalised to a concrete Response so the two
    // branches can be unified into one server.
    let base = routes(&cfg.path, status.clone()).boxed();
    let combined = match file_api {
        Some((fs, api_cfg)) => {
            tracing::info!("📂 hub file API enabled on the health port");
            base.or(crate::pnfs::mds::fileapi::routes_gated(fs, api_cfg, status))
                .unify()
                .boxed()
        }
        None => base,
    };
    match warp::serve(combined).try_bind_ephemeral(addr) {
        Ok((bound, server)) => {
            tracing::info!("🩺 hub status endpoint listening on {}", bound);
            Some(tokio::spawn(server))
        }
        Err(e) => {
            tracing::warn!(
                "hub status endpoint could not bind {}: {} — continuing without it \
                 (NFS is unaffected, but the lifecycle controller will see no deep status)",
                addr,
                e
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hub with no tier must report `rpoClean: null`, never `true`.
    /// A controller reading tier-off as "clean" would hibernate a share
    /// whose PVC is the only copy of the data.
    #[tokio::test]
    async fn no_tier_reports_unknown_rpo_not_clean() {
        let status = HubStatus::new();
        let doc = status.render().await;
        assert_eq!(doc.rpo_clean, None);
        assert!(doc.rpo.is_none());
        assert_eq!(doc.phase, HubPhase::Starting);
    }

    #[tokio::test]
    async fn the_endpoint_answers_health_and_status() {
        let status = Arc::new(HubStatus::new());
        status.set_phase(HubPhase::ClaimingEpoch);
        let api = routes("/health", status);

        let health = warp::test::request().method("GET").path("/health").reply(&api).await;
        assert_eq!(health.status(), 200);
        assert_eq!(health.body(), "OK");

        let res = warp::test::request().method("GET").path("/status").reply(&api).await;
        assert_eq!(res.status(), 200);
        let doc: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        // The pre-listener phases are the point of binding this early:
        // a controller polling during a slow epoch claim must see
        // progress, not a refused connection.
        assert_eq!(doc["phase"], "claimingEpoch");
        assert!(doc["activity"]["idleSecs"].is_number());
        assert!(doc.get("rpoClean").is_some());
    }

    #[tokio::test]
    async fn phase_moves_and_serializes() {
        let status = HubStatus::new();
        status.set_phase(HubPhase::Importing);
        let doc = status.render().await;
        assert_eq!(doc.phase, HubPhase::Importing);
        let json = serde_json::to_string(&doc).unwrap();
        assert!(json.contains("\"phase\":\"importing\""), "{json}");
        // The gate a controller reads must always be present as a key,
        // even when unknown, so a missing field can never be mistaken
        // for false.
        assert!(json.contains("\"rpoClean\":null"), "{json}");
    }
}
