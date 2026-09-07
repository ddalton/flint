//! Compaction tiers (X18, `docs/plans/forge-compaction-tiers-design.md`).
//!
//! The shipped rule rewrote the whole repository every 24 packs, with
//! the serving loop inside the upload: 33× the bytes pushed, one push
//! held 816 s, five copies of the repository in the bucket (the walgit
//! comparison's P9). This module replaces it with git's own geometric
//! split — `split_pack_geometry` from `builtin/repack.c`, weighted by
//! pack BYTES rather than object counts — over plain packs: push packs
//! and fold packs are tiers, the largest pack is the base and the only
//! one that carries a bitmap, and nothing new enters the bucket. The
//! snapshot's `packs` stays the whole reference set and the sweep's
//! predicate stays "named by the snapshot whose etag this sweep read".
//!
//! Three rules carry the design, each the answer to a refutation:
//!
//! 1. **The fold's bytes are never on a push's path.** `pack-objects`
//!    and the upload run on a task beside the loop, into a scratch
//!    directory git never scans; only the commit — renames, one CAS,
//!    two small PUTs — is on the loop.
//! 2. **The commit names `(snapshot.packs \ S) ∪ {F}`, never the
//!    directory.** A fresh listing can hold a pack no batch uploaded (a
//!    refused push's), and a snapshot naming an un-uploaded pack is a
//!    restore that refuses to start. F is renamed into `objects/pack`
//!    BEFORE the CAS, so a batch after the commit finds it `known` and
//!    uploads nothing of it; the superseded packs are subtracted from
//!    every listing (`Syncer::listed_packs`) so no batch re-names them.
//! 3. **The fold ticks its own counter.** The renewer renews a
//!    `Pushing` phase only while the hold's counter moves; a fold
//!    ticking that counter would keep a wedged batch's holder renewing
//!    for the whole of a base rebuild's upload.
//!
//! Three more from the wire (runca, design §12; the simulation in the
//! design's §13 is what chose them), all in the pure planner:
//!
//! 4. **The ladder starts at the floor, not at the push.** A tier fold
//!    below `fold_min_bytes` (256 MiB) waits, so 8 MiB pushes are
//!    rewritten log2(top ÷ 256 MiB) times rather than log2(top ÷ 8 MiB);
//!    the pack cap bounds what waits.
//! 5. **A pack that alone meets the base rule is the base rule's.** A
//!    tier pack at or above `base_tier_percent` of the base is never a
//!    fold input: the rebuild takes it once, where the ladder rewrote
//!    ten 1 GiB pushes as folds of 2.3, 4.3, 6.7, 2.0 and 3.1 GiB and
//!    then rebuilt the base over them anyway.
//! 6. **The cadence is the base's age by the store's clock**, read from
//!    the LIST at restore, never process memory (a restarted pod
//!    rebuilt a 12 GiB base at once); and a closed cadence yields to
//!    the pack cap, so the packs rule 5 holds back cannot pile up
//!    without bound. The disk check never yields.
//!
//! What a tier fold is: `pack-objects --stdin-packs` over S, every
//! object of the inputs reachable or not, deltas reused, the window off.
//! What the base rebuild is: `pack-objects --all --write-bitmap-index`,
//! by reachability — the only place unreachable objects are dropped —
//! after `reflog expire`, because the restore's proof walks reflogs
//! unless told not to, and a warm restart keeps them.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use flint_store::{ObjectStore, StoreError};

use super::{batch, lease, packio, snapshot, ForgeError, ForgeResult, Syncer};

/// The content of the base marker, a `.keep` beside the base pack.
/// git honours any `.keep` (the pack is never rolled by its own tools);
/// the content says whose it is, so `receive-pack`'s transient `.keep`
/// on a push mid-migration is never mistaken for the base.
pub const BASE_MARKER: &str = "flint-forge base\n";

/// One pack as the planner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackInfo {
    pub name: String,
    pub bytes: u64,
    pub is_base: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Roll `inputs` (≥ 2 tier packs) into one pack, no bitmap.
    Fold { inputs: Vec<String> },
    /// Rebuild the base from every named pack by reachability, with the
    /// bitmap; `inputs` is every named pack.
    Base { inputs: Vec<String> },
}

impl Plan {
    pub fn inputs(&self) -> &[String] {
        match self {
            Plan::Fold { inputs } | Plan::Base { inputs } => inputs,
        }
    }
    pub fn is_base(&self) -> bool {
        matches!(self, Plan::Base { .. })
    }
}

/// The knobs the planner reads; the config's, gathered so the planner
/// is a pure function of them.
#[derive(Debug, Clone, Copy)]
pub struct PlanKnobs {
    /// git's `--geometric=<factor>`; 0 = never fold.
    pub factor: u64,
    /// Rebuild the base when the tiers reach this percent of it.
    pub base_tier_percent: u64,
    /// With no base yet, rebuild one once the named packs reach this.
    pub base_min_bytes: u64,
    /// A fold smaller than this is skipped unless the pack cap forces it.
    pub fold_min_bytes: u64,
    /// Fold regardless when the tier count reaches this; a closed
    /// cadence yields to it too.
    pub fold_max_packs: usize,
    /// Whether the disk allows a base rebuild now (1.2× the named bytes
    /// free); the caller's check, the planner only reads the answer.
    /// Never overridden.
    pub base_allowed: bool,
    /// Whether the cadence allows one (`base_rebuild_min_secs` since
    /// the last, by the base's age in the store). The pack cap
    /// overrides it.
    pub cadence_open: bool,
}

