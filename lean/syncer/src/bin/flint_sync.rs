//! flint-sync: the lean checkout/publish syncer (plan of record:
//! docs/plans/flint-lean-plan.md). Runs beside an agent container as a
//! native sidecar: checkout gates the agent start; the barrier loop
//! publishes on the flush floor; preStop drains.
//!
//! Subcommands:
//!   checkout   materialize the workspace (restart-matrix aware), exit.
//!              Takes NO lease: it installs nothing in the bucket, so
//!              the publish fence is not its to hold, and holding it
//!              only made concurrent readers serialise on one cell.
//!   barrier    one publish barrier, exit
//!   sync       the HITL sync verb (scan-first), exit
//!   ctl <boundary|sync|status>
//!              talk to the UDS door of the syncer running in THIS
//!              pod (§2.5). A client, not a second syncer: it takes
//!              no lease and no state lock. Requires FLINT_SYNC_UDS_DOOR.
//!   status     render gauges + pending + lease state as JSON, exit.
//!              Takes NO lease and NO state-dir lock: it exists to
//!              diagnose a workspace whose syncer is dead or deposed,
//!              and claiming would depose the very syncer under
//!              diagnosis.
//!   probe-copy verify the cross-key copy surface against THIS bucket
//!   probe-conditional  verify that If-Match / If-None-Match on PUT and
//!              If-Match on DELETE are ENFORCED by THIS store (one that
//!              ignores them turns every manifest CAS into
//!              last-writer-wins, and lets a garbage collector delete
//!              another writer's upload — silently)
//!   manifest   print the resolved manifest and a HEAD of every citation
//!              as one JSON line, exit. Read-only: no lease, no state
//!              lock, no tree. One HEAD per entry — for drills and for an
//!              operator asking whether every citation resolves, not for a
//!              100k-entry workspace on a hot path.
//!   run        checkout → barrier loop (floorSecs) → drain on SIGTERM.
//!              No lease is held between barriers: each barrier claims
//!              the publish fence for its commit section only (after
//!              its uploads) and hands it to the next waiter, so a second
//!              writer on the workspace is Ready in checkout time.
//!              Nothing marks a writer live between barriers: the cell
//!              detects a dead holder from its own token.
//!
//! Environment:
//!   FLINT_SYNC_BUCKET    (required) bucket name
//!   FLINT_SYNC_PREFIX    (required) subtree key prefix
//!   FLINT_SYNC_ROOT      (required) workspace root
//!   FLINT_SYNC_ENDPOINT  S3 endpoint override (MinIO/proxy rigs)
//!   FLINT_SYNC_RAW_READS "true" routes every GET/HEAD through the raw
//!                        HTTP/1.1 read path (flint-store `rawread.rs`):
//!                        SigV4 by hand, pooled keep-alive, no SDK
//!                        per-request machinery. Writes stay on the SDK.
//!   FLINT_SYNC_FLOOR_SECS         publish cadence floor (default 60)
//!   FLINT_SYNC_MAX_BYTES/_FILES   checkout budgets (0 = unlimited)
//!   FLINT_SYNC_CHECKOUT_SCOPE     comma list of path prefixes; checkout
//!                                 materialises ONLY what they cover.
//!                                 Unset = the whole manifest. A scope
//!                                 whose every entry is malformed is
//!                                 REFUSED, never widened. The admitted
//!                                 set is not frozen: a path the remote
//!                                 changes arrives through the inbox and
//!                                 is then owned like any other.
//!   FLINT_SYNC_FANOUT             concurrent FETCHES (default 128)
//!   FLINT_SYNC_UPLOAD_FANOUT      concurrent UPLOADS (default 32)
//!   FLINT_SYNC_RANGE_GET_MIN_MB   materialise objects >= this as parallel
//!                                 ranges (default 8; 0 = off)
//!   FLINT_SYNC_RANGE_GET_CHUNK_MB bytes per range (default 16)
//!   FLINT_SYNC_RANGE_GET_PARALLELISM ranges in flight per object (default 4)
//!   FLINT_SYNC_FETCH_INFLIGHT_MB  checkout bytes in flight (default 128)
//!   FLINT_SYNC_COPY_WHOLE_MAX_MB   single-request ceiling for a cross-key
//!                                 copy (default 5120; above it, MPU +
//!                                 UploadPartCopy)
//!   FLINT_SYNC_UPLOAD_PART_PARALLELISM  parts of ONE object uploaded
//!                                 concurrently on publish (default 8)
//!   FLINT_SYNC_UPLOAD_INFLIGHT_MB upload bytes in flight — every part
//!                                 and whole body is read into RAM before
//!                                 its PUT (default 256; 0 = no bound)
//!   FLINT_SYNC_SOLE_WRITER        "true" marks every manifest this
//!                                 syncer installs as a PUBLISHED
//!                                 mirror: readers then refuse an
//!                                 object that has moved off its
//!                                 citation instead of adopting it.
//!                                 Set by forge's legible export.
//!   FLINT_SYNC_SENTINELS          auto|off|force (default auto)
//!   FLINT_SYNC_SENTINEL_MIN_INTERVAL_SECS  (default 5)
//!   FLINT_SYNC_SENTINEL_HOURLY_BUDGET      work units/hour (default 60)
//!   FLINT_SYNC_SENTINEL_POLL_SECS          (default 1; env-only)
//!   FLINT_SYNC_UDS_DOOR                    "true" arms .flint-sync/ctl.sock
//!   FLINT_SYNC_METRICS                     "true" arms /metrics (D15)
//!   FLINT_SYNC_METRICS_PORT                default 9847
//!   FLINT_SYNC_WORKSPACE/_NAMESPACE        the only two metric labels

