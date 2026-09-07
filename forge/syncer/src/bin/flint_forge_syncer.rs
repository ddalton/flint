//! flint-forge-syncer: the one process that owns a repository's writes
//! (design of record: `docs/plans/flint-forge-design.md` §4).
//!
//! It is the pod's MAIN container: its exit restarts the pod, which is
//! what makes a fence recoverable — a deposed server stops answering
//! fetches, restarts, and restores from the snapshot rather than
//! serving refs it can no longer prove.
//!
//! Environment:
//!   FLINT_FORGE_BUCKET     (required) bucket name
//!   FLINT_FORGE_PREFIX     (required) this repository's key prefix
//!   FLINT_FORGE_REPO       (required) the bare repository on local disk
//!   FLINT_FORGE_ENDPOINT   S3 endpoint override (MinIO/proxy rigs)
//!   FLINT_FORGE_PROJECT_ID refuse a prefix another project claims
//!   FLINT_FORGE_SOCKET     hook socket (default <repo>/flint-forge/syncer.sock)
//!   FLINT_FORGE_STATUS_ADDR   status listener (default 127.0.0.1:9848)
//!   FLINT_FORGE_HEARTBEAT_SECS   lease renewal period (default 10)
//!   FLINT_FORGE_BATCH_WINDOW_MS  a fixed wait for more pushes once one arrived (default 0: a batch is what queued while the last one ran)
//!   FLINT_FORGE_FOLD_FACTOR      compaction tiers: git's geometric factor over pack bytes (default 2; 0 = the full repack at REPACK_THRESHOLD)
//!   FLINT_FORGE_BASE_TIER_PERCENT / BASE_MIN_MIB / BASE_REBUILD_MIN_SECS   when the base is rebuilt (50 / 64 / 3600)
//!   FLINT_FORGE_FOLD_RETAIN_SECS / FOLD_STALL_SECS / SWEEP_EVERY_SECS      retention for readers, the stall bound, the full sweep's cadence (900 / 300 / 3600)
//!   FLINT_FORGE_FOLD_MIN_MIB / FOLD_MAX_PACKS   a floor under a tier fold and a cap on the tier count (256 / 64)
//!   FLINT_FORGE_BATCH_MAX        pushes per batch (default 64)
//!   FLINT_FORGE_REPACK_THRESHOLD packs before a repack (default 24)
//!   FLINT_FORGE_ORPHAN_GRACE_SECS  sweep grace (default 3600)
//!   FLINT_FORGE_UNDO_WINDOW_SECS / UNDO_MAX_POINTS  how long a force-pushed state stays recoverable, and how many
//!                                  points a sweep reads (604800 / 64; 0 = undo off)
//!   FLINT_FORGE_FANOUT           pack PUTs / ranged GETs in flight (default 4)
//!   FLINT_FORGE_DEFAULT_BRANCH   HEAD for an empty repository (main)
//!   FLINT_FORGE_HOOKS_PATH       core.hooksPath (the hooks ship in the
//!                                git image, not in the repository)
//!   FLINT_FORGE_POLICY_DIR       where the rendered branch policy is
//!                                re-read from between batches
//!   FLINT_FORGE_PROTECTED        comma-separated globs, no direct push
//!   FLINT_FORGE_ALLOW_NON_FF     comma-separated globs, force allowed
//!   FLINT_FORGE_EXPORT_REF       the ref whose tree is exported
//!   FLINT_FORGE_EXPORT_PREFIX    the lean workspace prefix it goes to
//!   FLINT_FORGE_EXPORT_EVERY_SECS  floor between exports (default 300)
//!   FLINT_FORGE_EXPORT_TIMEOUT_SECS  how long one export barrier may
//!                                run before it is killed (default 300).
//!                                It is inline in the serving loop, so
//!                                this also bounds how long a blocked
//!                                export can stop pushes (design §17).
//!   FLINT_FORGE_SYNC_BIN         flint-sync (default /usr/local/bin/flint-sync)
//!   FLINT_FORGE_BUNDLES          "true" arms clone bundles (§8)
//!   FLINT_FORGE_BUNDLE_EVERY_SECS   floor between cuts (default 3600)
//!   FLINT_FORGE_BUNDLE_URL_TTL_SECS presigned URL lifetime (default 21600)
//!   FLINT_FORGE_PRUNE_PATTERN    refs eligible for pruning, e.g. agent/*
//!   FLINT_FORGE_PRUNE_AFTER_SECS how long a MERGED branch must be quiet
//!   FLINT_FORGE_PRUNE_EVERY_SECS how often the pass runs (default 86400)
//!   FLINT_FORGE_LFS              "true" arms the git-LFS batch API
//!   FLINT_FORGE_LFS_TTL_SECS     transfer URL lifetime (default 3600)

