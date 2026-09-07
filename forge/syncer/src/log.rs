//! The batch log: what each snapshot CAS changed, as its own small
//! immutable object (X15's second half, `docs/plans/flint-forge-simplification-2026-09-05.md`).
//!
//! The snapshot is the truth and it is whole: every batch rewrites the
//! full ref map and the full pack list, so a reader that wants to know
//! what MOVED has to read the whole thing and diff it against a copy it
//! kept. That is O(the repository's refs) per look, and a follower that
//! looks every heartbeat pays it whether or not anything happened —
//! which is why forge had no follower and why every wake was a full
//! restore (X14).
//!
//! The log is the delta beside the truth. After each CAS the writer
//! puts one object at `git/log/<seq>.json` naming the refs that moved,
//! the packs that appeared and the packs that left. Three rules make it
//! safe to be a hint rather than a second source of truth:
//!
//! 1. **The entry is written AFTER its CAS.** The opposite order would
//!    let a fenced writer leave an entry for a batch that never landed,
//!    and a follower applying it would install refs no server ever
//!    acknowledged. Written after, the only failure is a MISSING entry
//!    — a gap — and a gap is detectable.
//! 2. **A follower advances only along a contiguous chain.** It knows
//!    the seq it stands at; it reads `seq + 1`, then `seq + 2`. A hole
//!    (a crash between the CAS and the put, or an entry the pruner
//!    took) stops it, and it falls back to the snapshot — the same
//!    reconcile it would have done anyway.
//! 3. **Nothing in the log is a reference.** Entries do not keep packs
//!    alive; the sweep's reference set is the snapshot's packs and the
//!    undo points', exactly as before. An entry naming a pack the
//!    sweep has taken is a gap of the first kind: the follower's fetch
//!    404s and it falls back.
//!
//! What it buys, in the two places forge actually pays: an idle poll is
//! one 404 on `<seq+1>` instead of a whole snapshot, and a challenger
//! waiting out another server's lease can hold a warm repository for
//! the price of the pushes it missed (`follow.rs`).

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use flint_store::{crc64_nvme, GenerationStamps, ObjectStore, PutCondition, StoreError};

use super::{snapshot::Snapshot, ForgeConfig, ForgeError, ForgeResult, Syncer};

/// Bumped only for a change an older reader could MISREAD. A follower
/// that meets a higher version treats the entry as a gap and falls back
/// to the snapshot, which is always correct and never wrong — the log
/// has no authority to lose.
pub const LOG_VERSION: u32 = 1;

/// One file of a pack as the writer had it on disk: the `.pack` and
/// whichever of `.idx`, `.bitmap` and `.rev` existed beside it. A
/// follower fetches exactly these and needs no LIST to find them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PackFiles {
    pub pack: String,
    pub files: Vec<String>,
    /// The pack's own size, for a log line and for largest-first
    /// ordering. Not a precondition of anything: the fetch pins on the
    /// store's etag, never on this.
    #[serde(default)]
    pub bytes: u64,
}

/// What one CAS changed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    pub version: u32,
    /// The seq this entry PRODUCED.
    pub seq: u64,
    /// The seq it replaced. `seq - 1` for every entry a live writer
    /// makes; carried explicitly so a reader never has to assume it.
    pub from_seq: u64,
    pub epoch: u64,
    pub unix: u64,
    pub writer: String,
    /// Refs that moved: name -> new oid, with the empty string for a
    /// deletion. A ref the batch did not touch does not appear.
    pub refs: BTreeMap<String, String>,
    pub packs_added: Vec<PackFiles>,
    pub packs_removed: Vec<String>,
    /// The whole (small) lists, so applying an entry reproduces the
    /// snapshot exactly rather than approximately.
    #[serde(default)]
    pub bundles: Vec<String>,
    #[serde(default)]
    pub exported_commit: Option<String>,
}

