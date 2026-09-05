//! The legible export (design §9): a chosen ref's tree, published as a
//! lean workspace so lite, lean and passthrough readers can mount what
//! forge holds without any forge code in them.
//!
//! **Forge writes no manifest.** It materialises the tree and then runs
//! the shipped `flint-sync barrier` over it — the same binary a lean
//! sidecar runs, with lean's own ordering (upload, CAS, deletes LAST).
//! The first draft of the design described that ordering as "PUT the
//! files, delete what is gone, then write the manifest", which is
//! exactly `LeanDanglingOrder`, the mutation lean's model refutes: a
//! crash between the deletes and the manifest leaves the manifest
//! citing objects that are gone. Reusing the binary means that class of
//! mistake is not reachable from here at all.
//!
//! Two ordering rules of forge's own:
//!
//! - **The export never CASes the snapshot.** It records the commit it
//!   published in the syncer, and the NEXT batch's single CAS carries
//!   it. An export that wrote its own snapshot would be a second writer
//!   racing pushes for the one object the whole design says has one.
//! - **The export runs after the report.** It is derived data; a push
//!   is acknowledged on the strength of the pack and the snapshot, and
//!   nothing about the export may delay or fail that.
//!
//! The export is a MIRROR, never a source of truth.
//!
//! This comment used to go on: "a foreign write into its prefix is
//! overwritten by the next export, and the CRD says so." **That was
//! false**, and composition drill C3 measured it. The barrier computes
//! what to upload and delete from a LOCAL scan diffed against a LOCAL
//! baseline, consulting only the manifest pointer remotely — so an
//! object changed behind its back is in no diff, and no later export
//! repairs it. Two further commits were exported over a foreign write,
//! each republishing the file git had changed, and the foreign bytes
//! stood.
//!
//! What is true is narrower and worth stating exactly. The export
//! marks its manifests `sole_writer`, so a reader that verifies
//! against the manifest REFUSES an object that has moved off its
//! citation rather than adopting it (drill C4). A reader that does not
//! verify — a key and a GET — still takes the foreign bytes, because
//! there is nothing for it to check them against.
//!
//! So: a read-write mount over an export prefix is unsupported, and
//! the divergence it causes is detectable by manifest readers but not
//! self-healing.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::gitcmd::Git;
use super::{ForgeError, ForgeResult};

/// Where the state lean's barrier keeps its baseline lives, inside the
/// scratch tree. Skipped by lean's own scan, and skipped here when the
/// tree is cleared for a full re-materialise — losing it would only
/// cost one full re-upload, but losing it silently on every export
/// would make the export O(everything) forever.
const LEAN_STATE_DIR: &str = ".flint-sync";

#[derive(Debug, Clone)]
pub struct ExportConfig {
    /// The ref whose tree is published, full (`refs/heads/main`).
    pub reference: String,
    /// The lean workspace prefix, without a trailing slash. Always a
    /// DIFFERENT prefix from the repository's own: the export carries
    /// lean's control objects, and putting them beside `git/` would be
    /// two writers in one subtree.
    pub prefix: String,
    /// A floor, not a schedule. The export runs after a batch that
    /// moved the ref, no more often than this.
    pub every_secs: u64,
    pub bucket: String,
    pub endpoint: Option<String>,
    /// The `flint-sync` binary. It ships in the syncer's image.
    pub sync_bin: PathBuf,
    /// How long one barrier may run before it is killed.
    ///
    /// WHY THIS EXISTS AT ALL. `flint-sync`'s claim loop does not give
    /// up: a foreign holder on the export prefix leaves it sleeping and
    /// retrying forever. This call is awaited INLINE in the serving
    /// loop, so waiting on that child stops PUSHES for as long as it
    /// waits. A composition drill measured that: a read-write lean
    /// workspace mounted over the export prefix took the repository
    /// down, with nothing in the log, while the status listener went on
    /// answering Ready (design §17, C2).
    ///
    /// **It no longer costs the LEASE, and it used to.** When this
    /// comment was first written the heartbeat was a timer arm of that
    /// same `select!`, so a barrier blocked past the takeover threshold
    /// let the token go quiet and a challenger deposed a working pod —
    /// a 300 s default timeout against a 60 s threshold, five times
    /// over. The renewer is its own task now (`lease::spawn_renewer`,
    /// and see the `select!`'s own comment in `server.rs`), and it
    /// renews through an export because `Phase::Serving` is not a phase
    /// that must progress. What a blocked export costs today is pushes,
    /// bounded by this timeout and then by `backoff_secs`; reads keep
    /// serving and the repository stays claimed.
    ///
    /// The default is the export floor's default. An export that
    /// cannot finish within the interval between exports can never
    /// keep up regardless, so that is the point past which waiting
    /// buys nothing.
    pub timeout_secs: u64,
    /// The materialised tree, and the index that makes updating it
    /// incremental.
    pub root: PathBuf,
    pub index: PathBuf,
    /// Stamped into the export's claim cell check, so a prefix another
    /// project claims is refused rather than overwritten.
    pub project_id: Option<String>,
}