use std::sync::Arc;
use std::time::Duration;

use flint_lean::state::SyncerState;
use flint_lean::lease;
use flint_lean::verbs;
use flint_lean::{LeanConfig, LeanError, Syncer, SentinelMode};
use flint_store::s3::S3Store;
use flint_store::ObjectStore;
use warp::Filter;

fn env_req(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("flint-sync: {name} is required");
        std::process::exit(2);
    })
}

fn flint_lean_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One retry log line, with a credential refusal called by its name.
///
/// Every serve arm retries a non-fence error, which is right. But
/// "failed (retrying)" describes a 401/403 exactly as it describes a
/// flaky bucket, and only one of the two is fixed by waiting — the
/// diagnosis `StoreError::Auth` exists to make (design §6.3). The
/// consequence is worth spelling out on the line itself: a paused
/// holder stops renewing, and a stopped renewal is precisely what a
/// challenger reads as a dead holder.
fn log_retry(sc: &Syncer, e: &LeanError, fallback: &str) {
    if !e.is_auth() {
        eprintln!("flint-sync: {fallback}: {e}");
        return;
    }
    let paused = sc
        .load_gauges()
        .ok()
        .and_then(|g| g.auth_paused_since_unix)
        .map(|t| flint_lean_now().saturating_sub(t))
        .unwrap_or(0);
    eprintln!(
        "flint-sync: REFUSED reason=auth arm={fallback} paused_secs={paused}: {e} \
         — the store rejected our credentials. Not contention, not a lease \
         conflict; retrying does not fix it. Local files keep serving and \
         staged work is intact. Check the credential broker and the projected \
         token (and this node's clock — a skewed one answers 403 too). \
         Renewals have STOPPED: a challenger that can still reach the store \
         may depose this live writer."
    );
}


