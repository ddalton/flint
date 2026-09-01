//! `flint-hub-stub` — a fleet-rig stand-in for the flint-lite hub.
//!
//! # Why this exists
//!
//! The operator's design target is 3000 `FlintShare`s with ~300 live
//! hubs, and nothing has ever asserted it — every drill has run 2-4
//! shares. The term that breaks first is the CONTROL PLANE (reconcile
//! rate, apiserver writes, operator RSS, arbitration CPU), not the data
//! plane. A real hub carries `state.db`, the whole tier, a page cache
//! and a real PVC, so 300 of them need a real cluster and real S3. This
//! serves the three things the OPERATOR actually looks at — a TCP
//! listener on the NFS port for the rendered probes, `/health`, and a
//! `/status` document — in a few MB, so 300 live shares fit on a small
//! rig and the measurement is about the operator.
//!
//! # Why it builds the REAL `StatusDoc`
//!
//! Hand-rolled JSON is the obvious shortcut and it is a trap: the
//! operator's parser is tolerant, so a stub whose document drifted from
//! the real one would not fail loudly — it would fall onto
//! `poll_hub`'s `Err` branch, every share would read as unreachable,
//! and the rig would report a beautifully stable fleet that was
//! actually measuring nothing. Constructing [`StatusDoc`] means drift
//! is a COMPILE ERROR instead.
//!
//! # It lies on command
//!
//! The ladder is driven by what the hub reports, so the rig has to be
//! able to say "idle for 20 minutes" without waiting 20 minutes:
//!
//! | env | effect |
//! |---|---|
//! | `STUB_IDLE_SECS` | fixed seconds, or `ramp:<per-sec>` to advance with the clock |
//! | `STUB_RPO_CLEAN` | `true`/`false` — the hibernate gate |
//! | `STUB_PHASE` | `serving` (default), `starting`, `importing`, `draining` |
//! | `STUB_ACTIVE_LEASES` | what `nfs.activeLeases` reports |
//! | `STUB_STATUS_DELAY_MS` | added latency per `/status`, to inflate the operator's `d` |
//! | `STUB_STATUS_FAIL_PCT` | percentage of polls answered 503, for the backoff legs |
//! | `STUB_HEALTH_PORT` | default 8080 |
//! | `STUB_NFS_PORT` | default 2049 |

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use spdk_csi_driver::nfs::activity::ActivitySnapshot;
use spdk_csi_driver::pnfs::mds::status::{
    EpochDoc, FileApiDoc, HubPhase, NfsDoc, StatusDoc, TierDoc,
};
use spdk_csi_driver::tier::rpo::RpoStatus;

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn env_bool(k: &str, d: bool) -> bool {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn phase_from_env() -> HubPhase {
    match std::env::var("STUB_PHASE").unwrap_or_else(|_| "serving".into()).as_str() {
        "starting" => HubPhase::Starting,
        "claimingEpoch" => HubPhase::ClaimingEpoch,
        "importing" => HubPhase::Importing,
        "reconciling" => HubPhase::Reconciling,
        "sweeping" => HubPhase::Sweeping,
        "draining" => HubPhase::Draining,
        _ => HubPhase::Serving,
    }
}

/// `STUB_IDLE_SECS` is either a constant or `ramp:<per-second>`, which
/// advances with wall time so a share can cross a `suspendAfterSecs`
/// threshold during a run without the rig sleeping through it.
fn idle_secs(started: u64) -> u64 {
    // DEFAULT IS UPTIME, not 0. A hub nobody is talking to reports an
    // idle counter that advances with the clock — that is what a real
    // idle hub does, and it is the input the ladder acts on. A stub
    // pinned at 0 would look permanently busy AND would hold its
    // condition message constant, which silently hides the
    // self-amplification term the fleet rig exists to measure.
    let raw = match std::env::var("STUB_IDLE_SECS") {
        Ok(v) => v,
        Err(_) => return now_unix().saturating_sub(started),
    };
    if let Some(rate) = raw.strip_prefix("ramp:") {
        let rate: u64 = rate.parse().unwrap_or(1);
        return now_unix().saturating_sub(started) * rate;
    }
    raw.parse().unwrap_or(0)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Every binary that may build a TLS client installs the provider —
    // enforced by a test that walks [[bin]] rather than a hand list,
    // because what shipped broken in 1.26.0 was a binary that never
    // called it.
    spdk_csi_driver::install_crypto_provider();

    tracing_subscriber::fmt().with_env_filter("info").init();

    let started = now_unix();
    let polls = Arc::new(AtomicU64::new(0));
    let health_port = env_u64("STUB_HEALTH_PORT", 8080) as u16;
    let nfs_port = env_u64("STUB_NFS_PORT", 2049) as u16;

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async move {
        // The rendered Deployment probes TCP 2049. Accept and drop:
        // nothing in the rig speaks NFS, but a closed port fails the
        // readiness probe and the share never reaches Ready.
        tokio::spawn(async move {
            match tokio::net::TcpListener::bind(("0.0.0.0", nfs_port)).await {
                Ok(l) => loop {
                    if l.accept().await.is_err() {
                        break;
                    }
                },
                Err(e) => tracing::error!("stub: cannot bind {nfs_port}: {e}"),
            }
        });

        let listener = tokio::net::TcpListener::bind(("0.0.0.0", health_port)).await?;
        tracing::info!(
            health = health_port, nfs = nfs_port,
            "flint-hub-stub listening (NOT a hub — a control-plane rig stand-in)"
        );

        loop {
            let (mut sock, _) = listener.accept().await?;
            let polls = Arc::clone(&polls);
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/");

                let delay = env_u64("STUB_STATUS_DELAY_MS", 0);
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }

                // Deterministic failure injection: every Nth poll, not
                // a random one, so a leg that trips it reproduces.
                let n_poll = polls.fetch_add(1, Ordering::Relaxed);
                let fail_pct = env_u64("STUB_STATUS_FAIL_PCT", 0);
                if fail_pct > 0 && (n_poll * fail_pct / 100) != ((n_poll + 1) * fail_pct / 100) {
                    let _ = sock
                        .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }

                if path.starts_with("/health") {
                    let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
                    return;
                }

                let idle = idle_secs(started);
                let clean = env_bool("STUB_RPO_CLEAN", true);
                let doc = StatusDoc {
                    phase: phase_from_env(),
                    server_id: Some(format!("stub-{}", std::process::id())),
                    pod_name: std::env::var("POD_NAME").ok(),
                    started_unix: started,
                    uptime_secs: now_unix().saturating_sub(started),
                    epoch: Some(EpochDoc { held: true, number: Some(1) }),
                    import: None,
                    sweep: None,
                    import_refused: None,
                    warm_fill: None,
                    tier: TierDoc { gauges: None, meters: Default::default() },
                    nfs: NfsDoc {
                        active_leases: Some(env_u64("STUB_ACTIVE_LEASES", 0) as usize),
                        // Knob so the fleet rig can exercise the
                        // delegation suspend guard at scale — a stub
                        // that always says zero could never show the
                        // ladder refusing to suspend.
                        outstanding_delegations: Some(env_u64("STUB_DELEGATIONS", 0)),
                    },
                    activity: ActivitySnapshot {
                        last_activity_unix: now_unix().saturating_sub(idle),
                        idle_secs: idle,
                        data_ops: 0,
                        namespace_ops: 0,
                        browse_ops: 0,
                    },
                    rpo_clean: Some(clean),
                    rpo: Some(RpoStatus {
                        clean,
                        dirty_files: 0,
                        pending_capture: false,
                        tombstones: 0,
                        epoch_held: true,
                        manifest_current: clean,
                        manifest_seq: Some(1),
                        beyond_rpo: Some(0),
                        awaiting_first_barrier: false,
                    }),
                    // The stub serves its file API, so the operator's
                    // routesMounted check has something truthful to read.
                    file_api: Some(FileApiDoc { routes_mounted: true }),
                };
                let body = serde_json::to_vec(&doc).unwrap_or_default();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            });
        }
    })
}
