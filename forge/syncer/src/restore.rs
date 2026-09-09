//! Start-up: claim, rotate, restore, prove, serve (design §5).
//!
//! The local repository is a cache and the snapshot is the truth, so
//! start-up reconciles the cache TO the snapshot in both directions —
//! a ref the bucket does not name is deleted locally, not kept. The
//! asymmetry is deliberate: the only way a local ref can be ahead of
//! the snapshot is a bug, and preserving it would let a repository
//! serve history no other server could restore.
//!
//! Two rules from lean, for the same reasons lean has them. A pack the
//! snapshot names and the bucket lacks is re-read ONCE before being
//! believed — a repack under the previous holder can move the list
//! while this reader is fetching. And a repository that cannot be
//! proved (`fsck --connectivity-only`) is refused loudly rather than
//! served: half a repository serves clones that succeed and check out
//! nothing.

use std::collections::{BTreeMap, BTreeSet};

use flint_store::StoreError;

use super::{follow, gitcmd, packio, snapshot, ForgeError, ForgeResult, Syncer};

/// What a restore cost, so the difference between a cold wake and a
/// warm one is a number rather than an impression. Read by `/status`
/// and by the drills; the tests assert on it, which is what keeps the
/// incremental paths from silently becoming the full one again.
#[derive(Debug, Clone, Default)]
pub struct RestoreReport {
    pub seq: u64,
    pub packs_named: usize,
    pub files_fetched: usize,
    pub bytes_fetched: u64,
    pub unlinked: usize,
    pub proof: Option<follow::Proof>,
    pub elapsed_ms: u128,
}

impl RestoreReport {
    pub fn line(&self) -> String {
        format!(
            "seq {}, {} pack(s) named, {} file(s) fetched ({:.1} MiB), proof {:?}, {} ms",
            self.seq,
            self.packs_named,
            self.files_fetched,
            self.bytes_fetched as f64 / (1024.0 * 1024.0),
            self.proof,
            self.elapsed_ms
        )
    }
}

/// `merge-tree -X ours|theirs` is 2.43; below it the option means
/// something else, so the floor is asserted once at start rather than
/// discovered inside a merge.
pub const GIT_FLOOR: (u32, u32) = (2, 43);

pub async fn check_git_floor(sc: &Syncer) -> ForgeResult<()> {
    let (major, minor) = sc.git.version().await?;
    if (major, minor) < GIT_FLOOR {
        return Err(ForgeError::Refused(format!(
            "git {major}.{minor} is below forge's floor {}.{} (merge-tree -X)",
            GIT_FLOOR.0, GIT_FLOOR.1
        )));
    }
    Ok(())
}