// See `fastalloc` in Cargo.toml: musl's one-lock malloc is the second
// ceiling on the read path once the fan-out runs on several threads.
#[cfg(feature = "fastalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[tokio::main]
async fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "run".into());
    let bucket = env_req("FLINT_SYNC_BUCKET");
    let prefix = env_req("FLINT_SYNC_PREFIX");
    let root = env_req("FLINT_SYNC_ROOT");
    let endpoint = std::env::var("FLINT_SYNC_ENDPOINT").ok();

    // Parts of ONE object uploaded concurrently. `fanout` already
    // spreads uploads ACROSS objects, so this only moves a tree whose
    // critical path is a single large object — which is the shape a
    // checkpoint actually has. 8: measured on runcu (2026-09-12, n=3)
    // the 4 GiB checkpoint publish went from 50.7-58.6 s at 1 to
    // 13.8-14.4 s at 8, where the NIC is full (16 bought nothing). It
    // could only become the default once the byte bound below existed:
    // every part is read whole into RAM before its PUT, and without the
    // bound the window held `min(objects, fanout) x this x 64 MiB`.
    let part_par = env_u64("FLINT_SYNC_UPLOAD_PART_PARALLELISM", 8).max(1) as usize;
    // Bytes the upload path may hold at once, parts and whole bodies
    // alike — the write side's FLINT_SYNC_FETCH_INFLIGHT_MB. A single
    // body or part larger than the whole window still uploads, alone.
    let upload_inflight = env_u64("FLINT_SYNC_UPLOAD_INFLIGHT_MB", 256) * 1024 * 1024;
    // The single-request copy ceiling, in MiB. Exists so a drill can
    // drive the MPU + UploadPartCopy arm without a 5 GiB fixture: that
    // arm has never executed anywhere, and an arm only a 5 GiB object
    // can reach is an arm nothing reaches.
    let copy_whole_max = env_u64("FLINT_SYNC_COPY_WHOLE_MAX_MB", 5 * 1024) * 1024 * 1024;
    let raw_reads = std::env::var("FLINT_SYNC_RAW_READS")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let store = match S3Store::connect(bucket, endpoint).await {
        Ok(s) => match s
            .with_part_parallelism(part_par)
            .with_upload_inflight_max_bytes(upload_inflight)
            .with_copy_whole_max(copy_whole_max)
            .with_raw_reads(raw_reads)
        {
            Ok(s) => Arc::new(s) as Arc<dyn ObjectStore>,
            Err(e) => {
                eprintln!("flint-sync: raw reads: {e}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("flint-sync: store connect: {e}");
            std::process::exit(1);
        }
    };
    let mut cfg = LeanConfig::new(&prefix, &root);
    cfg.floor_secs = env_u64("FLINT_SYNC_FLOOR_SECS", 60);
    cfg.sole_writer = std::env::var("FLINT_SYNC_SOLE_WRITER")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    cfg.max_bytes = env_u64("FLINT_SYNC_MAX_BYTES", 0);
    cfg.max_files = env_u64("FLINT_SYNC_MAX_FILES", 0);
    cfg.fanout = env_u64("FLINT_SYNC_FANOUT", 128).max(1) as usize;
    // 0 = auto (the machine's cores, at most 8) — the value the CR
    // stamps by default, so a stamped default and no stamp read alike.
    match env_u64("FLINT_SYNC_FETCH_DRIVERS", 0) {
        0 => {}
        n => cfg.fetch_drivers = n as usize,
    }
    // Separate from the read knob on purpose — see barrier.rs: uploads
    // have no byte gate, and this number multiplies into the lease-fence
    // window. Raising it needs its own measurement.
    cfg.upload_fanout = env_u64("FLINT_SYNC_UPLOAD_FANOUT", 32).max(1) as usize;
    cfg.project_id = std::env::var("FLINT_SYNC_PROJECT_ID").ok().filter(|p| !p.is_empty());
    cfg.fetch_inflight_max_bytes =
        env_u64("FLINT_SYNC_FETCH_INFLIGHT_MB", 128).max(1) * 1024 * 1024;
    // 0 = off, which is the shipped default until a drill moves it.
    cfg.range_get_min_bytes = env_u64("FLINT_SYNC_RANGE_GET_MIN_MB", 8) * 1024 * 1024;
    cfg.range_get_chunk_bytes = env_u64("FLINT_SYNC_RANGE_GET_CHUNK_MB", 16).max(1) * 1024 * 1024;
    cfg.range_get_parallelism = env_u64("FLINT_SYNC_RANGE_GET_PARALLELISM", 4).max(1) as usize;
    if let Ok(m) = std::env::var("FLINT_SYNC_SENTINELS") {
        match SentinelMode::parse(&m) {
            Some(sm) => cfg.sentinel_mode = sm,
            None => {
                eprintln!("flint-sync: FLINT_SYNC_SENTINELS={m:?} is not auto|off|force");
                std::process::exit(2);
            }
        }
    }
    cfg.sentinel_min_interval_secs = env_u64("FLINT_SYNC_SENTINEL_MIN_INTERVAL_SECS", 5);
    cfg.sentinel_hourly_budget = env_u64("FLINT_SYNC_SENTINEL_HOURLY_BUDGET", 60);
    cfg.sentinel_poll_secs = env_u64("FLINT_SYNC_SENTINEL_POLL_SECS", 1).max(1);
    // Drill-only: opens the mid-commit window no drill can hit by timing.
    cfg.drill_hold_commit_secs = env_u64("FLINT_SYNC_DRILL_HOLD_COMMIT_SECS", 0);
    cfg.drill_hold_gc_secs = env_u64("FLINT_SYNC_DRILL_HOLD_GC_SECS", 0);
    if matches!(std::env::var("FLINT_SYNC_EVENT_TRACE").as_deref(), Ok("1") | Ok("true")) {
        cfg.event_trace = Some(flint_lean::trace::Sink::Stderr);
    }
    // Also dispatched before the state directory is opened, and for a
    // stronger reason: `ctl` is a CLIENT of the running syncer. Taking
    // the occupancy lock — or the lease — would fight the very process
    // it is asking to do something.
    if cmd == "ctl" {
        let verb = std::env::args().nth(2).unwrap_or_else(|| "status".into());
        let (method, path) = match verb.as_str() {
            "boundary" => ("POST", "/v1/boundary"),
            "sync" => ("POST", "/v1/sync"),
            "status" => ("GET", "/v1/status"),
            other => {
                eprintln!("flint-sync ctl: unknown verb {other:?} (boundary|sync|status)");
                std::process::exit(2);
            }
        };
        let sock = flint_lean::uds::socket_path(&cfg.state_dir());
        match ctl_call(&sock, method, path).await {
            Ok(body) => {
                println!("{body}");
                return;
            }
            Err(e) => {
                eprintln!("flint-sync ctl: {} ({e})", sock.display());
                std::process::exit(1);
            }
        }
    }

    // Read-only, before the state directory for the same reason as
    // `status`: it must answer while a syncer holds the occupancy lock.
    if cmd == "manifest" {
        match flint_lean::manifest::load(store.as_ref(), &cfg).await {
            Ok(Some(m)) => {
                let mut heads = serde_json::Map::new();
                for (path, e) in &m.manifest.entries {
                    let head = match store.head(&e.key).await {
                        Ok(meta) => serde_json::Value::String(meta.etag),
                        Err(flint_store::StoreError::NotFound(_)) => serde_json::Value::Null,
                        Err(err) => {
                            eprintln!("flint-sync: manifest: HEAD {}: {err}", e.key);
                            std::process::exit(1);
                        }
                    };
                    heads.insert(path.clone(), head);
                }
                let out = serde_json::json!({
                    "seq": m.manifest.seq, "pointer_etag": m.etag,
                    "entries": m.manifest.entries, "heads": heads,
                });
                println!("{}", serde_json::to_string(&out).unwrap());
                return;
            }
            Ok(None) => {
                println!("{}", serde_json::json!({"seq": null, "entries": {}, "heads": {}}));
                return;
            }
            Err(e) => {
                eprintln!("flint-sync: manifest: {e}");
                std::process::exit(1);
            }
        }
    }

    // Dispatched before the state directory is opened: a live syncer
    // holds the occupancy flock, and `status` must work WHILE it does.
    if cmd == "status" {
        match flint_lean::status_report(&cfg) {
            Ok(r) => {
                println!("{}", serde_json::to_string_pretty(&r).unwrap());
                return;
            }
            Err(e) => {
                eprintln!("flint-sync: status: {e}");
                std::process::exit(1);
            }
        }
    }

    let state = match SyncerState::open(cfg.state_dir()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("flint-sync: state dir: {e}");
            std::process::exit(1);
        }
    };
    let mut sc = Syncer { store, cfg, state, lease: None, noted_not_regular: Default::default() };

    // Conformance probes: they take NO lease and touch no tree, so they
    // run before the claim. A probe that had to depose a live syncer to
    // answer "does this bucket support X?" would be unusable on exactly
    // the workspaces anyone wants the answer for.
    if cmd == "probe-copy" {
        let key = format!("{}/{}/probe-copy", sc.cfg.prefix, flint_lean::LEAN_DIR);
        // Say which ARM this run exercises. The probe's payload is a few
        // dozen bytes, so any ceiling above zero takes the single-request
        // CopyObject path — and a run that reported PASS while silently
        // repeating the arm you already tested is worse than no run.
        eprintln!(
            "flint-sync: probe-copy ceiling={} bytes -> the {} arm",
            copy_whole_max,
            if copy_whole_max == 0 { "MPU + UploadPartCopy" } else { "CopyObject (payload is tiny)" }
        );
        match flint_store::probe::probe_cross_key_copy(sc.store.as_ref(), &key).await {
            Ok(()) => {
                eprintln!("flint-sync: probe-copy PASS ({key})");
                return;
            }
            Err(e) => {
                eprintln!("flint-sync: probe-copy FAIL: {e}");
                std::process::exit(1);
            }
        }
    }

    if cmd == "probe-conditional" {
        // Both surfaces lean's arbitration stands on: If-Match /
        // If-None-Match on PUT (every upload, every pointer CAS) and
        // If-Match on DELETE (the garbage collector, once two writers
        // share a workspace). A store that ignores either answers exactly
        // like one that enforces it, so each is asked, not assumed.
        let key = format!("{}/{}/probe-conditional", sc.cfg.prefix, flint_lean::LEAN_DIR);
        if let Err(e) = flint_store::probe::probe_conditional_writes(sc.store.as_ref(), &key).await {
            eprintln!("flint-sync: probe-conditional FAIL (PUT): {e}");
            std::process::exit(1);
        }
        let dkey = format!("{}/{}/probe-conditional-delete", sc.cfg.prefix, flint_lean::LEAN_DIR);
        match flint_store::probe::probe_conditional_delete(sc.store.as_ref(), &dkey).await {
            Ok(()) => {
                eprintln!("flint-sync: probe-conditional PASS — PUT ({key}) and DELETE ({dkey})");
                return;
            }
            Err(e) => {
                eprintln!("flint-sync: probe-conditional FAIL (DELETE): {e}");
                std::process::exit(1);
            }
        }
    }

    let result = match cmd.as_str() {
        "checkout" => verbs::run_verb(&mut sc, verbs::Step::Checkout).await,
        "barrier" => verbs::run_verb(&mut sc, verbs::Step::Barrier).await,
        "sync" => verbs::run_verb(&mut sc, verbs::Step::Sync).await,
        // `rescope <a> <b> ...` narrows/widens to exactly that set;
        // `rescope --all` goes back to the whole tree. Spelled out
        // rather than "no arguments means everything", because the
        // whole-tree case is the DESTRUCTIVE-looking one to get by
        // accident: a bare `rescope` would silently refetch a
        // deliberately-scoped workspace's entire manifest.
        "rescope" => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            let target = if rest == ["--all"] {
                None
            } else if rest.is_empty() {
                eprintln!(
                    "flint-sync: rescope needs a scope (`rescope inputs docs/spec.md`) or \
                     `--all` for the whole tree"
                );
                std::process::exit(2);
            } else {
                Some(rest)
            };
            verbs::run_verb(&mut sc, verbs::Step::Rescope(target)).await
        }
        "run" => run_loop(&mut sc).await,
        other => {
            eprintln!(
                "flint-sync: unknown subcommand {other:?} \
                 (checkout|barrier|sync|rescope|status|manifest|ctl|run|probe-copy|probe-conditional)"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("flint-sync: {e}");
        // A refusal is final: EXIT_REFUSED is the code the CSI plugin
        // reads as "tear the worker down and name the reason on the
        // tenant" — under OnFailure any other code is relaunched in
        // place forever, and a refusal that shared it looked to the
        // tenant like a checkout that never finished (leg S22).
        std::process::exit(match e {
            LeanError::Refused(_) => flint_lean::EXIT_REFUSED,
            _ => 1,
        });
    }
}

/// One request over the control socket. Deliberately hand-rolled: the
/// door is a bounded, pod-internal, one-request-per-connection surface,
/// and a client that pulls in an HTTP stack to say twelve bytes would
/// be the tail wagging the dog.
async fn ctl_call(
    sock: &std::path::Path,
    method: &str,
    path: &str,
) -> std::io::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::UnixStream::connect(sock).await?;
    s.write_all(format!("{method} {path} HTTP/1.1\r\nhost: flint\r\n\r\n").as_bytes())
        .await?;
    s.flush().await?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf).to_string();
    Ok(match text.split_once("\r\n\r\n") {
        Some((_, body)) => body.to_string(),
        None => text,
    })
}