/// git's `split_pack_geometry` over bytes, the base excluded, plus the
/// base rule, the floor, the exemption and the cap. Pure; the tests are
/// the design's property tests.
pub fn plan(packs: &[PackInfo], k: PlanKnobs) -> Option<Plan> {
    if k.factor == 0 || packs.is_empty() {
        return None;
    }
    let base_bytes: u64 = packs.iter().filter(|p| p.is_base).map(|p| p.bytes).sum();
    let has_base = packs.iter().any(|p| p.is_base);
    let mut tiers: Vec<&PackInfo> = packs.iter().filter(|p| !p.is_base).collect();
    tiers.sort_by(|a, b| a.bytes.cmp(&b.bytes).then_with(|| a.name.cmp(&b.name)));
    let tier_bytes: u64 = tiers.iter().map(|p| p.bytes).sum();
    let all_names = || {
        let mut v: Vec<String> = packs.iter().map(|p| p.name.clone()).collect();
        v.sort();
        v
    };
    let cap = k.fold_max_packs.max(2);

    // The base rule first: it is the only rewrite that applies
    // reachability and the only one that writes a bitmap. A closed
    // cadence yields to the pack cap — the packs the exemption below
    // holds back wait for this rebuild, and their count must not grow
    // without bound — but the disk never does.
    if k.base_allowed && (k.cadence_open || tiers.len() >= cap) {
        if !has_base && tier_bytes >= k.base_min_bytes && !tiers.is_empty() {
            return Some(Plan::Base { inputs: all_names() });
        }
        if has_base
            && !tiers.is_empty()
            && tier_bytes.saturating_mul(100) >= base_bytes.saturating_mul(k.base_tier_percent)
        {
            return Some(Plan::Base { inputs: all_names() });
        }
    }

    // A pack that alone meets the base rule is the base rule's, never
    // a fold's input: the rebuild takes it once, where the ladder
    // would rewrite it at every level up to the base. With no base yet
    // the reference is the floor the first base is built at.
    let reference = if has_base { base_bytes } else { k.base_min_bytes };
    let tiers: Vec<&PackInfo> = tiers
        .into_iter()
        .filter(|p| p.bytes.saturating_mul(100) < reference.saturating_mul(k.base_tier_percent))
        .collect();
    let n = tiers.len();
    if n < 2 {
        return None;
    }
    // git: count, from the largest down, the packs that already form a
    // geometric progression; the first break is the split.
    let w = |i: usize| tiers[i].bytes;
    let mut i = n - 1;
    while i > 0 {
        if w(i) < k.factor.saturating_mul(w(i - 1)) {
            break;
        }
        i -= 1;
    }
    let mut split = i;
    if split > 0 {
        split += 1;
    }
    // Then extend the split upward while the heavy half no longer
    // dominates the pack the light half would become.
    let mut total: u64 = (0..split).map(w).sum();
    let mut j = split;
    while j < n {
        if w(j) < k.factor.saturating_mul(total) {
            total += w(j);
            split += 1;
            j += 1;
        } else {
            break;
        }
    }
    let forced = n >= cap;
    if split < 2 {
        if forced {
            // A perfect progression that has grown too long: the
            // smallest half by count, never every tier — every tier is
            // a rewrite of everything but the base.
            let mut inputs: Vec<String> = tiers[..n.div_ceil(2)].iter().map(|p| p.name.clone()).collect();
            inputs.sort();
            return Some(Plan::Fold { inputs });
        }
        return None;
    }
    // The floor: the ladder starts here, not at the push size. What
    // waits under it is bounded by the cap.
    if total < k.fold_min_bytes && !forced {
        return None;
    }
    let mut inputs: Vec<String> = tiers[..split].iter().map(|p| p.name.clone()).collect();
    inputs.sort();
    Some(Plan::Fold { inputs })
}

// ── persistence beside the repository ────────────────────────────────

/// A superseded pack kept on disk for readers that opened it before
/// the commit; unlinked on the tick after `unlink_after_unix`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Retained {
    pub name: String,
    pub unlink_after_unix: u64,
}

/// What a commit unnamed in the bucket, for the ledger sweep: the exact
/// keys' file names, so deleting them costs no LIST.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LedgerEntry {
    pub files: Vec<String>,
    pub unnamed_unix: u64,
}

pub fn retained_path(sc: &Syncer) -> PathBuf {
    sc.cfg.state_dir.join("fold-retained.json")
}
pub fn ledger_path(sc: &Syncer) -> PathBuf {
    sc.cfg.state_dir.join("fold-ledger.json")
}
pub fn scratch_dir(sc: &Syncer) -> PathBuf {
    sc.cfg.state_dir.join("fold")
}

fn load_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => T::default(),
    }
}