/// Bring the local repository to exactly what the snapshot names.
pub async fn restore(sc: &mut Syncer) -> ForgeResult<RestoreReport> {
    sc.check_fence()?;
    let began = std::time::Instant::now();
    let mut report = RestoreReport::default();
    let branch = sc.cfg.default_branch.clone();
    let hooks = sc.cfg.hooks_path.clone();
    sc.git.init_bare(&branch, hooks.as_deref()).await?;
    // The fold's state from a previous incarnation: the retained set
    // is honoured, the ledger kept, the scratch wiped (nothing in it
    // was ever named) and a stray multi-pack index removed.
    super::fold::load_state(sc)?;

    let mut cell = match sc.cell.clone() {
        // A takeover rotation already loaded (and rewrote) it.
        Some(c) => c,
        None => snapshot::load(sc.store.as_ref(), &sc.cfg).await?,
    };
    if cell.etag.is_none() {
        // A repository nobody has published: an empty bare repo is
        // exactly right, and the first batch creates the snapshot under
        // `If-None-Match: *`.
        sc.cell = Some(cell);
        return Ok(report);
    }

    let mut listed = list_pack_files(sc).await?;
    // The preamble counts as movement too: at a real round trip the
    // snapshot, the listing and the sweep are a dozen requests before
    // the first chunk lands, and the renewer must not read that as a
    // wedge.
    sc.hold.tick(1);
    let mut revalidated = false;
    loop {
        let missing: Vec<String> = cell
            .snap
            .packs
            .iter()
            .filter(|p| !listed.contains_key(*p))
            .cloned()
            .collect();
        if missing.is_empty() {
            break;
        }
        if revalidated {
            return Err(ForgeError::Refused(format!(
                "snapshot {} names {} pack(s) the bucket does not hold ({}) — refusing to serve a \
                 repository that cannot be restored",
                sc.cfg.snapshot_key(),
                missing.len(),
                missing.join(", ")
            )));
        }
        // A repack under the previous holder can move the pack list
        // while this reader is fetching. Re-read once before believing
        // the absence (lean's revalidate rule).
        revalidated = true;
        cell = snapshot::load(sc.store.as_ref(), &sc.cfg).await?;
        listed = list_pack_files(sc).await?;
    }

    // Fetch every file that belongs to a named pack: the pack itself,
    // its index, and the bitmap and reverse index when they exist. The
    // bitmap is what makes the restored repository clone-ready without
    // a local `repack -b` (§8). All of it goes through one fan-out
    // bounded by `fanout`, across files and chunks alike: one file at a
    // time paid a round trip per sibling in series, and one chunk at a
    // time made a repacked repository's single pack a single stream.
    let pack_dir = sc.cfg.repo.join("objects/pack");
    std::fs::create_dir_all(&pack_dir)?;
    let mut units = Vec::new();
    for pack in &cell.snap.packs {
        let stem = pack.trim_end_matches(".pack");
        for (name, obj) in listed.iter() {
            if name.starts_with(stem) {
                let dest = pack_dir.join(name);
                if dest.exists() {
                    continue;
                }
                units.push(packio::FetchUnit {
                    key: obj.key.clone(),
                    dest,
                    size: obj.size,
                    etag: obj.etag.clone(),
                });
            }
        }
    }
    // Largest first, so the base's chunks start at once and the tail
    // of the fan-out is not one stream.
    units.sort_by_key(|u| std::cmp::Reverse(u.size));
    report.files_fetched = units.len();
    report.bytes_fetched = units.iter().map(|u| u.size).sum();
    packio::fetch_all(sc.store.clone(), units, sc.cfg.fanout, Some(sc.hold.progress_handle()))
        .await?;

    // Reconcile packs, the twin of the refs rule above: a local pack
    // the snapshot does not name is unlinked unless retention keeps it.
    // This is what makes every fold crash window benign — a fold pack
    // renamed in but never CAS'd, or inputs unnamed but not yet
    // retained — and it brings the code to what `ForgeSync.tla`'s
    // `Restore` already assumes. A pack without its index (a push
    // mid-migration) is left alone.
    {
        let named: BTreeSet<&String> = cell.snap.packs.iter().collect();
        let retained: BTreeSet<&String> = sc.retained.iter().map(|r| &r.name).collect();
        for pack in sc.git.local_packs()? {
            if named.contains(&pack) || retained.contains(&pack) {
                continue;
            }
            let stem = pack.trim_end_matches(".pack").to_string();
            for ext in [".idx", ".rev", ".bitmap", ".keep", ".pack"] {
                let _ = std::fs::remove_file(pack_dir.join(format!("{stem}{ext}")));
            }
            report.unlinked += 1;
            eprintln!("flint-forge: restore unlinked {pack}, which the snapshot does not name");
        }
    }

    // The base marker: the named pack whose bitmap the bucket carries;
    // the largest of them if a legacy `repack -b` pack coexists with a
    // new base (git picks one bitmap silently).
    {
        let mut with_bitmap: Vec<(u64, &String)> = cell
            .snap
            .packs
            .iter()
            .filter(|p| listed.contains_key(&format!("{}.bitmap", p.trim_end_matches(".pack"))))
            .map(|p| (listed.get(p.as_str()).map(|o| o.size).unwrap_or(0), p))
            .collect();
        with_bitmap.sort();
        if let Some((_, base)) = with_bitmap.last() {
            super::fold::set_base_marker(&sc.cfg.repo, base)?;
            // The rebuild cadence is the base's age by the store's
            // clock. A fresh incarnation has no memory of the last
            // rebuild, and without this the pod P5 restarted on runca
            // rebuilt a 12 GiB base the moment it restored.
            sc.last_base_rebuild_unix =
                listed.get(base.as_str()).and_then(|o| o.last_modified_unix).unwrap_or(0);
        }
    }

    // Refs: the snapshot's set, exactly. `update-ref --stdin` verifies
    // each object exists, so this is also the first proof that the
    // packs we just fetched contain what the refs name.
    let local = sc.git.refs().await?;
    let mut script = String::new();
    let want: BTreeMap<&String, &String> = cell.snap.refs.iter().collect();
    for (name, oid) in &want {
        if local.get(*name).map(|l| l == *oid).unwrap_or(false) {
            continue;
        }
        script.push_str(&format!("update {name} {oid}\n"));
    }
    let keep: BTreeSet<&String> = cell.snap.refs.keys().collect();
    for (name, oid) in &local {
        if !keep.contains(name) {
            script.push_str(&format!("delete {name} {oid}\n"));
        }
    }
    if !script.is_empty() {
        let out = sc.git.run(&["update-ref", "--stdin"], Some(script.as_bytes())).await?;
        if !out.ok() {
            return Err(ForgeError::Refused(format!(
                "restore could not install the snapshot's refs: {}",
                out.stderr.trim()
            )));
        }
        // `update-ref` writes a loose file per ref, so a restored
        // repository would otherwise start with every ref loose and
        // `receive-pack` would walk all of them on every push until the
        // first derived tick — which would itself then block the loop
        // for the whole cold pack. Measured: 5.3 s at 8,000 refs and
        // 12.0 s at 20,000 when everything is loose, against ~50 ms in
        // the steady state the tick actually sees. Design §3/§5 always
        // said the restore writes `packed-refs`; the code did not.
        if let Err(e) = sc.git.pack_refs().await {
            eprintln!("flint-forge: pack-refs after restore failed (refs stay loose): {e}");
        }
    }

    // HEAD, from the derived object if the bucket has one.
    match sc.store.get_whole(&sc.cfg.head_key(), None).await {
        Ok((_, body)) => {
            let text = String::from_utf8_lossy(&body);
            if let Some(target) = text.trim().strip_prefix("ref: ") {
                if target.starts_with("refs/") {
                    sc.git.symbolic_head(target).await?;
                }
            }
        }
        Err(StoreError::NotFound(_)) => {}
        Err(e) => return Err(e.into()),
    }

    // The proof, over the packs the snapshot names and no others. The
    // retained packs reconciled above are still on disk, deliberately,
    // and an fsck that walked them would prove the directory rather
    // than the bucket — passing for a repository that cannot be
    // restored once the ledger sweep runs (audit F2). A cold start pays
    // the full walk; when this process (or the warm pass before the
    // claim) already proved a pack set the snapshot still names, only
    // the tips that moved since are left to walk (`follow.rs`).
    let proof = follow::prove(sc, &cell.snap.refs, &cell.snap.packs).await?;
    report.proof = Some(proof);
    report.seq = cell.snap.seq;
    report.packs_named = cell.snap.packs.len();
    sc.cell = Some(cell);
    // The state that lets the NEXT restore be incremental. Written
    // after the proof, never before: a state file claiming a proof that
    // did not happen is the one way this becomes unsound.
    if let Err(e) = follow::checkpoint(sc, super::now_unix()) {
        eprintln!("flint-forge: could not record the restore's proof ({e}); the next start-up \
                   pays a full fsck");
    }
    report.elapsed_ms = began.elapsed().as_millis();
    Ok(report)
}