/// What the syncer last published, and when. Persisted beside the
/// scratch tree so a restart does not re-export, and so the
/// incremental update knows which tree it is coming FROM.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Record {
    pub commit: Option<String>,
    pub unix: u64,
    /// When a barrier was last abandoned on the timeout, or 0. Read by
    /// `plan` as a floor of its own. `#[serde(default)]` so a record
    /// written before this field parses rather than resetting the
    /// export to "never ran".
    #[serde(default)]
    pub blocked_unix: u64,
    /// Consecutive abandoned barriers. Drives the hold-off ladder, and
    /// is reset by any export that publishes.
    #[serde(default)]
    pub blocked_streak: u32,
}

/// How long to hold the export off after `streak` consecutive
/// abandoned barriers.
///
/// A FLAT hold-off of one timeout is not enough, and the drill that
/// found the wedge measured why: the blocker is a misconfiguration
/// that stands until somebody fixes it, so forge would spend one
/// timeout blocked out of every two — a 50% outage, politely paced.
/// Doubling makes the first failure cost one timeout and every later
/// one cost almost nothing, which is the right shape for a fault that
/// only a human can clear.
pub fn backoff_secs(cfg: &ExportConfig, streak: u32) -> u64 {
    const CAP_SECS: u64 = 3600;
    let shift = streak.saturating_sub(1).min(6);
    cfg.timeout_secs.saturating_mul(1u64 << shift).min(CAP_SECS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Skip(String),
    /// `from` is the previously exported commit, which makes the
    /// working-tree update a two-tree merge — the only form that
    /// DELETES a path the new tree no longer has.
    Run { from: Option<String>, to: String },
}

/// Should this export run, and from where?
///
/// Pure, and separated from the doing because the interesting failures
/// are here: exporting a commit already exported is wasted work, and
/// exporting on every push regardless of the floor is how a repository
/// under a burst spends its whole batch budget on derived data.
pub fn plan(cfg: &ExportConfig, head: Option<&str>, last: &Record, now: u64) -> Plan {
    let Some(head) = head else {
        return Plan::Skip(format!("{} does not exist in this repository", cfg.reference));
    };
    if last.commit.as_deref() == Some(head) {
        return Plan::Skip(format!("{head} is already exported"));
    }
    // A barrier abandoned on the timeout waits out a floor of its own
    // before it is tried again. Without this the timeout only changes
    // the shape of the outage: the serving loop would re-enter the
    // doomed barrier on the very next batch and spend the whole
    // timeout again, and again, for as long as the blocker stands.
    let hold = backoff_secs(cfg, last.blocked_streak);
    if last.blocked_unix > 0 && now.saturating_sub(last.blocked_unix) < hold {
        return Plan::Skip(format!(
            "the last barrier was abandoned on the {}s timeout {}s ago ({} in a row); \
             holding off for {hold}s",
            cfg.timeout_secs,
            now.saturating_sub(last.blocked_unix),
            last.blocked_streak
        ));
    }
    if last.unix > 0 && now.saturating_sub(last.unix) < cfg.every_secs {
        return Plan::Skip(format!(
            "inside the {}s floor ({}s since the last export)",
            cfg.every_secs,
            now.saturating_sub(last.unix)
        ));
    }
    Plan::Run { from: last.commit.clone(), to: head.to_string() }
}

fn record_path(cfg: &ExportConfig) -> PathBuf {
    cfg.root.with_extension("record.json")
}

pub fn load_record(cfg: &ExportConfig) -> Record {
    std::fs::read(record_path(cfg))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_record(cfg: &ExportConfig, r: &Record) -> ForgeResult<()> {
    if let Some(parent) = record_path(cfg).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(r)
        .map_err(|e| ForgeError::State(format!("export record will not serialise: {e}")))?;
    std::fs::write(record_path(cfg), body)?;
    Ok(())
}

/// Everything in the scratch tree except lean's own state directory.
fn clear_tree(root: &Path) -> ForgeResult<()> {
    let rd = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in rd {
        let entry = entry?;
        if entry.file_name() == LEAN_STATE_DIR {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Bring the scratch tree to `to`, incrementally when we can.
///
/// The two-tree `read-tree -m -u <from> <to>` is the whole reason the
/// previous commit is tracked: it is the only form that touches exactly
/// the paths that changed AND deletes the ones the new tree no longer
/// has. `git archive | tar -x`, which the design first described, does
/// neither — it rewrites every file, so the next barrier would see the
/// whole tree as touched, and it leaves deleted paths behind, so the
/// export would publish files the ref no longer contains.
///
/// The full path clears the tree first, for the same reason: a stale
/// file left behind is a file the export publishes forever.
pub async fn materialize(git: &Git, cfg: &ExportConfig, from: Option<&str>, to: &str) -> ForgeResult<()> {
    std::fs::create_dir_all(&cfg.root)?;
    if let Some(parent) = cfg.index.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let root = cfg.root.to_string_lossy().into_owned();
    let index = cfg.index.to_string_lossy().into_owned();

    let incremental = from.is_some() && cfg.index.exists();
    if incremental {
        let from = from.expect("checked");
        let out = git
            .run_env(
                &["--work-tree", &root, "read-tree", "-m", "-u", from, to],
                &[("GIT_INDEX_FILE", index.as_str())],
            )
            .await?;
        if out.ok() {
            return Ok(());
        }
        // A two-tree merge refuses when the working tree has drifted —
        // a half-finished export, or someone poking at the scratch. The
        // full path is always available and always correct, so this is
        // a fallback rather than a failure.
        eprintln!(
            "flint-forge: incremental export update refused ({}); re-materialising the whole tree",
            out.stderr.trim()
        );
    }

    clear_tree(&cfg.root)?;
    let _ = std::fs::remove_file(&cfg.index);
    git.run_env(
        &["--work-tree", &root, "read-tree", to],
        &[("GIT_INDEX_FILE", index.as_str())],
    )
    .await
    .and_then(|o| {
        if o.ok() {
            Ok(())
        } else {
            Err(ForgeError::Git(format!("read-tree {to}: {}", o.stderr.trim())))
        }
    })?;
    // `-u` is load-bearing, not tidiness: it records the stat data of
    // each file it writes into the index. Without it the index knows
    // the content but not the mtime, and the NEXT two-tree update
    // refuses with "Entry '<path>' not uptodate. Cannot merge." — so
    // every export would fall back to the full path, re-materialise the
    // whole tree, and make lean's next scan re-upload all of it. The
    // export would have been O(everything) forever, with only a log
    // line to say so. Found by the incremental test.
    let out = git
        .run_env(
            &["--work-tree", &root, "checkout-index", "-a", "-f", "-u"],
            &[("GIT_INDEX_FILE", index.as_str())],
        )
        .await?;
    if !out.ok() {
        return Err(ForgeError::Git(format!("checkout-index: {}", out.stderr.trim())));
    }
    Ok(())
}

/// The command that publishes the tree. Built by a function rather than
/// inline so a test can assert on it: everything load-bearing about the
/// export is in this environment, and a missing variable would show up
/// as a workspace published to the wrong prefix.
pub fn barrier_command(cfg: &ExportConfig) -> (PathBuf, Vec<String>, Vec<(String, String)>) {
    let mut env = vec![
        ("FLINT_SYNC_BUCKET".to_string(), cfg.bucket.clone()),
        ("FLINT_SYNC_PREFIX".to_string(), cfg.prefix.clone()),
        ("FLINT_SYNC_ROOT".to_string(), cfg.root.to_string_lossy().into_owned()),
    ];
    if let Some(e) = cfg.endpoint.as_deref().filter(|e| !e.is_empty()) {
        env.push(("FLINT_SYNC_ENDPOINT".to_string(), e.to_string()));
    }
    // The export IS a mirror: forge is the only party entitled to
    // write this prefix, and everything in it is derived from a commit.
    // Saying so in the manifest is what lets a later reader tell a
    // foreign write apart from a legitimate one — without it, lean's
    // default arm adopts the stranger's bytes into the reader's tree
    // and reports success (drill C4).
    env.push(("FLINT_SYNC_SOLE_WRITER".to_string(), "true".to_string()));
    if let Some(p) = cfg.project_id.as_deref().filter(|p| !p.is_empty()) {
        // The same refuse-foreign rule the syncer takes on its own
        // prefix: an export must not overwrite another project's
        // workspace just because someone typed its prefix.
        env.push(("FLINT_SYNC_PROJECT_ID".to_string(), p.to_string()));
    }
    (cfg.sync_bin.clone(), vec!["barrier".to_string()], env)
}

/// Run one barrier over the materialised tree.
///
/// The AWS credentials are inherited, not passed: the syncer and the
/// export publish to the same bucket under the same principal, and
/// re-deriving them here would be a second place for them to be wrong.
pub async fn run_barrier(cfg: &ExportConfig) -> ForgeResult<()> {
    let (bin, args, env) = barrier_command(cfg);
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Load-bearing with the timeout below. The timeout drops the
        // future that owns the child; without this the abandoned
        // barrier would keep running, keep holding whatever it managed
        // to claim, and be joined by a fresh one on the next attempt.
        .kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().map_err(|e| {
        ForgeError::State(format!("cannot exec {} barrier: {e}", bin.display()))
    })?;
    let out = match tokio::time::timeout(
        std::time::Duration::from_secs(cfg.timeout_secs),
        child.wait_with_output(),
    )
    .await
    {
        Ok(r) => r.map_err(|e| {
            ForgeError::State(format!("{} barrier: {e}", bin.display()))
        })?,
        Err(_) => {
            // Named rather than generic: this is the one failure whose
            // cause is almost always a second writer, and the operator
            // needs to be sent to the cell rather than to the export.
            return Err(ForgeError::ExportBlocked(format!(
                "the barrier over {} did not finish within {}s and was killed. The usual \
                 cause is a SECOND WRITER standing on the export prefix — `flint-sync` \
                 waits for a foreign lease forever — so check who holds \
                 {}/.flint/lean/epoch. Nothing was published; the export backs off and \
                 retries.",
                cfg.prefix, cfg.timeout_secs, cfg.prefix
            )));
        }
    };
    if !out.status.success() {
        return Err(ForgeError::State(format!(
            "flint-sync barrier exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    // The barrier's own summary line is worth keeping: it is the only
    // place the object and chunk counts of an export appear.
    let summary = String::from_utf8_lossy(&out.stderr);
    for line in summary.lines().filter(|l| l.contains("barrier")) {
        eprintln!("flint-forge: export — {}", line.trim());
    }
    Ok(())
}

/// One export, if one is due. Returns the commit published, which the
/// caller stashes for the NEXT snapshot CAS.
/// `flint-sync`'s baseline — the etag it last published for each file.
///
/// WHY THIS HAS TO BE IN THE BUCKET. lean protects a workspace from a
/// second writer by making every upload conditional: if the object's
/// etag is not the one it last wrote, it PARKS the file rather than
/// overwriting a stranger's bytes. That is exactly right for lean,
/// where the workspace lives on a volume and the baseline survives with
/// it.
///
/// Forge's export has no volume. The baseline sat in the export tree on
/// the pod's `emptyDir`, so the FIRST restart destroyed it — and then
/// every object in the bucket looked foreign, every upload 412'd, and
/// the export parked all of them. Permanently: nothing rebuilds a
/// baseline, so the published workspace froze at whatever it held
/// before that restart while `main` moved on. The first cluster run
/// found `README.md` still holding the very first seed commit's text
/// with 164 files parked and `up=0`.
///
/// So the baseline goes where every other durable thing forge owns
/// goes.
fn baseline_path(cfg: &ExportConfig) -> PathBuf {
    cfg.root.join(".flint-sync").join("baseline.json")
}

/// Put the saved baseline back before the first barrier of a new pod.
/// Absent locally and present in the bucket is exactly the restart case.
pub async fn rehydrate_baseline(
    store: &dyn flint_store::ObjectStore,
    key: &str,
    cfg: &ExportConfig,
) -> ForgeResult<bool> {
    let path = baseline_path(cfg);
    if path.exists() {
        return Ok(false);
    }
    match store.get_whole(key, None).await {
        Ok((_, body)) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, &body)?;
            eprintln!(
                "flint-forge: export baseline rehydrated from the bucket ({} bytes)",
                body.len()
            );
            Ok(true)
        }
        Err(flint_store::StoreError::NotFound(_)) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Save it again after a barrier that succeeded.
///
/// Ordering is deliberate: the baseline is written only AFTER the
/// barrier reports success, so a saved baseline never claims to have
/// published something that was not. The reverse — dying between the
/// barrier and this write — leaves a baseline one generation stale,
/// and those files park once and are re-adopted on the pass after.
pub async fn preserve_baseline(
    store: &dyn flint_store::ObjectStore,
    key: &str,
    cfg: &ExportConfig,
    epoch: u64,
) -> ForgeResult<()> {
    let path = baseline_path(cfg);
    if !path.exists() {
        return Ok(());
    }
    let body = std::fs::read(&path)?;
    super::packio::put_small(store, key, body, epoch).await
}

pub async fn maybe_run(git: &Git, cfg: &ExportConfig, now: u64) -> ForgeResult<Option<String>> {
    let head = git.ref_oid(&cfg.reference).await?;
    let last = load_record(cfg);
    match plan(cfg, head.as_deref(), &last, now) {
        Plan::Skip(why) => {
            // Not an error and not silence: an export that never runs
            // because of a floor nobody remembers setting is exactly
            // the thing an operator needs to be able to see.
            eprintln!("flint-forge: export skipped — {why}");
            Ok(None)
        }
        Plan::Run { from, to } => {
            materialize(git, cfg, from.as_deref(), &to).await?;
            if let Err(e) = run_barrier(cfg).await {
                if matches!(e, ForgeError::ExportBlocked(_)) {
                    // Stamped with the time the barrier was ABANDONED,
                    // not the `now` this call was given: that one was
                    // read before the barrier started, so a hold-off
                    // measured from it is consumed by the very timeout
                    // it is supposed to follow. The first version of
                    // this fix made exactly that mistake and the drill
                    // caught it — the hold-off existed and was already
                    // expired every time it was read.
                    //
                    // Failing to write the record only costs an earlier
                    // retry, so it is logged rather than raised over
                    // the error the caller actually needs to see.
                    let mut r = last.clone();
                    r.blocked_unix = super::now_unix();
                    r.blocked_streak = last.blocked_streak.saturating_add(1);
                    if let Err(e2) = save_record(cfg, &r) {
                        eprintln!("flint-forge: export backoff not recorded: {e2}");
                    }
                }
                return Err(e);
            }
            // A published export clears the backoff: the blocker is
            // demonstrably gone.
            save_record(cfg, &Record {
                commit: Some(to.clone()),
                unix: now,
                blocked_unix: 0,
                blocked_streak: 0,
            })?;
            Ok(Some(to))
        }
    }
}