fn save_json<T: serde::Serialize>(path: &Path, v: &T) -> ForgeResult<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(v).map_err(|e| ForgeError::State(e.to_string()))?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Read the retained set and the ledger a previous incarnation left,
/// and wipe the scratch: nothing in it was ever named.
pub fn load_state(sc: &mut Syncer) -> ForgeResult<()> {
    sc.retained = load_json::<Vec<Retained>>(&retained_path(sc));
    sc.fold_ledger = load_json::<Vec<LedgerEntry>>(&ledger_path(sc));
    let scratch = scratch_dir(sc);
    if scratch.exists() {
        std::fs::remove_dir_all(&scratch)?;
    }
    // A stale multi-pack index (a hand-run `repack --write-midx` in the
    // pod) naming a deleted pack fails the proof with every object
    // present; this design never writes one.
    let dir = sc.cfg.repo.join("objects/pack");
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("multi-pack-index") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(())
}

pub fn save_retained(sc: &Syncer) -> ForgeResult<()> {
    save_json(&retained_path(sc), &sc.retained)
}
pub fn save_ledger(sc: &Syncer) -> ForgeResult<()> {
    save_json(&ledger_path(sc), &sc.fold_ledger)
}

/// Whether a `.keep` beside `pack` is the base marker.
pub fn is_base_marker(repo: &Path, pack: &str) -> bool {
    let keep = repo.join("objects/pack").join(format!("{}.keep", pack.trim_end_matches(".pack")));
    std::fs::read_to_string(keep).map(|s| s == BASE_MARKER).unwrap_or(false)
}

/// The named pack that carries the base marker, if any.
pub fn base_of(sc: &Syncer) -> Option<String> {
    let cell = sc.cell.as_ref()?;
    cell.snap.packs.iter().find(|p| is_base_marker(&sc.cfg.repo, p)).cloned()
}

/// Mark `pack` as the base and unmark every other pack that carries
/// our marker. A `.keep` that is not ours (receive-pack's, mid-push) is
/// left alone.
pub fn set_base_marker(repo: &Path, pack: &str) -> ForgeResult<()> {
    let dir = repo.join("objects/pack");
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".keep")
                && std::fs::read_to_string(e.path()).map(|s| s == BASE_MARKER).unwrap_or(false)
                && name != format!("{}.keep", pack.trim_end_matches(".pack"))
            {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    std::fs::write(dir.join(format!("{}.keep", pack.trim_end_matches(".pack"))), BASE_MARKER)?;
    Ok(())
}

/// The planner's view of the named packs: local sizes, the base marker.
pub fn pack_infos(sc: &Syncer) -> ForgeResult<Vec<PackInfo>> {
    let cell = sc.cell()?;
    let mut out = Vec::new();
    for name in &cell.snap.packs {
        let path = sc.git.pack_path(name);
        let bytes = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            // A named pack that is not local is a bug the restore
            // refuses on; here it is simply not a fold input.
            Err(_) => continue,
        };
        out.push(PackInfo { name: name.clone(), bytes, is_base: is_base_marker(&sc.cfg.repo, name) });
    }
    Ok(out)
}

// ── the task ─────────────────────────────────────────────────────────

/// A fold in flight: what the loop knows about the task beside it.
pub struct InFlight {
    pub inputs: Vec<String>,
    pub is_base: bool,
    pub task: tokio::task::JoinHandle<()>,
    /// The fold's OWN counter — bytes uploaded — never the hold's.
    pub progress: Arc<AtomicU64>,
    pub started_unix: u64,
    /// For the stall detector: the counter's value and when it last moved.
    pub last_seen: u64,
    pub last_moved_unix: u64,
    pub stage: Arc<std::sync::Mutex<&'static str>>,
}

/// What the task hands the loop when its upload is complete.
#[derive(Debug)]
pub struct FoldResult {
    /// `pack-<sha>.pack`, in the scratch directory.
    pub pack: String,
    /// Every sibling written beside it, the pack included.
    pub siblings: Vec<String>,
    pub inputs: Vec<String>,
    pub is_base: bool,
    pub cell_etag: Option<String>,
    /// `None` on success; the task's error otherwise — the loop logs
    /// it and clears the fold.
    pub error: Option<String>,
}

/// The base rule's two preconditions the planner cannot see: the disk
/// (1.2× the named bytes free, §7.9) and the cadence. Returned apart,
/// because the pack cap may override the cadence and never the disk.
/// The cadence is the base's age: `last_base_rebuild_unix` is set by
/// the commit and, on a fresh incarnation, by the restore from the
/// LIST's `last_modified` of the base pack.
fn base_gate(sc: &Syncer, now: u64) -> (bool, bool) {
    let cadence_open = sc.last_base_rebuild_unix == 0
        || now.saturating_sub(sc.last_base_rebuild_unix) >= sc.cfg.base_rebuild_min_secs;
    let named: u64 = pack_infos(sc).map(|v| v.iter().map(|p| p.bytes).sum()).unwrap_or(0);
    let disk_ok = match free_bytes(&sc.cfg.repo) {
        Some(free) if free < named.saturating_mul(12) / 10 => {
            eprintln!(
                "flint-forge: base rebuild deferred: {free} bytes free under the repository, \
                 {named} named"
            );
            false
        }
        _ => true,
    };
    (disk_ok, cadence_open)
}