/// Every object under the repository's pack prefix, by file name. One
/// LIST serves both the restore's fetch plan and the sweep's candidate
/// set.
pub async fn list_pack_files(sc: &Syncer) -> ForgeResult<BTreeMap<String, PackObject>> {
    let prefix = sc.cfg.pack_prefix();
    let mut out = BTreeMap::new();
    for obj in sc.store.list(&prefix).await? {
        if let Some(name) = obj.key.rsplit('/').next() {
            out.insert(
                name.to_string(),
                PackObject {
                    key: obj.key.clone(),
                    size: obj.size,
                    etag: obj.etag.clone(),
                    last_modified_unix: obj.last_modified_unix,
                },
            );
        }
    }
    Ok(out)
}

/// What `list` already told us about a pack file. The size and etag
/// were previously discarded and then not available to the fetch, which
/// is why it read whole objects; carrying them costs nothing (they ride
/// the same LIST) and is what lets the restore fetch ranges pinned to
/// one generation without a HEAD per file.
#[derive(Debug, Clone)]
pub struct PackObject {
    pub key: String,
    pub size: u64,
    pub etag: String,
    /// The listing's age, for the sweep's prefilter; `None` when the
    /// store did not say.
    pub last_modified_unix: Option<u64>,
}

/// A repository with no refs and no snapshot: set the default branch so
/// a first clone is not a detached mystery.
pub async fn set_default_branch(sc: &Syncer, branch: &str) -> ForgeResult<()> {
    let target = if branch.starts_with("refs/") {
        branch.to_string()
    } else {
        format!("refs/heads/{branch}")
    };
    if sc.git.refs().await?.is_empty() {
        sc.git.symbolic_head(&target).await?;
    }
    Ok(())
}

/// Re-exported for the serving loop's convenience.
pub use gitcmd::RefUpdate;