use std::path::PathBuf;
use std::sync::Arc;

use flint_forge::bundle::BundleConfig;
use flint_forge::export::ExportConfig;
use flint_forge::prune::PruneConfig;
use flint_forge::policy::Policy;
use flint_forge::server::{self, LfsOpts, ServerOpts};
use flint_forge::{ForgeConfig, ForgeError, Syncer, EXIT_REFUSED};
use flint_store::s3::S3Store;
use flint_store::ObjectStore;

fn env_req(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        eprintln!("flint-forge-syncer: {name} is required");
        std::process::exit(2);
    })
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_globs(name: &str) -> Vec<String> {
    std::env::var(name)
        .ok()
        .map(|v| {
            v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
        })
        .unwrap_or_default()
}

fn rendered_policy(dir: &std::path::Path) -> Option<Policy> {
    match Policy::load(dir) {
        Ok(p) => p,
        Err(e) => {
            // A policy the enforcers cannot read must never read as
            // "no policy": that is how a rendering bug becomes an open
            // repository.
            eprintln!("flint-forge-syncer: {e}");
            std::process::exit(EXIT_REFUSED);
        }
    }
}

fn main() {
    // One binary, three names. Invoked as `pre-receive` or
    // `proc-receive` — the git image's hook symlinks point here — it
    // is the hook, and it exits before it has looked at a single
    // syncer variable. The hook in a pod is then the same build as the
    // syncer it talks to, so the socket protocol between the two
    // containers cannot drift between two image tags.
    let args: Vec<String> = std::env::args().collect();
    if let Some(role) = flint_forge::hook::role_of(&args) {
        std::process::exit(flint_forge::hook::run_hook(role));
    }
    // A read-only listing of what a force-push left recoverable (X15).
    // Its own process, no lease and no writes, so an operator can run
    // it beside the serving one: `kubectl exec … -- flint-forge-syncer
    // --undo-list [<ref>]`.
    if args.iter().any(|a| a == "--undo-list") {
        let want = args.iter().skip_while(|a| *a != "--undo-list").nth(1).cloned();
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(undo_list(want));
        return;
    }
    // The same shape for the batch log: what changed, per snapshot seq,
    // which is what a follower reads and what an operator wants when a
    // repository looks like it moved and nobody says when.
    if args.iter().any(|a| a == "--log-list") {
        let want = args
            .iter()
            .skip_while(|a| *a != "--log-list")
            .nth(1)
            .and_then(|v| v.parse::<usize>().ok());
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(log_list(want.unwrap_or(20)));
        return;
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(serve());
}

async fn serve() {
    let bucket_name = env_req("FLINT_FORGE_BUCKET");
    let prefix = env_req("FLINT_FORGE_PREFIX");
    let repo = PathBuf::from(env_req("FLINT_FORGE_REPO"));
    let endpoint = std::env::var("FLINT_FORGE_ENDPOINT").ok();

    let store = match S3Store::connect(bucket_name.clone(), endpoint.clone()).await {
        Ok(s) => Arc::new(s) as Arc<dyn ObjectStore>,
        Err(e) => {
            eprintln!("flint-forge-syncer: store connect: {e}");
            std::process::exit(1);
        }
    };

    let mut cfg = ForgeConfig::new(&prefix, &repo);
    cfg.heartbeat_secs = env_u64("FLINT_FORGE_HEARTBEAT_SECS", 10).max(1);
    cfg.batch_window_ms = env_u64("FLINT_FORGE_BATCH_WINDOW_MS", 0);
    cfg.batch_max = env_u64("FLINT_FORGE_BATCH_MAX", 64).max(1) as usize;
    cfg.repack_threshold = env_u64("FLINT_FORGE_REPACK_THRESHOLD", 24) as usize;
    // Compaction tiers (X18). FOLD_FACTOR=0 is the control: the shipped
    // full repack at REPACK_THRESHOLD packs.
    cfg.fold_factor = env_u64("FLINT_FORGE_FOLD_FACTOR", 2);
    cfg.base_tier_percent = env_u64("FLINT_FORGE_BASE_TIER_PERCENT", 50).max(1);
    cfg.base_min_bytes = env_u64("FLINT_FORGE_BASE_MIN_MIB", 64) * 1024 * 1024;
    cfg.base_rebuild_min_secs = env_u64("FLINT_FORGE_BASE_REBUILD_MIN_SECS", 3600);
    cfg.fold_retain_secs = env_u64("FLINT_FORGE_FOLD_RETAIN_SECS", 900);
    cfg.fold_stall_secs = env_u64("FLINT_FORGE_FOLD_STALL_SECS", 300).max(1);
    cfg.sweep_every_secs = env_u64("FLINT_FORGE_SWEEP_EVERY_SECS", 3600);
    cfg.fold_min_bytes = env_u64("FLINT_FORGE_FOLD_MIN_MIB", 256) * 1024 * 1024;
    cfg.fold_max_packs = env_u64("FLINT_FORGE_FOLD_MAX_PACKS", 64).max(2) as usize;
    cfg.orphan_grace_secs = env_u64("FLINT_FORGE_ORPHAN_GRACE_SECS", 3600);
    // X15: how long a destructive push's predecessor state is kept.
    cfg.undo_window_secs = env_u64("FLINT_FORGE_UNDO_WINDOW_SECS", 7 * 24 * 3600);
    cfg.undo_max_points = env_u64("FLINT_FORGE_UNDO_MAX_POINTS", 64).max(1) as usize;
    // X15's second half and X14's cheap half: the batch log a follower
    // reads, and whether a server waiting for another's lease brings
    // the repository down while it waits. 0 entries is the control —
    // no log, and a wake that pays for the whole repository.
    cfg.log_max_entries = env_u64("FLINT_FORGE_LOG_MAX_ENTRIES", 512) as usize;
    // The dumb protocol's derived files, off the push path (X19's
    // cheap half). 0 restores the shipped behaviour, once per batch.
    cfg.derived_every_secs = env_u64("FLINT_FORGE_DERIVED_EVERY_SECS", 60);
    cfg.prewarm = env_u64("FLINT_FORGE_PREWARM", 1) != 0;
    cfg.prewarm_resync_secs = env_u64("FLINT_FORGE_PREWARM_RESYNC_SECS", 300);
    cfg.fanout = env_u64("FLINT_FORGE_FANOUT", 4).max(1) as usize;
    cfg.project_id = std::env::var("FLINT_FORGE_PROJECT_ID").ok().filter(|p| !p.is_empty());
    cfg.default_branch =
        std::env::var("FLINT_FORGE_DEFAULT_BRANCH").unwrap_or_else(|_| "main".into());
    cfg.hooks_path = std::env::var("FLINT_FORGE_HOOKS_PATH").ok().filter(|p| !p.is_empty());

    let policy_dir = std::env::var("FLINT_FORGE_POLICY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| cfg.state_dir.clone());
    let socket = std::env::var("FLINT_FORGE_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| cfg.state_dir.join(flint_forge::uds::SOCKET_NAME));
    let status_addr = std::env::var("FLINT_FORGE_STATUS_ADDR")
        .ok()
        .filter(|a| !a.is_empty() && a != "off")
        .or_else(|| Some("127.0.0.1:9848".to_string()));

    // The export is off unless a ref AND a prefix are both named. Half
    // a configuration is a configuration error, not a default: an
    // export with no prefix would publish into the repository's own.
    let export = match (
        std::env::var("FLINT_FORGE_EXPORT_REF").ok().filter(|r| !r.is_empty()),
        std::env::var("FLINT_FORGE_EXPORT_PREFIX").ok().filter(|p| !p.is_empty()),
    ) {
        (Some(r), Some(p)) => {
            let reference = if r.starts_with("refs/") { r } else { format!("refs/heads/{r}") };
            let prefix = p.trim_end_matches('/').to_string();
            if prefix == cfg.prefix {
                eprintln!(
                    "flint-forge-syncer: FLINT_FORGE_EXPORT_PREFIX is this repository's own                      prefix; the export is a separate lean workspace and would be a second                      writer under git/"
                );
                std::process::exit(EXIT_REFUSED);
            }
            let base = cfg.state_dir.join("export");
            Some(ExportConfig {
                reference,
                prefix,
                every_secs: env_u64("FLINT_FORGE_EXPORT_EVERY_SECS", 300),
                timeout_secs: env_u64("FLINT_FORGE_EXPORT_TIMEOUT_SECS", 300).max(1),
                bucket: bucket_name.clone(),
                endpoint: endpoint.clone(),
                sync_bin: PathBuf::from(
                    std::env::var("FLINT_FORGE_SYNC_BIN")
                        .unwrap_or_else(|_| "/usr/local/bin/flint-sync".into()),
                ),
                root: base.join("tree"),
                index: base.join("index"),
                project_id: cfg.project_id.clone(),
            })
        }
        (None, None) => None,
        _ => {
            eprintln!(
                "flint-forge-syncer: the export needs BOTH FLINT_FORGE_EXPORT_REF and                  FLINT_FORGE_EXPORT_PREFIX"
            );
            std::process::exit(EXIT_REFUSED);
        }
    };

    // Clone bundles: off unless armed. They cost a full copy of the
    // repository per cut, and they are inert unless the agent image
    // also sets `transfer.bundleURI=true` — which is why the guide
    // says so and why this is opt-in rather than a default.
    let bundle = std::env::var("FLINT_FORGE_BUNDLES")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
        .then(|| BundleConfig {
            every_secs: env_u64("FLINT_FORGE_BUNDLE_EVERY_SECS", 3600),
            url_ttl_secs: env_u64("FLINT_FORGE_BUNDLE_URL_TTL_SECS", 6 * 3600),
        });

    // Pruning: off unless BOTH a pattern and a TTL are given. A default
    // TTL would be a clock deleting branches nobody asked it to.
    let prune = match (
        std::env::var("FLINT_FORGE_PRUNE_PATTERN").ok().filter(|p| !p.is_empty()),
        std::env::var("FLINT_FORGE_PRUNE_AFTER_SECS").ok().and_then(|v| v.parse::<u64>().ok()),
    ) {
        (Some(pattern), Some(after_secs)) => Some(PruneConfig {
            pattern: if pattern.starts_with("refs/") {
                pattern
            } else {
                format!("refs/heads/{pattern}")
            },
            after_secs,
            into: format!("refs/heads/{}", cfg.default_branch.trim_start_matches("refs/heads/")),
            every_secs: env_u64("FLINT_FORGE_PRUNE_EVERY_SECS", 86_400),
        }),
        (None, None) => None,
        _ => {
            eprintln!(
                "flint-forge-syncer: pruning needs BOTH FLINT_FORGE_PRUNE_PATTERN and \
                 FLINT_FORGE_PRUNE_AFTER_SECS"
            );
            std::process::exit(EXIT_REFUSED);
        }
    };

    // Git LFS: off unless armed. The bytes never come through this
    // process — the batch response hands the client a presigned URL —
    // but the API lives here because this is where the bucket
    // credentials are.
    let lfs = std::env::var("FLINT_FORGE_LFS")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
        .then(|| LfsOpts {
            ttl_secs: env_u64("FLINT_FORGE_LFS_TTL_SECS", flint_forge::lfs::DEFAULT_TTL_SECS),
        });

    let opts = ServerOpts {
        socket,
        status_addr,
        // The rendered document beside the repository is the operator's
        // surface; the env knobs are the pre-operator posture and the
        // rigs'. A file, when present, wins outright rather than
        // merging — a policy assembled from two sources is a policy
        // nobody can read off one screen.
        policy_dir: Some(policy_dir.clone()),
        policy: rendered_policy(&policy_dir).unwrap_or(Policy {
            protected: env_globs("FLINT_FORGE_PROTECTED"),
            allow_non_fast_forward: env_globs("FLINT_FORGE_ALLOW_NON_FF"),
            ..Policy::default()
        }),
        export,
        bundle,
        prune,
        lfs,
    };

    let holder_id = format!("forge-{}", uuid::Uuid::new_v4());
    let sc = Syncer::new(store, cfg, holder_id);

    match server::run(sc, opts).await {
        Ok(()) => {}
        // A refusal is final: the delivery must tear this repository
        // down and name the reason rather than restart into the same
        // wall, which a tenant reads as "starting" for as long as it
        // lives (lean's leg S22).
        Err(e @ ForgeError::Refused(_)) => {
            eprintln!("flint-forge-syncer: {e}");
            std::process::exit(EXIT_REFUSED);
        }
        // A fence IS restartable, and restarting is the right answer:
        // the successor restores from the snapshot it was deposed by.
        Err(e) => {
            eprintln!("flint-forge-syncer: {e}");
            std::process::exit(1);
        }
    }
}

/// `--undo-list [<ref>]`: the states a destructive push left behind,
/// newest first, and for each the refs it held. With a ref name, only
/// the points whose value for that ref differs from the live snapshot's
/// — which is the question an operator actually has ("what was this
/// branch before?"). Reads the bucket and nothing else.
async fn undo_list(want: Option<String>) {
    let bucket_name = env_req("FLINT_FORGE_BUCKET");
    let prefix = env_req("FLINT_FORGE_PREFIX");
    let repo = PathBuf::from(std::env::var("FLINT_FORGE_REPO").unwrap_or_else(|_| "/tmp".into()));
    let endpoint = std::env::var("FLINT_FORGE_ENDPOINT").ok();
    let store = match S3Store::connect(bucket_name, endpoint).await {
        Ok(s) => Arc::new(s) as Arc<dyn ObjectStore>,
        Err(e) => {
            eprintln!("flint-forge-syncer: store connect: {e}");
            std::process::exit(1);
        }
    };
    let cfg = ForgeConfig::new(&prefix, &repo);
    let live = match flint_forge::snapshot::load(store.as_ref(), &cfg).await {
        Ok(c) => c.snap,
        Err(e) => {
            eprintln!("flint-forge-syncer: snapshot: {e}");
            std::process::exit(1);
        }
    };
    let points = match flint_forge::undo::list(store.as_ref(), &cfg, 256).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("flint-forge-syncer: undo list: {e}");
            std::process::exit(1);
        }
    };
    if points.is_empty() {
        println!("no undo points: no destructive push has landed inside the window");
        return;
    }
    for p in &points {
        let when = p.unix.map(|t| t.to_string()).unwrap_or_else(|| "?".into());
        match &want {
            Some(name) => {
                let full = if name.starts_with("refs/") {
                    name.clone()
                } else {
                    format!("refs/heads/{name}")
                };
                let then = p.snap.refs.get(&full);
                let now = live.refs.get(&full);
                if then == now {
                    continue;
                }
                println!(
                    "seq {:<6} unix {:<12} {full}: {} (live: {})",
                    p.seq,
                    when,
                    then.map(|s| s.as_str()).unwrap_or("absent"),
                    now.map(|s| s.as_str()).unwrap_or("absent")
                );
            }
            None => {
                println!(
                    "seq {:<6} unix {:<12} {} ref(s), {} pack(s), written by {}",
                    p.seq,
                    when,
                    p.snap.refs.len(),
                    p.snap.packs.len(),
                    p.snap.writer
                );
            }
        }
    }
    println!(
        "\nthese states are kept while their point stands; their packs are held with them. \
         To put one back, set the ref to the oid above through a push from a clone that has it, \
         or restore this repository from the point's pack list."
    );
}

/// `--log-list [<n>]`: the last n batch-log entries, oldest first. No
/// lease, no writes; it reads the entries and nothing else, so it is
/// safe beside a serving syncer and it answers "what moved, and when"
/// without downloading the snapshot's whole ref map.
async fn log_list(n: usize) {
    let bucket_name = env_req("FLINT_FORGE_BUCKET");
    let prefix = env_req("FLINT_FORGE_PREFIX");
    let repo = PathBuf::from(std::env::var("FLINT_FORGE_REPO").unwrap_or_else(|_| "/tmp".into()));
    let endpoint = std::env::var("FLINT_FORGE_ENDPOINT").ok();
    let store = match S3Store::connect(bucket_name, endpoint).await {
        Ok(s) => Arc::new(s) as Arc<dyn ObjectStore>,
        Err(e) => {
            eprintln!("flint-forge-syncer: store connect: {e}");
            std::process::exit(1);
        }
    };
    let cfg = ForgeConfig::new(&prefix, &repo);
    let seqs = match flint_forge::log::seqs(store.as_ref(), &cfg).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("flint-forge-syncer: log list: {e}");
            std::process::exit(1);
        }
    };
    if seqs.is_empty() {
        println!(
            "no log entries: either nothing has been pushed, or FLINT_FORGE_LOG_MAX_ENTRIES is 0"
        );
        return;
    }
    let from = seqs.len().saturating_sub(n);
    for seq in &seqs[from..] {
        match flint_forge::log::read(store.as_ref(), &cfg, *seq).await {
            Ok(Some(e)) => {
                let refs: Vec<String> = e
                    .refs
                    .iter()
                    .map(|(k, v)| {
                        if v.is_empty() {
                            format!("{k} deleted")
                        } else {
                            format!("{k} {}", &v[..v.len().min(12)])
                        }
                    })
                    .collect();
                println!(
                    "seq {:<6} unix {:<12} +{} pack(s) -{} pack(s)  {}",
                    e.seq,
                    e.unix,
                    e.packs_added.len(),
                    e.packs_removed.len(),
                    if refs.is_empty() { "(no ref moved)".to_string() } else { refs.join(", ") }
                );
            }
            Ok(None) => println!("seq {seq:<6} (unreadable — a follower treats this as a gap)"),
            Err(e) => println!("seq {seq:<6} ({e})"),
        }
    }
    println!(
        "\n{} entrie(s) in the bucket, seq {}..{}. A follower reads forward from the seq it \
         stands at; a hole sends it back to the snapshot.",
        seqs.len(),
        seqs[0],
        seqs[seqs.len() - 1]
    );
}
