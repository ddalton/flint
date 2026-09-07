//! Following the log, and proving only what is new (X14's cheap half,
//! `docs/plans/flint-forge-simplification-2026-09-05.md`).
//!
//! Two costs make forge's wake proportional to the repository rather
//! than to what changed, and the log (`log.rs`) is what lets both be
//! paid in advance:
//!
//! - **The bytes.** A challenger restores only after it claims, so on
//!   a 40 GiB repository the gap between "the holder died" and "a
//!   client is served" contains a full download. A challenger that
//!   holds a warm repository while it waits has already paid it.
//! - **The proof.** `fsck --connectivity-only` walks every object the
//!   refs reach, and it runs at every start-up — including the
//!   container restart whose `emptyDir` still holds the repository it
//!   proved ten seconds ago. A proof is a statement about a pack set
//!   and a set of tips; if the packs that carried the last proof are
//!   all still on disk, the only thing left to prove is the tips that
//!   moved since.
//!
//! The state that makes this safe is deliberately narrow. It records
//! WHAT WAS PROVED — the snapshot brought down, the pack files on disk
//! at the time, the tips walked — and it lives in the repository's own
//! `state_dir`, on the `emptyDir` beside the packs it describes, so it
//! cannot outlive them. A state that does not match the disk is not
//! repaired or reasoned about: it is discarded and the full proof runs,
//! which is what forge did before this module existed.
//!
//! The warm pass is NOT entitled to anything. It takes no lease, it
//! never writes to the bucket, it never sets the cell a CAS would use,
//! and it stops the moment the holder's token goes quiet — a challenger
//! about to take over must race, not download. Everything it leaves
//! behind is a cache the restore reconciles anyway.

use std::collections::{BTreeMap, BTreeSet};

use flint_store::StoreError;

use super::{log, packio, restore, snapshot::Snapshot, ForgeError, ForgeResult, Syncer};

/// Bumped when the shape changes; an older or newer file is discarded
/// rather than parsed, and the full proof runs.
pub const FOLLOW_VERSION: u32 = 1;

/// What this repository has been brought to, and what was proved about
/// it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FollowState {
    pub version: u32,
    /// The snapshot this repository holds, reconstructed by the log or
    /// read whole. Its etag is deliberately absent: a follower is not
    /// entitled to CAS, and carrying a token it may not use is how a
    /// cache becomes a second writer.
    pub snap: Snapshot,
    /// The pack files on disk when the proof was made.
    pub packs: Vec<String>,
    /// The tips walked by that proof.
    pub tips: Vec<String>,
    /// When the snapshot object itself was last read. The log poll is
    /// cheap but blind — an entry that was never written, or one the
    /// pruner took, leaves a follower believing it is caught up — so
    /// the snapshot is re-read on a timer regardless.
    pub confirmed_unix: u64,
}

fn path(sc: &Syncer) -> std::path::PathBuf {
    sc.cfg.state_dir.join("follow.json")
}

pub fn load(sc: &Syncer) -> Option<FollowState> {
    let bytes = std::fs::read(path(sc)).ok()?;
    let st: FollowState = serde_json::from_slice(&bytes).ok()?;
    if st.version != FOLLOW_VERSION {
        return None;
    }
    Some(st)
}

pub fn save(sc: &Syncer, st: &FollowState) -> ForgeResult<()> {
    std::fs::create_dir_all(&sc.cfg.state_dir)?;
    let body = serde_json::to_vec(st)
        .map_err(|e| ForgeError::State(format!("follow state will not serialise: {e}")))?;
    // Written beside and renamed: a torn state file read as valid would
    // claim a proof that never happened.
    let tmp = path(sc).with_extension("json.tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path(sc))?;
    Ok(())
}

pub fn forget(sc: &Syncer) {
    let _ = std::fs::remove_file(path(sc));
}

/// What a proof cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proof {
    /// `fsck --connectivity-only` over the whole repository.
    Full,
    /// `rev-list` over the tips that moved, against the tips the last
    /// proof walked. `new` is how many tips that was.
    Delta { new: usize },
    /// Nothing moved since the last proof and every pack that carried
    /// it is still on disk.
    Nothing,
}

impl Proof {
    pub fn is_full(&self) -> bool {
        matches!(self, Proof::Full)
    }
}