async fn run_loop(sc: &mut Syncer) -> Result<(), LeanError> {
    // No claim before checkout (design 2026-09-13 §4): the cell is a
    // publish fence and a checkout installs nothing. What stays are the
    // project-id precondition (a refusal, not a fence), the shared-prefix
    // diagnostic, and one repair: a cell a previous container of THIS pod
    // left held is released now, before anyone waits 60 s to depose it.
    lease::verify_claim(sc).await?;
    lease::warn_if_prefix_is_shared(sc).await;
    if let Err(e) = lease::release_stale_own(sc).await {
        eprintln!("flint-sync: could not release a fence left held by a previous container: {e}");
    }
    // This incarnation owes its own drain attestation; one left by an
    // earlier life of this tree must not vouch for it.
    if let Err(e) = sc.state.clear_drained() {
        eprintln!("flint-sync: could not clear a stale drain attestation: {e}");
    }
    // The drain's retry budget: the grace the delivery derived for it
    // (the CSI node plugin stamps the tenant's grace), else the shipped
    // three attempts.
    let drain_budget = Duration::from_secs(env_u64("FLINT_SYNC_DRAIN_BUDGET_SECS", 6));
    // D11: the capability marker is written at EVERY run startup —
    // after claim, before the first poll — not inside checkout. The
    // live-tree restart row returns at `marker_present()` without
    // reaching checkout's body, so pinning the write there would
    // upgrade a fleet whose live workspaces never get the marker:
    // sentinels dead on exactly the pods the upgrade targeted.
    let posture = sc.sentinel_preflight()?;
    sc.write_capabilities(&posture)?;
    if !posture.enabled {
        eprintln!(
            "flint-sync: sentinel verbs DISABLED ({}) — the poll arm will not arm",
            posture.reason.as_deref().unwrap_or("unknown")
        );
    }
    sc.checkout_scoped(verbs::env_list("FLINT_SYNC_CHECKOUT_SCOPE")).await?;
    // RE-RUN the preflight rather than republishing the pre-checkout
    // snapshot (review: U25). Two of the preflight's inputs are written
    // BY checkout — `baseline.inst_base` wholesale, and the posture file
    // itself, from checkout's own fresher verdict — so passing the
    // `posture` computed above clobbered a newer answer with an older
    // one, and D0.4's fleet-visible verdict could advertise verbs as
    // live on the pod-replacement path. The preflight is sticky
    // (disabled stays disabled unless mode is `force`), so re-running it
    // can only narrow, never spuriously re-enable.
    let posture = sc.sentinel_preflight()?;
    sc.write_capabilities(&posture)?;
    eprintln!("flint-sync: checkout complete — agent may start");

    // The uniform crash rule (D2): a surviving pending sentinel is
    // honored, acked and retired BEFORE the poll arm may consume a
    // fresh one.
    if posture.enabled {
        if let Err(e) = sc.settle_pending_at_startup().await {
            eprintln!("flint-sync: startup settle failed (retrying at the floor): {e}");
        }
    }

    // D15's exposition, opt-in and DEGRADING. The agent container is
    // the likely occupant of any well-known port, so a collision must
    // leave the workspace fully operable — gauges.json, the lease
    // cell's echo and `flint-sync status` remain the authority for every
    // operational decision, and /metrics is additive.
    {
        let enabled = std::env::var("FLINT_SYNC_METRICS").ok().as_deref() == Some("true");
        let port: u16 = std::env::var("FLINT_SYNC_METRICS_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(9847);
        let mut posture =
            flint_lean::metrics::MetricsPosture { enabled, port, bound: false, error: None };
        if enabled {
            let labels = flint_lean::metrics::Labels {
                workspace: std::env::var("FLINT_SYNC_WORKSPACE").unwrap_or_else(|_| "unknown".into()),
                namespace: std::env::var("FLINT_SYNC_NAMESPACE").unwrap_or_else(|_| "unknown".into()),
            };
            let state_dir = sc.cfg.state_dir();
            let route = warp::get().and(warp::path("metrics")).map(move || {
                // Read the file the tick already wrote. No store, no
                // stage, no clock: a scrape costs zero bucket requests.
                let g: flint_lean::Gauges =
                    std::fs::read(state_dir.join("gauges.json"))
                        .ok()
                        .and_then(|b| serde_json::from_slice(&b).ok())
                        .unwrap_or_default();
                warp::reply::with_header(
                    flint_lean::metrics::render(&g, &labels),
                    "content-type",
                    "text/plain; version=0.0.4",
                )
            });
            let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
            match warp::serve(route).try_bind_ephemeral(addr) {
                Ok((bound, fut)) => {
                    eprintln!("flint-sync: /metrics on {bound}");
                    posture.bound = true;
                    tokio::spawn(fut);
                }
                Err(e) => {
                    eprintln!(
                        "flint-sync: /metrics NOT exposed on {addr} ({e}) — the workspace is \
                         unaffected; gauges.json and the lease cell's echo remain authoritative"
                    );
                    posture.error = Some(e.to_string());
                }
            }
        }
        if let Err(e) = sc.save_metrics_posture(&posture) {
            eprintln!("flint-sync: could not record the metrics posture: {e}");
        }
    }

    // §2.5's UDS door, opt-in. Bind failure DEGRADES: a workspace
    // whose control socket cannot be created is fully operable through
    // the file protocol, and killing the syncer over a missing
    // convenience would be a worse outcome than not having it.
    let mut ctl_rx = if std::env::var("FLINT_SYNC_UDS_DOOR").ok().as_deref() == Some("true") {
        let path = flint_lean::uds::socket_path(&sc.cfg.state_dir());
        match flint_lean::uds::bind(&path) {
            Ok(listener) => {
                let (tx, rx) = tokio::sync::mpsc::channel(16);
                tokio::spawn(flint_lean::uds::serve(listener, tx));
                eprintln!("flint-sync: control socket at {}", path.display());
                Some(rx)
            }
            Err(e) => {
                eprintln!("flint-sync: control socket NOT available ({e}) — the file protocol \
                           is unaffected");
                None
            }
        }
    } else {
        None
    };

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");

    // D3/D12: INDEPENDENT, non-resettable interval timers.
    //
    // The shipped loop recreated `sleep(floor)` inside `select!` on
    // every iteration — so a second arm completing every second would
    // win every iteration and perpetually reset the floor sleep.
    // Independent intervals make no arm's readiness able to starve
    // another. (There was a third, the writer heartbeat, until
    // 2026-09-14: nothing that fences read it, and the floor tick is
    // what probes our credentials now.)
    let floor = Duration::from_secs(sc.cfg.floor_secs.max(1));
    let poll_every = Duration::from_secs(sc.cfg.sentinel_poll_secs.max(1));

    let mut floor_iv = tokio::time::interval(floor);
    let mut poll_iv = tokio::time::interval(poll_every);
    for iv in [&mut floor_iv, &mut poll_iv] {
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        iv.reset(); // consume the immediate first tick
    }

    loop {
        tokio::select! {
            _ = floor_iv.tick() => {
                match sc.floor_tick().await {
                    Ok(o) if !o.no_change || !o.acks.is_empty() => eprintln!(
                        "flint-sync: barrier seq={:?} up={} del={} consumed={} acks={}{}",
                        o.seq, o.uploaded, o.deleted, o.consumed, o.acks.len(),
                        // Structured and greppable: this line is the
                        // only signal surface an operator has without
                        // the metrics endpoint.
                        match &o.withheld_reason {
                            Some(why) => format!(" withheld_reason={why}"),
                            None => String::new(),
                        }
                    ),
                    Ok(_) => {}
                    // A fence (deposed inside the commit section) is one
                    // more failed barrier: nothing was installed, the
                    // uploads stand, the next floor claims again.
                    Err(e) => log_retry(&sc, &e, "barrier failed (retrying next floor)"),
                }
            }
            _ = poll_iv.tick() => {
                if !posture.enabled { continue; }
                match sc.sentinel_tick().await {
                    Ok(acks) => for a in acks {
                        eprintln!(
                            "flint-sync: sentinel ack status={} boundary={} seq={:?} nonces={}",
                            a.status, a.boundary, a.seq, a.nonces.len()
                        );
                    },
                    Err(e) => log_retry(&sc, &e, "sentinel honor failed (retrying)"),
                }
            }
            // The socket's requests are served by the ONE task that
            // holds the lease and the state directory. That is what
            // makes the door sugar rather than a second writer.
            Some(req) = async { match ctl_rx.as_mut() { Some(rx) => rx.recv().await, None => None } } => {
                match req {
                    flint_lean::uds::CtlRequest::Boundary { note, reply } => {
                        let out = async {
                            sc.request_boundary(&format!("uds:{}", flint_lean_now()), note)?;
                            sc.sentinel_tick().await
                        }
                        .await;
                        let v = match out {
                            Ok(acks) => serde_json::json!({
                                "status": "ok",
                                "acks": acks.iter().map(|a| serde_json::json!({
                                    "status": a.status, "boundary": a.boundary,
                                    "seq": a.seq, "nonces": a.nonces,
                                })).collect::<Vec<_>>(),
                            }),
                            Err(e) => serde_json::json!({
                                "status": "error", "message": e.to_string(),
                            }),
                        };
                        let _ = reply.send(v);
                    }
                    flint_lean::uds::CtlRequest::Sync { reply } => {
                        let v = match sc.sync().await {
                            Ok(r) => serde_json::to_value(&r).unwrap_or_default(),
                            Err(e) => serde_json::json!({
                                "status": "error", "message": e.to_string(),
                            }),
                        };
                        let _ = reply.send(v);
                    }
                    flint_lean::uds::CtlRequest::Status { reply } => {
                        let v = match flint_lean::status_report(&sc.cfg) {
                            Ok(r) => serde_json::to_value(&r).unwrap_or_default(),
                            Err(e) => serde_json::json!({
                                "status": "error", "message": e.to_string(),
                            }),
                        };
                        let _ = reply.send(v);
                    }
                }
            }
            _ = term.recv() => {
                eprintln!("flint-sync: SIGTERM — final drain barrier");
                // D10 rule 2: bounded retry. The shipped arm made ONE
                // attempt and released the lease even on failure, so a
                // transient store error silently forfeited everything
                // since the last boundary. Now: retried for at least
                // three attempts and for as long as the budget allows;
                // and the OUTCOME is attested rather than implied
                // (audit 2026-09-03, finding 3). On success the marker
                // is written. On failure it is
                // not: the absent marker is what makes the
                // node plugin PRESERVE the tree instead of removing it
                // with the pod. A fence inside the drain's commit section
                // is one more failed attempt — the drain's barrier
                // claims again on the retry.
                let started = std::time::Instant::now();
                let mut attempt = 0u32;
                let mut last = Ok(vec![]);
                loop {
                    attempt += 1;
                    last = sc.drain().await;
                    match &last {
                        Ok(_) => break,
                        Err(e) => {
                            eprintln!("flint-sync: drain attempt {attempt} failed: {e}");
                            let again = attempt < 3
                                || started.elapsed() + Duration::from_secs(2) < drain_budget;
                            if !again { break; }
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
                match last {
                    Ok(acks) => {
                        let seq = sc.state.load_baseline().ok().map(|b| b.seq);
                        if let Err(e) = sc.state.write_drained(seq, acks.len()) {
                            eprintln!("flint-sync: drain published but its attestation could not be written: {e}");
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        eprintln!(
                            "flint-sync: drain FAILED after {attempt} attempts over {}s — no drain \
                             attestation written; the tree keeps everything since the last boundary",
                            started.elapsed().as_secs()
                        );
                        return Err(e);
                    }
                }
            }
        }
    }
}