impl LogEntry {
    /// Apply this entry to a snapshot standing at `from_seq`. The
    /// caller has already checked the chain.
    pub fn apply(&self, snap: &mut Snapshot) {
        for (name, oid) in &self.refs {
            if oid.is_empty() {
                snap.refs.remove(name);
            } else {
                snap.refs.insert(name.clone(), oid.clone());
            }
        }
        let gone: BTreeSet<&String> = self.packs_removed.iter().collect();
        snap.packs.retain(|p| !gone.contains(p));
        for add in &self.packs_added {
            if !snap.packs.contains(&add.pack) {
                snap.packs.push(add.pack.clone());
            }
        }
        snap.bundles = self.bundles.clone();
        snap.exported_commit = self.exported_commit.clone();
        snap.seq = self.seq;
        snap.epoch = self.epoch;
        snap.unix = self.unix;
        snap.writer = self.writer.clone();
    }
}

/// The entry that describes `prev -> next`. `files_of` names the files
/// beside a pack (the writer reads its own `objects/pack/`), so the
/// follower can fetch without a LIST.
pub fn diff(
    prev: &Snapshot,
    next: &Snapshot,
    files_of: impl Fn(&str) -> (Vec<String>, u64),
) -> LogEntry {
    let mut refs = BTreeMap::new();
    for (name, oid) in &next.refs {
        if prev.refs.get(name) != Some(oid) {
            refs.insert(name.clone(), oid.clone());
        }
    }
    for name in prev.refs.keys() {
        if !next.refs.contains_key(name) {
            refs.insert(name.clone(), String::new());
        }
    }
    let had: BTreeSet<&String> = prev.packs.iter().collect();
    let has: BTreeSet<&String> = next.packs.iter().collect();
    let packs_added = next
        .packs
        .iter()
        .filter(|p| !had.contains(*p))
        .map(|p| {
            let (files, bytes) = files_of(p);
            PackFiles { pack: p.clone(), files, bytes }
        })
        .collect();
    let packs_removed = prev.packs.iter().filter(|p| !has.contains(*p)).cloned().collect();
    LogEntry {
        version: LOG_VERSION,
        seq: next.seq,
        from_seq: prev.seq,
        epoch: next.epoch,
        unix: next.unix,
        writer: next.writer.clone(),
        refs,
        packs_added,
        packs_removed,
        bundles: next.bundles.clone(),
        exported_commit: next.exported_commit.clone(),
    }
}

/// Put one entry. Unconditional and idempotent: the seq is written once
/// by construction, because the CAS that produced it made the next one.
pub async fn put(store: &dyn ObjectStore, cfg: &ForgeConfig, entry: &LogEntry) -> ForgeResult<()> {
    let body = serde_json::to_vec(entry)
        .map_err(|e| ForgeError::State(format!("log entry will not serialise: {e}")))?;
    let crc = crc64_nvme(&body);
    let stamps = GenerationStamps {
        generation: entry.seq,
        epoch: entry.epoch,
        flush_uuid: uuid::Uuid::new_v4().to_string(),
        boundary_source: None,
        posix: None,
    };
    store
        .put_whole(&cfg.log_key(entry.seq), Bytes::from(body), &PutCondition::Unconditional, &stamps, crc)
        .await?;
    Ok(())
}

/// The entry a writer that just CAS'd should put, reading its own
/// `objects/pack/` for the file names beside each new pack. Pure: the
/// batch builds it before it hands the PUT to `emit`, which is what
/// lets the write ride BESIDE the derived files rather than after them
/// (`batch.rs` step 7) and keeps the log off the push's latency even
/// though it is on its request count.
pub fn entry_for(sc: &Syncer, prev: &Snapshot, next: &Snapshot) -> Option<LogEntry> {
    if sc.cfg.log_max_entries == 0 {
        return None;
    }
    Some(diff(prev, next, |pack| {
        let files = sc.git.pack_siblings(pack);
        let bytes = std::fs::metadata(sc.git.pack_path(pack)).map(|m| m.len()).unwrap_or(0);
        (files, bytes)
    }))
}

/// Put an entry and say so if it did not land.
///
/// Best effort by construction: the entry is a hint, and a missing one
/// costs a follower one fallback. Refusing the push because the hint
/// did not land would trade a cheap wake for an acknowledged push,
/// which is the wrong way round.
pub async fn emit(store: &dyn ObjectStore, cfg: &ForgeConfig, entry: Option<LogEntry>) {
    let Some(entry) = entry else { return };
    if let Err(e) = put(store, cfg, &entry).await {
        eprintln!(
            "flint-forge: log entry for seq {} NOT written ({e}); a follower will fall back to the \
             snapshot",
            entry.seq
        );
    }
}