/// Prove that every object `want` names is present and connected,
/// skipping what a previous proof already walked.
///
/// The incremental arm holds only when EVERY pack the last proof was
/// made over is still on disk. A fold or a base rebuild replaces packs,
/// and the objects the old proof walked are then in files this process
/// never verified — so a fold costs one full proof, exactly as a cold
/// start does, and says so.
pub async fn prove(
    sc: &Syncer,
    want: &BTreeMap<String, String>,
    local_packs: &[String],
) -> ForgeResult<Proof> {
    let here: BTreeSet<&String> = local_packs.iter().collect();
    let prior = load(sc).filter(|st| st.packs.iter().all(|p| here.contains(p)));
    let Some(prior) = prior else {
        sc.git.fsck_connectivity().await?;
        return Ok(Proof::Full);
    };
    let known: BTreeSet<&String> = prior.tips.iter().collect();
    let new: Vec<String> =
        want.values().filter(|t| !known.contains(*t)).cloned().collect::<BTreeSet<_>>().into_iter().collect();
    if new.is_empty() {
        return Ok(Proof::Nothing);
    }
    sc.git.prove_reachable(&new, &prior.tips).await?;
    Ok(Proof::Delta { new: new.len() })
}

/// Record what this repository now holds and what has been proved about
/// it.
///
/// Called after a restore and on the serving loop's slow tick. The tips
/// a batch accepted are proved by `receive-pack`'s own connectivity
/// check and by the `update-ref` transaction that installed them, so a
/// checkpoint taken while serving is a statement this process is
/// entitled to make — and taking it on the tick rather than per push
/// keeps a 50 KB local write off the acknowledgement path.
pub fn checkpoint(sc: &Syncer, confirmed_unix: u64) -> ForgeResult<()> {
    let Ok(cell) = sc.cell() else { return Ok(()) };
    let packs = sc.git.local_packs()?;
    let tips: Vec<String> = cell.snap.refs.values().cloned().collect::<BTreeSet<_>>().into_iter().collect();
    save(
        sc,
        &FollowState {
            version: FOLLOW_VERSION,
            snap: cell.snap.clone(),
            packs,
            tips,
            confirmed_unix,
        },
    )
}

/// One warm pass.
#[derive(Debug, Clone, Default)]
pub struct WarmReport {
    /// How many log entries were applied; `None` when the snapshot was
    /// read whole instead.
    pub entries: Option<usize>,
    pub seq: u64,
    pub files_fetched: usize,
    pub bytes_fetched: u64,
    pub proof: Option<Proof>,
    pub unlinked: usize,
}

impl WarmReport {
    pub fn moved(&self) -> bool {
        self.files_fetched > 0 || self.proof.is_some() || self.unlinked > 0
    }
    pub fn line(&self) -> String {
        let via = match self.entries {
            Some(n) => format!("{n} log entrie(s)"),
            None => "the snapshot".into(),
        };
        format!(
            "seq {} via {via}: {} file(s), {:.1} MiB, proof {:?}",
            self.seq,
            self.files_fetched,
            self.bytes_fetched as f64 / (1024.0 * 1024.0),
            self.proof
        )
    }
}

/// How many entries one pass will chase before it gives up and reads
/// the snapshot instead. A follower this far behind is cheaper to
/// reconcile whole.
const MAX_CHASE: usize = 512;