#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid C string and an out-parameter of the right type.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// What the planner would do now, from the syncer's state: the named
/// packs' sizes and marker, the config's knobs, the disk and the
/// cadence. `None` when nothing is due, the factor is 0, a fold is in
/// flight, no snapshot is loaded, or the process is fenced.
pub fn planned(sc: &Syncer, now: u64) -> ForgeResult<Option<Plan>> {
    if sc.cfg.fold_factor == 0 || sc.fold.is_some() || sc.cell.is_none() || sc.fenced().is_some() {
        return Ok(None);
    }
    let infos = pack_infos(sc)?;
    let (base_allowed, cadence_open) = base_gate(sc, now);
    let knobs = PlanKnobs {
        factor: sc.cfg.fold_factor,
        base_tier_percent: sc.cfg.base_tier_percent,
        base_min_bytes: sc.cfg.base_min_bytes,
        fold_min_bytes: sc.cfg.fold_min_bytes,
        fold_max_packs: sc.cfg.fold_max_packs,
        base_allowed,
        cadence_open,
    };
    Ok(plan(&infos, knobs))
}

/// Plan and spawn a fold if one is due and none is in flight. Returns
/// the plan spawned, for the log.
pub fn maybe_spawn(
    sc: &mut Syncer,
    done: tokio::sync::mpsc::Sender<FoldResult>,
    now: u64,
) -> ForgeResult<Option<Plan>> {
    let Some(plan) = planned(sc, now)? else { return Ok(None) };
    spawn(sc, plan.clone(), done, now)?;
    Ok(Some(plan))
}