/// PROOF THAT THE CALLER IS IN THE `Importing -> Serving` WINDOW.
///
/// `reclaim_at_rest` unlinks packs, and the model is unambiguous that
/// WHERE it runs is what makes it safe: `ForgeSyncReclaimUnlinks` HOLDS
/// at 86,039,237 distinct states, and its control
/// `ForgeSyncReclaimUnlinksServing` — the same run with
/// `ReclaimWhileServing = TRUE`, one constant moved — violates
/// `Inv_AckedIsDurable` in 1min 12s, because outside the window
/// unlinking destroys a pack an ACKED push still needed.
///
/// A comment saying "only call this before serving" is exactly the kind
/// of instruction a later refactor steps over. This type makes the
/// placement an argument the caller has to produce: `before_serving()`
/// is the only way to get one, and it is named so that constructing it
/// anywhere else reads as the mistake it would be.
pub struct AtRest(());

impl AtRest {
    /// Minted ONLY between `restore` and `Phase::Serving`, where the
    /// lease is already held and no push is being served.
    pub(crate) fn before_serving() -> Self {
        AtRest(())
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ReclaimReport {
    pub dropped: usize,
    pub bytes: u64,
    /// Named packs actually EXAMINED. This exists because the empty
    /// report conflated two different states: `dropped == 0` after a
    /// complete walk that found nothing to collect, and `dropped == 0`
    /// because the function declined to reason at all. Every one of the
    /// early returns below yields a legal-looking zero, so neither an
    /// operator's log line nor a test could tell them apart.
    pub considered: usize,
    /// What the walk cost. It runs before `Phase::Serving`, so this is
    /// wake latency, and it is dominated by ONE `show-index` fork per
    /// named pack (~12.9 ms each on darwin/arm64,
    /// `forge/e2e/results/d4-wake-cost-20260909-local.log`).
    pub elapsed_ms: u64,
    /// Why it did not reason, when it did not. `None` means it ran to a
    /// verdict — which may still be "nothing to collect".
    pub declined: Option<&'static str>,
}

/// DIRECTION 4 — the COLLECTOR, and the half direction 5 cannot do.
///
/// Direction 5 stops NAMING the packs of pushes forge refused, which
/// lowers the slope; it cannot remove what is already named, and what
/// it keeps (a MIXED push shares one pack with accepted objects) grows
/// linearly with nothing to collect it. This is the collector: a named
/// pack whose every REACHABLE object another KEPT pack also holds is
/// dead weight, and dropping it costs a reachability read and one CAS.
/// It builds no pack and uploads nothing.
///
/// THE CAS COMES BEFORE THE UNLINK. The other order can leave the
/// snapshot naming a pack that is gone from disk. This order can only
/// leave a pack on disk the snapshot does not name — which the reconcile
/// step above already unlinks on the next restore, so a crash between
/// the two heals itself.
pub async fn reclaim_at_rest(sc: &mut Syncer, _window: AtRest) -> ForgeResult<ReclaimReport> {
    let t0 = std::time::Instant::now();
    let mut report = reclaim_inner(sc).await?;
    report.elapsed_ms = t0.elapsed().as_millis() as u64;
    Ok(report)
}

async fn reclaim_inner(sc: &mut Syncer) -> ForgeResult<ReclaimReport> {
    let mut report = ReclaimReport::default();
    if !sc.cfg.reclaim_at_rest {
        report.declined = Some("the rule is off");
        return Ok(report);
    }
    let cell = sc.cell()?.clone();
    let named: Vec<String> = cell.snap.packs.clone();
    report.considered = named.len();
    if named.len() < 2 {
        // One pack cannot be covered by another, and zero is nothing.
        report.declined = Some("fewer than two named packs");
        return Ok(report);
    }
    let tips: Vec<String> = cell.snap.refs.values().cloned().collect();
    let reach: std::collections::HashSet<String> =
        sc.git.reachable_from(&tips).await?.into_iter().collect();

    // What each pack holds THAT IS STILL REACHABLE. Unreachable objects
    // are exactly the residue and must not keep a pack alive — that is
    // the whole finding. Read from the `.idx`, so this costs the index
    // and never the pack.
    let pack_dir = sc.cfg.repo.join("objects/pack");
    let mut live: std::collections::BTreeMap<String, std::collections::HashSet<String>> =
        Default::default();
    let mut size: std::collections::BTreeMap<String, u64> = Default::default();
    for p in &named {
        let stem = p.trim_end_matches(".pack");
        let idx = pack_dir.join(format!("{stem}.idx"));
        if !idx.exists() {
            // A pack whose index is absent is one this process cannot
            // reason about. Treat it as holding everything — i.e. never
            // drop it, and never let it license dropping another.
            report.declined = Some("a named pack has no .idx");
            return Ok(report);
        }
        let ids = sc.git.pack_object_ids(&idx).await?;
        live.insert(p.clone(), ids.into_iter().filter(|o| reach.contains(o)).collect());
        let bytes = std::fs::metadata(pack_dir.join(p)).map(|m| m.len());
        match bytes {
            Ok(b) => {
                size.insert(p.clone(), b);
            }
            // A named pack that is not on disk is a state this function
            // must not guess about: it returns rather than treating a
            // missing file as a zero-byte one.
            Err(_) => {
                report.declined = Some("a named pack is not on disk");
                return Ok(report);
            }
        }
    }

    // GREEDY, and it must re-test against what is still KEPT: two packs
    // that cover each other are both individually droppable but not
    // both together, and testing against the original set would drop
    // the pair and strand every object they shared.
    //
    // THE SCAN ORDER DECIDES HOW MUCH IS COLLECTED, and the first cut
    // took whatever order the pack hashes happened to sort in. A
    // roll-up covers the small packs it was built from, and those small
    // packs TOGETHER cover the roll-up — so every one of them is
    // individually droppable, and whichever is reached first wins.
    // Dropping the roll-up is legal, frees one pack, and then blocks
    // the two it would have licensed: 3 of 12 runs of
    // `direction_4_collects_a_wholly_covered_pack_and_unlinks_it`
    // collected 1 instead of 2, on a coin flip over pack names.
    //
    // Ascending live-object count drops the SUBSUMED packs first and
    // keeps the licensor, which leaves fewer packs — and fewer packs is
    // exactly what the wake path pays for, one `show-index` fork each
    // (~12.9 ms, `d4-wake-cost-20260909-local.log`). The tiebreak on
    // name makes it deterministic, so a repository reclaims the same
    // way on every wake instead of differently each time — which is
    // also what makes "the third restart changed nothing" mean
    // convergence rather than luck.
    let mut order: Vec<String> = named.clone();
    order.sort_by(|a, b| live[a].len().cmp(&live[b].len()).then_with(|| a.cmp(b)));
    let mut kept: Vec<String> = named.clone();
    let mut drop: Vec<String> = Vec::new();
    loop {
        let victim = order
            .iter()
            .filter(|p| kept.contains(p))
            .find(|p| live[*p].iter().all(|o| kept.iter().any(|q| q != *p && live[q].contains(o))))
            .cloned();
        match victim {
            Some(v) => {
                kept.retain(|x| x != &v);
                report.bytes += size[&v];
                drop.push(v);
            }
            None => break,
        }
    }
    if drop.is_empty() {
        return Ok(report);
    }

    // THE BELT OVER THE GREEDY'S BRACES. The loop's invariant already
    // says every reachable object an evicted pack held is in a kept
    // one; this asserts it over the FINAL set rather than trusting the
    // induction, because the cost of being wrong is an unrecoverable
    // repository and the cost of the check is a set walk.
    let covered: std::collections::HashSet<&String> =
        kept.iter().flat_map(|p| live[p].iter()).collect();
    for p in &drop {
        for o in &live[p] {
            if !covered.contains(o) {
                return Err(ForgeError::State(format!(
                    "reclaim refused: dropping {p} would strand reachable object {o}"
                )));
            }
        }
    }

    let epoch = sc.lease()?.epoch;
    let writer = sc.holder_id.clone();
    let mut next = cell.snap.clone();
    next.packs.retain(|p| !drop.contains(p));
    let new_cell =
        snapshot::cas(sc.store.as_ref(), &sc.cfg, &cell, next, epoch, &writer).await?;
    sc.cell = Some(new_cell);

    // Only now, and NOT into `retained`: a retained pack is excluded
    // from the listing forever, so a retry reusing its name (pack names
    // are many-to-one) would land with its objects unnamed. That is the
    // refuted form of this direction.
    for p in &drop {
        let stem = p.trim_end_matches(".pack").to_string();
        for ext in [".idx", ".rev", ".bitmap", ".keep", ".pack"] {
            let _ = std::fs::remove_file(pack_dir.join(format!("{stem}{ext}")));
        }
        report.dropped += 1;
        eprintln!("flint-forge: reclaim unlinked {p}, wholly covered by the packs kept");
    }
    Ok(report)
}