/// Record `prev -> next`, for the paths where nothing waits on the
/// round trip (the fold's commit, the control rule's repack).
pub async fn record(sc: &Syncer, prev: &Snapshot, next: &Snapshot) {
    emit(sc.store.as_ref(), &sc.cfg, entry_for(sc, prev, next)).await;
}

/// The same, for the two call sites that hold no `Syncer`: the takeover
/// rotation writes an entry whose only content is the seq, so a
/// follower's chain does not break across a handover.
pub async fn record_rotation(
    store: &dyn ObjectStore,
    cfg: &ForgeConfig,
    prev: &Snapshot,
    next: &Snapshot,
) {
    if cfg.log_max_entries == 0 {
        return;
    }
    let entry = diff(prev, next, |_| (Vec::new(), 0));
    if let Err(e) = put(store, cfg, &entry).await {
        eprintln!("flint-forge: rotation log entry for seq {} NOT written ({e})", entry.seq);
    }
}

/// Read one entry. `Ok(None)` is "not there", which for a follower
/// means either "caught up" or "a gap" and is never an error.
pub async fn read(store: &dyn ObjectStore, cfg: &ForgeConfig, seq: u64) -> ForgeResult<Option<LogEntry>> {
    match store.get_whole(&cfg.log_key(seq), None).await {
        Ok((_, body)) => match serde_json::from_slice::<LogEntry>(&body) {
            Ok(e) if e.version <= LOG_VERSION && e.seq == seq => Ok(Some(e)),
            // A version this reader does not speak, or a body under the
            // wrong key, is treated exactly as a gap: fall back to the
            // snapshot, which no layout change can make unreadable
            // without the snapshot's own version gate firing first.
            Ok(e) => {
                eprintln!(
                    "flint-forge: log entry {} is version {} seq {} — treating it as a gap",
                    cfg.log_key(seq),
                    e.version,
                    e.seq
                );
                Ok(None)
            }
            Err(e) => {
                eprintln!("flint-forge: log entry {} unreadable ({e}); treating it as a gap", cfg.log_key(seq));
                Ok(None)
            }
        },
        Err(StoreError::NotFound(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Every entry seq in the bucket, ascending. One LIST of keys — used by
/// the pruner and the CLI, never by the follower's poll, which reads
/// `seq + 1` directly and pays one 404 when nothing has happened.
pub async fn seqs(store: &dyn ObjectStore, cfg: &ForgeConfig) -> ForgeResult<Vec<u64>> {
    let mut out: Vec<u64> = store
        .list(&cfg.log_prefix())
        .await?
        .iter()
        .filter_map(|o| seq_of(&o.key))
        .collect();
    out.sort_unstable();
    Ok(out)
}

pub fn seq_of(key: &str) -> Option<u64> {
    key.rsplit('/').next()?.strip_suffix(".json")?.parse().ok()
}

/// Keep the newest `cfg.log_max_entries` and delete the rest.
///
/// Count, not age: what a follower needs is "how many batches may I
/// fall behind before a wake costs a full restore", and that is a
/// number of batches. A quiet repository keeps its whole history; a
/// busy one keeps the last N, which is exactly the window in which
/// falling behind is cheap.
pub async fn prune(sc: &Syncer) -> ForgeResult<usize> {
    let keep = sc.cfg.log_max_entries;
    let all = seqs(sc.store.as_ref(), &sc.cfg).await?;
    if keep == 0 {
        // The log is off: entries a previous configuration wrote are
        // ordinary rubbish, and nothing follows them.
        let mut n = 0;
        for seq in all {
            sc.store.delete(&sc.cfg.log_key(seq)).await?;
            n += 1;
        }
        return Ok(n);
    }
    if all.len() <= keep {
        return Ok(0);
    }
    let cut = all.len() - keep;
    let mut n = 0;
    for seq in &all[..cut] {
        sc.store.delete(&sc.cfg.log_key(*seq)).await?;
        n += 1;
    }
    Ok(n)
}