/// Freeze the inputs on the loop and start the task.
pub fn spawn(
    sc: &mut Syncer,
    plan: Plan,
    done: tokio::sync::mpsc::Sender<FoldResult>,
    now: u64,
) -> ForgeResult<()> {
    sc.check_fence()?;
    let scratch = scratch_dir(sc);
    if scratch.exists() {
        std::fs::remove_dir_all(&scratch)?;
    }
    std::fs::create_dir_all(&scratch)?;
    let inputs = plan.inputs().to_vec();
    let is_base = plan.is_base();
    let cell_etag = sc.cell()?.etag.clone();
    let epoch = sc.lease()?.epoch;
    let progress = Arc::new(AtomicU64::new(0));
    let stage = Arc::new(std::sync::Mutex::new("packing"));
    let git = super::gitcmd::Git::new(&sc.cfg.repo);
    let store = sc.store.clone();
    let hold = sc.hold.clone();
    let cfg = sc.cfg.clone();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    let threads = threads.saturating_sub(1).max(1);
    let t_progress = progress.clone();
    let t_stage = stage.clone();
    let t_inputs = inputs.clone();
    let task = tokio::spawn(async move {
        let result = run_task(
            &git, store.as_ref(), hold.as_ref(), &cfg, &scratch, &t_inputs, is_base, epoch, threads,
            t_progress, t_stage,
        )
        .await;
        let msg = match result {
            Ok((pack, siblings)) => FoldResult {
                pack,
                siblings,
                inputs: t_inputs,
                is_base,
                cell_etag,
                error: None,
            },
            Err(e) => FoldResult {
                pack: String::new(),
                siblings: vec![],
                inputs: t_inputs,
                is_base,
                cell_etag,
                error: Some(e.to_string()),
            },
        };
        let _ = done.send(msg).await;
    });
    sc.fold = Some(InFlight {
        inputs,
        is_base,
        task,
        progress,
        started_unix: now,
        last_seen: 0,
        last_moved_unix: now,
        stage,
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_task(
    git: &super::gitcmd::Git,
    store: &dyn ObjectStore,
    hold: &super::Hold,
    cfg: &super::ForgeConfig,
    scratch: &Path,
    inputs: &[String],
    is_base: bool,
    epoch: u64,
    threads: usize,
    progress: Arc<AtomicU64>,
    stage: Arc<std::sync::Mutex<&'static str>>,
) -> ForgeResult<(String, Vec<String>)> {
    let out_base = scratch.join("pack");
    let pack = if is_base {
        // The reflog would keep what the rebuild drops, and the proof
        // walks reflogs on a warm restart (design §7.7).
        git.reflog_expire_all().await?;
        git.pack_base(&out_base, threads).await?
    } else {
        git.pack_fold(inputs, &out_base, threads).await?
    };
    let Some(pack) = pack else {
        return Err(ForgeError::State("the fold produced no pack".into()));
    };
    let siblings = super::gitcmd::siblings_in(scratch, &pack);
    if let Ok(mut s) = stage.lock() {
        *s = "uploading";
    }
    for file in &siblings {
        if let Some(why) = hold.fenced() {
            return Err(ForgeError::Fenced(why));
        }
        packio::upload_file(store, &cfg.pack_key(file), &scratch.join(file), epoch, Some(progress.clone()))
            .await?;
    }
    if let Ok(mut s) = stage.lock() {
        *s = "uploaded";
    }
    Ok((pack, siblings))
}

/// The stall detector: a fold whose counter has not moved for
/// `fold_stall_secs` is aborted and its scratch removed; the plan runs
/// again at the next tick. No wall-clock bound — a large repository's
/// base rebuild takes what it takes.
pub fn check_stall(sc: &mut Syncer, now: u64) {
    let Some(f) = sc.fold.as_mut() else { return };
    let seen = f.progress.load(Ordering::Relaxed);
    if seen != f.last_seen {
        f.last_seen = seen;
        f.last_moved_unix = now;
        return;
    }
    let uploading = f.stage.lock().map(|s| *s == "uploading").unwrap_or(false);
    if uploading && now.saturating_sub(f.last_moved_unix) >= sc.cfg.fold_stall_secs {
        eprintln!(
            "flint-forge: fold stalled ({} bytes, no progress for {} s); aborting it",
            seen,
            now.saturating_sub(f.last_moved_unix)
        );
        f.task.abort();
        sc.fold = None;
        let _ = std::fs::remove_dir_all(scratch_dir(sc));
    }
}

/// Abort a fold in flight (SIGTERM, or a fence): the task is killed and
/// the scratch removed; nothing was named.
pub fn abort(sc: &mut Syncer) {
    if let Some(f) = sc.fold.take() {
        f.task.abort();
    }
    let _ = std::fs::remove_dir_all(scratch_dir(sc));
}

// ── the commit, on the loop ──────────────────────────────────────────

/// Take the task's result into the repository and the snapshot. O(1)
/// S3: one CAS and the derived files. Returns the pack named, or
/// `None` when the fold produced a name the snapshot already had.
pub async fn commit(sc: &mut Syncer, res: FoldResult, now: u64) -> ForgeResult<Option<String>> {
    sc.fold = None;
    let scratch = scratch_dir(sc);
    if let Some(e) = res.error {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(ForgeError::State(format!("fold failed: {e}")));
    }
    sc.check_fence()?;
    // One renewal before the write, exactly as a batch takes at its
    // step 3 (`lease::renew`, design §4). Without it this is the ONE
    // CAS on the loop that never revalidates the lease against the
    // cell, and `ForgeSync.tla`'s fold run found what that costs: a
    // holder deposed while its restore ran, whose restore then read the
    // successor's rotated snapshot, plans and commits a fold whose
    // If-Match matches — a straggler's CAS landing after its successor
    // restored (`Inv_NoStragglerLandAfterRestore`, mutation
    // `FoldNoRenew`). The roll-up itself is benign — it holds the same
    // pushes — but the commit retains its inputs and the ledger sweep
    // then deletes from the bucket packs the true holder still names.
    // One conditional PUT per fold, never per push.
    lease::renew(sc).await?;
    let cell = sc.cell()?.clone();
    let f = res.pack.clone();
    let stem_of = |p: &str| p.trim_end_matches(".pack").to_string();

    // Step 2: a rebuild can reproduce a name the snapshot holds (a base
    // rebuild over an unchanged reachable set reproduces the base's
    // own). Then nothing is renamed or uploaded, and F is never among
    // the packs unnamed or unlinked.
    let reproduced = cell.snap.packs.iter().any(|p| p == &f);
    let superseded: Vec<String> = res.inputs.iter().filter(|p| **p != f).cloned().collect();
    if reproduced {
        let _ = std::fs::remove_dir_all(&scratch);
        if superseded.is_empty() {
            // The rebuild produced the pack that is already there and
            // superseded nothing: no rename, no upload, no CAS. But it
            // IS the base and it DID run, so the marker and the cadence
            // are still owed — without them the planner sees no base,
            // plans a rebuild at every opportunity, and the first one
            // whose reachable set spans two packs writes a whole extra
            // copy of the repository. Measured on runcb's rate leg: a
            // repository whose reachable set was exactly one 64 MiB
            // push pack rebuilt again ten seconds later, for 128 MiB.
            if res.is_base {
                sc.last_base_rebuild_unix = now;
                set_base_marker(&sc.cfg.repo, &f)?;
            }
            return Ok(None);
        }
    } else {
        // Step 3: rename into objects/pack, the index LAST — a pack
        // without its index is invisible to git and to `local_packs`.
        let dir = sc.cfg.repo.join("objects/pack");
        let order = |name: &str| -> u8 {
            if name.ends_with(".idx") {
                3
            } else if name.ends_with(".bitmap") {
                2
            } else if name.ends_with(".rev") {
                1
            } else {
                0
            }
        };
        let mut files = res.siblings.clone();
        files.sort_by_key(|n| order(n));
        if res.is_base {
            std::fs::write(dir.join(format!("{}.keep", stem_of(&f))), BASE_MARKER)?;
        }
        for file in &files {
            std::fs::rename(scratch.join(file), dir.join(file))?;
        }
        let _ = std::fs::remove_dir_all(&scratch);
    }

    // Step 4: ONE CAS, from the snapshot's list and never the directory.
    let mut next = cell.snap.clone();
    let drop: BTreeSet<&String> = superseded.iter().collect();
    next.packs.retain(|p| !drop.contains(p));
    if !next.packs.iter().any(|p| p == &f) {
        next.packs.push(f.clone());
    }
    next.packs.sort();
    batch::carry_pending(sc, &mut next);
    let epoch = sc.lease()?.epoch;
    let writer = sc.holder_id.clone();
    let new_cell = match snapshot::cas(sc.store.as_ref(), &sc.cfg, &cell, next.clone(), epoch, &writer).await {
        Ok(c) => c,
        Err(ForgeError::Store(StoreError::PreconditionFailed(e))) => {
            return Err(sc.fence(format!(
                "snapshot CAS refused during a fold's commit, another server holds this repository: {e}"
            )))
        }
        Err(e) => {
            // The write may have landed and its response been lost:
            // re-read once. Named ⇒ adopt; else fatal — a "deferred"
            // commit would leave F unnamed on disk for the next batch
            // to upload on a push's path.
            let fresh = snapshot::load(sc.store.as_ref(), &sc.cfg).await?;
            if fresh.snap.packs.iter().any(|p| p == &f) && fresh.snap.epoch == epoch {
                fresh
            } else {
                return Err(ForgeError::State(format!("fold commit CAS failed and did not land: {e}")));
            }
        }
    };
    super::log::record(sc, &cell.snap, &new_cell.snap).await;
    sc.cell = Some(new_cell);
    sc.hold.tick(1);
    if res.is_base {
        sc.last_base_rebuild_unix = now;
        set_base_marker(&sc.cfg.repo, &f)?;
    }

    // Step 5: the superseded packs stay on disk for readers, and are
    // subtracted from every listing until unlinked.
    for p in &superseded {
        sc.retained.push(Retained { name: p.clone(), unlink_after_unix: now + sc.cfg.fold_retain_secs });
    }
    save_retained(sc)?;

    // Step 7 (before 6, so a crash between leaves the ledger complete):
    // what this commit unnamed, by file, for the ledger sweep.
    let mut files = Vec::new();
    for p in &superseded {
        for file in sc.git.pack_siblings(p) {
            files.push(file);
        }
    }
    if !files.is_empty() {
        sc.fold_ledger.push(LedgerEntry { files, unnamed_unix: now });
        save_ledger(sc)?;
    }

    // Step 6: the derived files, from the snapshot's list.
    if let Err(e) = batch::publish_derived(sc).await {
        eprintln!("flint-forge: derived files not republished after a fold: {e}");
    }
    let kind = if res.is_base { "base rebuild" } else { "fold" };
    eprintln!(
        "flint-forge: {kind} committed: {} from {} pack(s), snapshot seq {}, {} named",
        f,
        res.inputs.len(),
        sc.cell()?.snap.seq,
        sc.cell()?.snap.packs.len()
    );
    Ok(if reproduced { None } else { Some(f) })
}

/// Unlink retained packs past their time, skipping any stem that
/// currently carries a `.keep` (a push mid-migration onto the same
/// name, or the base's marker) — those retry next tick.
pub fn unlink_retained(sc: &mut Syncer, now: u64) -> ForgeResult<usize> {
    let dir = sc.cfg.repo.join("objects/pack");
    let mut kept = Vec::new();
    let mut unlinked = 0usize;
    let retained = std::mem::take(&mut sc.retained);
    for r in retained {
        if now < r.unlink_after_unix {
            kept.push(r);
            continue;
        }
        let stem = r.name.trim_end_matches(".pack");
        if dir.join(format!("{stem}.keep")).exists() {
            kept.push(r);
            continue;
        }
        // The index first: from then on git and `local_packs` do not
        // see the pack, whatever else remains.
        for ext in [".idx", ".rev", ".bitmap", ".pack"] {
            let _ = std::fs::remove_file(dir.join(format!("{stem}{ext}")));
        }
        unlinked += 1;
    }
    sc.retained = kept;
    if unlinked > 0 {
        save_retained(sc)?;
    }
    Ok(unlinked)
}

/// Delete what commits unnamed, past the grace, by the store's clock,
/// capped at `budget` requests so the loop is held for about a second.
pub async fn sweep_ledger(sc: &mut Syncer, now: u64, budget: usize) -> ForgeResult<usize> {
    sc.check_fence()?;
    let grace = sc.cfg.orphan_grace_secs;
    if !sc.fold_ledger.iter().any(|e| now.saturating_sub(e.unnamed_unix) >= grace) {
        return Ok(0);
    }
    // The reference set, read once for the pass: a stem the snapshot
    // names — or an undo point names — is never deleted.
    let fresh = snapshot::load(sc.store.as_ref(), &sc.cfg).await?;
    if fresh.etag != sc.cell()?.etag {
        return Ok(0);
    }
    let mut named: BTreeSet<String> =
        fresh.snap.packs.iter().map(|p| p.trim_end_matches(".pack").to_string()).collect();
    // X15: an undo point's packs are referenced too. This sweep deletes
    // by exact key and never lists, so without the union it would take
    // the packs a force-push just made recoverable.
    named.extend(super::undo::referenced(sc.store.as_ref(), &sc.cfg, now).await?.stems);
    let mut requests = 0usize;
    let mut deleted = 0usize;
    let mut remaining = Vec::new();
    let ledger = std::mem::take(&mut sc.fold_ledger);
    for mut entry in ledger {
        if now.saturating_sub(entry.unnamed_unix) < grace || requests >= budget {
            remaining.push(entry);
            continue;
        }
        let mut left = Vec::new();
        for file in entry.files.drain(..) {
            let stem = file.split('.').next().unwrap_or("").to_string();
            if named.contains(&stem) {
                continue;
            }
            if requests >= budget {
                left.push(file);
                continue;
            }
            let key = sc.cfg.pack_key(&file);
            requests += 1;
            let meta = match sc.store.head(&key).await {
                Ok(m) => m,
                Err(StoreError::NotFound(_)) => continue,
                Err(e) => {
                    left.push(file);
                    eprintln!("flint-forge: ledger sweep: {e}");
                    continue;
                }
            };
            // Rule 2: the age at the delete, by the store's clock.
            let old_enough = meta.last_modified_unix.map(|t| now.saturating_sub(t) >= grace).unwrap_or(false);
            if !old_enough {
                left.push(file);
                continue;
            }
            requests += 1;
            sc.store.delete(&key).await?;
            deleted += 1;
        }
        if !left.is_empty() {
            entry.files = left;
            remaining.push(entry);
        }
    }
    sc.fold_ledger = remaining;
    save_ledger(sc)?;
    if deleted > 0 {
        eprintln!("flint-forge: ledger sweep deleted {deleted} object(s) past the {grace}s grace");
    }
    Ok(deleted)
}

/// Facts for `/status`.
pub struct FoldFacts {
    pub base: Option<String>,
    pub tier_packs: usize,
    pub retained: usize,
    pub stage: Option<&'static str>,
    pub bytes: u64,
    pub inputs: usize,
    pub is_base: bool,
}

pub fn facts(sc: &Syncer) -> FoldFacts {
    let base = base_of(sc);
    let named = sc.cell.as_ref().map(|c| c.snap.packs.len()).unwrap_or(0);
    let (stage, bytes, inputs, is_base) = match sc.fold.as_ref() {
        Some(f) => (
            Some(f.stage.lock().map(|s| *s).unwrap_or("?")),
            f.progress.load(Ordering::Relaxed),
            f.inputs.len(),
            f.is_base,
        ),
        None => (None, 0, 0, false),
    };
    FoldFacts {
        tier_packs: named.saturating_sub(usize::from(base.is_some())),
        base,
        retained: sc.retained.len(),
        stage,
        bytes,
        inputs,
        is_base,
    }
}

#[cfg(test)]
mod plan_tests {
    //! The planner is pure: these are the design's property tests, each
    //! with the case that would pass a wrong planner named.
    use super::*;

    fn packs(sizes: &[u64]) -> Vec<PackInfo> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, b)| PackInfo { name: format!("pack-{i:03}.pack"), bytes: *b, is_base: false })
            .collect()
    }
    fn knobs() -> PlanKnobs {
        PlanKnobs {
            factor: 2,
            base_tier_percent: 50,
            base_min_bytes: u64::MAX,
            fold_min_bytes: 0,
            fold_max_packs: 64,
            base_allowed: true,
            cadence_open: true,
        }
    }
    fn fold_inputs(p: Option<Plan>) -> Vec<String> {
        match p {
            Some(Plan::Fold { inputs }) => inputs,
            other => panic!("expected a fold, got {other:?}"),
        }
    }

    /// git's own example: four equal packs roll into one.
    #[test]
    fn equal_packs_fold_together() {
        let got = fold_inputs(plan(&packs(&[8, 8, 8, 8]), knobs()));
        assert_eq!(got.len(), 4);
    }

    /// A perfect progression folds nothing: the control for every test
    /// below — a planner that always folds passes none of them.
    #[test]
    fn a_geometric_progression_is_left_alone() {
        assert_eq!(plan(&packs(&[8, 16, 32, 64]), knobs()), None);
        assert_eq!(plan(&packs(&[8, 32]), knobs()), None);
    }

    /// The light half rolls up, and the roll-up absorbs the heavy half
    /// only while the heavy half no longer dominates it.
    #[test]
    fn the_split_extends_while_the_heavy_half_is_dominated() {
        // [8, 8, 32]: the two 8s fold (16 < 32 keeps the 32 out).
        assert_eq!(fold_inputs(plan(&packs(&[8, 8, 32]), knobs())).len(), 2);
        // [8, 8, 16, 32]: 8+8 = 16 absorbs the 16 (16 < 32) and then the
        // 32 (32 < 64): everything folds.
        assert_eq!(fold_inputs(plan(&packs(&[8, 8, 16, 32]), knobs())).len(), 4);
    }

    /// The base never enters a tier fold, whatever its size.
    #[test]
    fn the_base_is_never_a_fold_input() {
        let mut p = packs(&[8, 8, 8]);
        p.push(PackInfo { name: "pack-base.pack".into(), bytes: 100, is_base: true });
        let got = fold_inputs(plan(&p, PlanKnobs { cadence_open: false, ..knobs() }));
        assert!(!got.iter().any(|n| n == "pack-base.pack"));
        assert_eq!(got.len(), 3);
    }

    /// A tier pack that alone meets the base rule waits for the base
    /// rebuild and is never a fold input: with the cadence closed the
    /// two small packs fold and the big one is left where it is; with
    /// it open the base rule takes everything. The control is the
    /// rule's absence — the design as first built folded [100, 100,
    /// 600] into one 800 MiB pack, then rebuilt the base over it.
    #[test]
    fn a_pack_that_alone_meets_the_base_rule_waits_for_the_base() {
        let mut p = packs(&[100, 100, 600]);
        p.push(PackInfo { name: "pack-base.pack".into(), bytes: 1000, is_base: true });
        let got = fold_inputs(plan(&p, PlanKnobs { cadence_open: false, ..knobs() }));
        assert_eq!(got, vec!["pack-000.pack", "pack-001.pack"], "the 600 waits");
        assert!(matches!(plan(&p, knobs()), Some(Plan::Base { .. })), "the base rule takes it");
        // Two big packs and nothing else: nothing to fold, the rebuild waits.
        let mut q = packs(&[600, 700]);
        q.push(PackInfo { name: "pack-base.pack".into(), bytes: 1000, is_base: true });
        assert_eq!(plan(&q, PlanKnobs { cadence_open: false, ..knobs() }), None);
    }

    /// A closed cadence yields to the pack cap, so the packs the
    /// exemption holds back cannot pile up without bound; the disk
    /// check never yields.
    #[test]
    fn a_closed_cadence_yields_to_the_pack_cap_and_the_disk_does_not() {
        let mut p = packs(&[600, 600, 600, 600]);
        p.push(PackInfo { name: "pack-base.pack".into(), bytes: 1000, is_base: true });
        let closed = PlanKnobs { cadence_open: false, fold_max_packs: 4, ..knobs() };
        assert!(matches!(plan(&p, closed), Some(Plan::Base { .. })), "four exempt packs at the cap rebuild");
        let below = PlanKnobs { fold_max_packs: 5, ..closed };
        assert_eq!(plan(&p, below), None, "below the cap they wait");
        let no_disk = PlanKnobs { base_allowed: false, ..closed };
        assert_eq!(plan(&p, no_disk), None, "the disk is never overridden");
    }

    /// The floor: a fold below it waits, until the cap forces it.
    #[test]
    fn the_floor_holds_small_folds_until_the_cap() {
        let k = PlanKnobs { fold_min_bytes: 100, ..knobs() };
        assert_eq!(plan(&packs(&[8, 8, 8]), k), None, "24 is under the floor");
        assert_eq!(fold_inputs(plan(&packs(&[8, 8, 8]), PlanKnobs { fold_max_packs: 3, ..k })).len(), 3);
        // Thirteen 8s reach the floor and fold together.
        assert_eq!(fold_inputs(plan(&packs(&[8; 13]), k)).len(), 13);
    }

    /// The base rule: the tiers at half the base rebuild it; below that
    /// they fold among themselves; with the cadence closed they never do.
    #[test]
    fn the_base_is_rebuilt_at_the_tier_percent_and_only_when_allowed() {
        let mut p = packs(&[100, 100, 100]);
        p.push(PackInfo { name: "pack-base.pack".into(), bytes: 1000, is_base: true });
        assert!(matches!(plan(&p, knobs()), Some(Plan::Fold { .. })));
        p.push(PackInfo { name: "pack-big.pack".into(), bytes: 300, is_base: false });
        match plan(&p, knobs()) {
            Some(Plan::Base { inputs }) => assert_eq!(inputs.len(), 5, "every named pack"),
            other => panic!("expected a base rebuild, got {other:?}"),
        }
        assert!(matches!(plan(&p, PlanKnobs { base_allowed: false, ..knobs() }), Some(Plan::Fold { .. })));
    }

    /// With no base, the first rebuild waits for `base_min_bytes`.
    #[test]
    fn a_fresh_repository_gets_its_base_at_the_floor() {
        let k = PlanKnobs { base_min_bytes: 30, ..knobs() };
        assert!(plan(&packs(&[8, 16]), k).is_none());
        assert!(matches!(plan(&packs(&[8, 8, 16]), k), Some(Plan::Base { .. })));
    }

    /// The cap folds a perfect progression that has grown too long:
    /// the smallest half by count, never every tier (every tier would
    /// be a rewrite of everything but the base).
    #[test]
    fn the_pack_cap_forces_a_fold_of_the_smallest_half() {
        let sizes: Vec<u64> = (0..6).map(|i| 8u64 << i).collect();
        assert_eq!(plan(&packs(&sizes), knobs()), None);
        let got = fold_inputs(plan(&packs(&sizes), PlanKnobs { fold_max_packs: 6, ..knobs() }));
        assert_eq!(got, vec!["pack-000.pack", "pack-001.pack", "pack-002.pack"]);
    }

    /// Factor 0 is the control arm: never a fold, never a base.
    #[test]
    fn factor_zero_plans_nothing() {
        assert_eq!(plan(&packs(&[8, 8, 8, 8]), PlanKnobs { factor: 0, ..knobs() }), None);
    }

    /// The simulation's shape from the design (§3.6): uniform 8 MiB
    /// pushes fold at every second push, the tier count stays
    /// logarithmic, and a fold never increases the pack count.
    #[test]
    fn uniform_pushes_keep_a_logarithmic_tier_count() {
        let mib = 1u64 << 20;
        let mut tiers: Vec<u64> = Vec::new();
        let mut folds = 0;
        let mut largest = 0;
        for _ in 0..48 {
            tiers.push(8 * mib);
            let before = tiers.len();
            if let Some(Plan::Fold { inputs }) = plan(&packs(&tiers), knobs()) {
                // Re-derive the sizes: the fold's bytes are its inputs'.
                let mut named: Vec<(String, u64)> = packs(&tiers).into_iter().map(|p| (p.name, p.bytes)).collect();
                let folded: u64 = named.iter().filter(|(n, _)| inputs.contains(n)).map(|(_, b)| b).sum();
                named.retain(|(n, _)| !inputs.contains(n));
                tiers = named.into_iter().map(|(_, b)| b).collect();
                tiers.push(folded);
                folds += 1;
                largest = largest.max(folded);
                assert!(tiers.len() < before, "a fold never increases the pack count");
            }
            assert!(tiers.len() <= 8, "tier count {} after {} folds", tiers.len(), folds);
        }
        assert_eq!(folds, 24, "uniform pushes fold at every second push");
        // 32 pushes of 8 MiB rolled into one at push 32; the design's
        // 336 MiB for P9 includes the run's earlier tier packs.
        assert_eq!(largest, 256 * mib, "the largest fold is the 32-push roll-up");
    }
}