/// Bring the local repository towards what the bucket holds, without a
/// lease and without touching the bucket's state.
///
/// Cheap when nothing has happened: one GET that 404s. Cheap when a
/// little has: the entries since, and the files they name. Falls back
/// to the whole snapshot on a gap, on a version it does not speak, and
/// on a timer, because a log poll cannot distinguish "nothing happened"
/// from "the entry was never written".
pub async fn warm(sc: &mut Syncer) -> ForgeResult<WarmReport> {
    let branch = sc.cfg.default_branch.clone();
    let hooks = sc.cfg.hooks_path.clone();
    sc.git.init_bare(&branch, hooks.as_deref()).await?;
    let now = super::now_unix();
    let state = load(sc);
    let due = state
        .as_ref()
        .map(|st| now.saturating_sub(st.confirmed_unix) >= sc.cfg.prewarm_resync_secs)
        .unwrap_or(true);

    // ── where are we going, and how did we learn it ──────────────────
    let mut report = WarmReport::default();
    let mut files_from_log: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let (target, confirmed) = match (&state, due) {
        (Some(st), false) => {
            let mut snap = st.snap.clone();
            let mut applied = 0usize;
            while applied < MAX_CHASE {
                match log::read(sc.store.as_ref(), &sc.cfg, snap.seq + 1).await? {
                    Some(entry) => {
                        for add in &entry.packs_added {
                            files_from_log.insert(add.pack.clone(), add.files.clone());
                        }
                        entry.apply(&mut snap);
                        applied += 1;
                    }
                    None => break,
                }
            }
            if applied == 0 {
                // Caught up as far as the log can say. Nothing read,
                // nothing fetched, nothing proved.
                report.seq = snap.seq;
                report.entries = Some(0);
                return Ok(report);
            }
            report.entries = Some(applied);
            (snap, st.confirmed_unix)
        }
        _ => {
            let cell = super::snapshot::load(sc.store.as_ref(), &sc.cfg).await?;
            if cell.etag.is_none() {
                // Nobody has published this repository; an empty bare
                // repo is already the whole of it.
                report.seq = 0;
                return Ok(report);
            }
            (cell.snap, now)
        }
    };
    report.seq = target.seq;

    // ── the files ────────────────────────────────────────────────────
    let pack_dir = sc.cfg.repo.join("objects/pack");
    std::fs::create_dir_all(&pack_dir)?;
    let have: BTreeSet<String> = sc.git.local_packs()?.into_iter().collect();
    let mut wanted: Vec<&String> = target.packs.iter().filter(|p| !have.contains(*p)).collect();
    // Largest first when the log told us the sizes; the fan-out's tail
    // is then not one stream.
    wanted.sort();
    let mut listed: Option<BTreeMap<String, restore::PackObject>> = None;
    for pack in wanted {
        let names = match files_from_log.get(pack) {
            Some(n) if !n.is_empty() => n.clone(),
            // The snapshot path, or an entry that named no files: one
            // LIST for the whole set, shared across the loop.
            _ => {
                if listed.is_none() {
                    listed = Some(restore::list_pack_files(sc).await?);
                }
                let stem = pack.trim_end_matches(".pack");
                listed
                    .as_ref()
                    .unwrap()
                    .keys()
                    .filter(|n| n.starts_with(stem))
                    .cloned()
                    .collect()
            }
        };
        for name in names {
            let dest = pack_dir.join(&name);
            if dest.exists() {
                continue;
            }
            let key = sc.cfg.pack_key(&name);
            match packio::fetch_to_file(sc.store.clone(), &key, &dest, sc.cfg.fanout).await {
                Ok(()) => {
                    report.files_fetched += 1;
                    report.bytes_fetched += std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                }
                // A pack the holder's sweep took while we were reading
                // it. The warm pass is a cache fill: it gives up on
                // this pass and the next one re-reads the snapshot.
                Err(ForgeError::Store(StoreError::NotFound(k))) => {
                    forget(sc);
                    return Err(ForgeError::State(format!(
                        "warm pass gave up: {k} is gone from the bucket; the next pass reads the \
                         snapshot"
                    )));
                }
                Err(e) => return Err(e),
            }
        }
    }

    // ── the packs the target does not name ───────────────────────────
    // The same reconcile the restore does, for the same reason: a
    // follower that kept every pack it ever saw would hold a growing
    // repository whose extra packs no snapshot names.
    {
        let named: BTreeSet<&String> = target.packs.iter().collect();
        for pack in sc.git.local_packs()? {
            if named.contains(&pack) {
                continue;
            }
            let stem = pack.trim_end_matches(".pack").to_string();
            for ext in [".idx", ".rev", ".bitmap", ".keep", ".pack"] {
                let _ = std::fs::remove_file(pack_dir.join(format!("{stem}{ext}")));
            }
            report.unlinked += 1;
        }
    }

    // ── the refs, then the proof ─────────────────────────────────────
    let local = sc.git.refs().await?;
    let mut script = String::new();
    for (name, oid) in &target.refs {
        if local.get(name).map(|l| l == oid).unwrap_or(false) {
            continue;
        }
        script.push_str(&format!("update {name} {oid}\n"));
    }
    for (name, oid) in &local {
        if !target.refs.contains_key(name) {
            script.push_str(&format!("delete {name} {oid}\n"));
        }
    }
    if !script.is_empty() {
        let out = sc.git.run(&["update-ref", "--stdin"], Some(script.as_bytes())).await?;
        if !out.ok() {
            forget(sc);
            return Err(ForgeError::State(format!(
                "warm pass could not install the refs it fetched: {}",
                out.stderr.trim()
            )));
        }
    }
    let local_packs = sc.git.local_packs()?;
    let proof = prove(sc, &target.refs, &local_packs).await?;
    report.proof = Some(proof);

    let tips: Vec<String> =
        target.refs.values().cloned().collect::<BTreeSet<_>>().into_iter().collect();
    save(
        sc,
        &FollowState {
            version: FOLLOW_VERSION,
            snap: target,
            packs: local_packs,
            tips,
            confirmed_unix: confirmed,
        },
    )?;
    Ok(report)
}
