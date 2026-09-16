//! FdCache — the open-fd cache behind OPEN/READ/WRITE/COMMIT and the
//! F17b/c stale-resolve fallbacks (an open holds the file across
//! rename-over).
//!
//! ## Why this module exists (F24)
//!
//! The cache used to be a bare `DashMap<[u8;12], CachedFile>` whose
//! consumers scanned it by path with `.iter().find(...)`. One call
//! site ran the scan as an `if let` scrutinee: scrutinee temporaries
//! live to the end of the block, so the DashMap `Iter` — holding the
//! matched shard's READ guard — was still alive during the same-map
//! `insert` inside the block. When the inserted key hashed to that
//! shard, the write acquisition queued behind the thread's own read
//! guard forever: one shard permanently locked, every worker
//! eventually parked on it, epoll unattended, server frozen with the
//! TCP connection still ESTABLISHED (so the client never reconnects —
//! it just waits).
//!
//! The structural answer, not just the one-line fix:
//!
//! 1. **No guard escapes this module.** Every public method returns
//!    owned clones (`Arc<File>` is the currency); callers cannot hold
//!    a shard guard because they never see one.
//! 2. **No iteration.** A secondary path index (`by_path`) makes every
//!    lookup a point lookup — the O(n)-scan-per-OPEN/COMMIT cliff
//!    under postgres's many-backends-open-one-file pattern is gone
//!    with the guard that the scan held.
//! 3. **Guard discipline is linted.** `no_iter_guards_in_scrutinees`
//!    below greps the NFS/pNFS trees for the exact F24 shape
//!    (`if let`/`while let` whose scrutinee iterates a map), the same
//!    mechanism that retired the ad-hoc-naming bug class via
//!    identity.rs. `clippy.toml` additionally denies holding dashmap
//!    guards across `.await` (the adjacent freeze, caught at compile
//!    time).
//!
//! ## Two-map consistency
//!
//! `by_stateid` is authoritative; `by_path` is an index. The maps are
//! not updated atomically, so `find_by_path` re-checks the resolved
//! entry's path before returning it — a transiently stale index entry
//! can only cause a cache miss (caller opens fresh), never a wrong fd.
//! The only unreclaimed residue a race can leave is a 12-byte stateid
//! id in a path's candidate vec (requires two concurrent inserts of
//! the SAME stateid under DIFFERENT paths — not a shape any NFS op
//! sequence produces).

use dashmap::DashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One cached open fd, keyed by the open stateid's `other` field.
#[derive(Clone)]
pub(crate) struct CachedFile {
    pub(crate) file: Arc<File>,
    pub(crate) path: PathBuf,
    /// Whether the fd was opened with write access. READ populates
    /// the cache too and falls back to a read-only open when the
    /// file mode denies write; WRITE only reuses writable entries.
    pub(crate) writable: bool,
    /// Inode captured at insert (fstat on the open fd). Identity key
    /// for v4 kernel handles, whose F17b/c stale-resolve fallbacks
    /// can't extract a path (no path is embedded) — they look the
    /// open file up by ino instead. 0 = unknown (never matches).
    pub(crate) ino: u64,
}

impl CachedFile {
    /// Capture the fd's inode for the ino index. On the open fd, so
    /// it names the OPEN-time object even across later renames.
    pub(crate) fn ino_of(file: &File) -> u64 {
        use std::os::unix::fs::MetadataExt;
        file.metadata().map(|m| m.ino()).unwrap_or(0)
    }
}

pub(crate) struct FdCache {
    /// stateid.other → open fd. Keyed on the stable 12-byte `other`
    /// field (not seqid) so entries survive seqid bumps from
    /// share-mask upgrades.
    by_stateid: DashMap<[u8; 12], CachedFile>,
    /// path → stateids holding an fd for it. Point-lookup index for
    /// the by-path consumers (OPEN fd seeding, COMMIT, stale-resolve
    /// fallbacks); never authoritative — see module docs.
    by_path: DashMap<PathBuf, Vec<[u8; 12]>>,
    /// ino → stateids holding an fd for it. Same index discipline as
    /// `by_path`; consumers are the v4-handle stale-resolve fallbacks.
    by_ino: DashMap<u64, Vec<[u8; 12]>>,
    /// stateid → last-use tick, for reaping least-recently-used once the
    /// cache is over its high-water mark. Written on insert AND on every
    /// `get` hit, which is what makes it recency rather than insertion
    /// order. Kept beside the entry rather than inside `CachedFile` so
    /// the entry type (cloned on every `get`) does not grow.
    order: DashMap<[u8; 12], u64>,
    seq: std::sync::atomic::AtomicU64,
    budget: FdBudget,
}

/// How large this cache may grow, derived at startup.
///
/// WHY A BOUND AT ALL. Until 2026-09-16 there was none — the module's
/// own sibling said so (`state/delegation.rs`: "`FdCache` has neither a
/// capacity bound nor an LRU, so the server leaked one open fd per
/// delegated file for the life of the process"). A drill then measured
/// ~2 descriptors leaked per sqlite transaction, which exhausted
/// `RLIMIT_NOFILE` and took the hub down: every operation returned
/// EMFILE, and the F33 watchdog read its own failing probe as a dead
/// backing store and exited 59 on a perfectly healthy ext4 export.
///
/// A bound turns "leak until death" into "evict the oldest", which is
/// what both reference implementations do — knfsd reaps its filecache
/// with an LRU shrinker, NFS-Ganesha with an LRU reaper thread at
/// `FD_LWMark_Percent` / `FD_HWMark_Percent`, denying requests only at
/// `FD_Limit_Percent`. Neither self-terminates.
///
/// It is safe to close a cached fd here: flint keeps byte-range locks in
/// its own in-memory table (`lockops.rs:23`), NOT as kernel fcntl locks
/// on the backing file, so dropping a descriptor releases no lock. An
/// in-flight op holding its own `Arc<File>` clone finishes normally, and
/// a miss just re-opens — this is a cache, not state.
///
/// DIVERGENCE FROM GANESHA, deliberate: its marks are percentages of the
/// rlimit because it assumes an operator-set `nofile`. We now raise the
/// soft limit to the hard limit at startup (1,048,576 on the drill
/// host), and 90% of that is not a sane number of open files to hold. So
/// the cap is the MINIMUM of a percentage and an absolute ceiling.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FdBudget {
    /// Reap once the cache exceeds this.
    pub(crate) hiwat: usize,
    /// Reap down to this.
    pub(crate) lowat: usize,
}

/// Absolute ceiling regardless of how generous the rlimit is.
/// `FLINT_FD_CACHE_MAX` overrides it.
const FD_CACHE_ABS_MAX: usize = 16384;

impl FdBudget {
    fn from_rlimit() -> Self {
        let rlim = current_nofile().unwrap_or(1024);
        let abs = std::env::var("FLINT_FD_CACHE_MAX")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(FD_CACHE_ABS_MAX);
        // Half the descriptor budget at most: sockets, the state db and
        // the log need the rest, and a cache that can consume the whole
        // table is the defect this exists to prevent.
        let hiwat = std::cmp::min(rlim / 2, abs).max(64);
        Self { hiwat, lowat: (hiwat * 3) / 4 }
    }
}

#[cfg(unix)]
fn current_nofile() -> Option<usize> {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0 {
        Some(lim.rlim_cur as usize)
    } else {
        None
    }
}

#[cfg(not(unix))]
fn current_nofile() -> Option<usize> {
    None
}

impl FdCache {
    pub(crate) fn new() -> Self {
        let budget = FdBudget::from_rlimit();
        tracing::info!(
            hiwat = budget.hiwat,
            lowat = budget.lowat,
            "fd cache bounded (reaps least-recently-used above hiwat; see FdBudget)"
        );
        Self {
            by_stateid: DashMap::new(),
            by_path: DashMap::new(),
            by_ino: DashMap::new(),
            order: DashMap::new(),
            seq: std::sync::atomic::AtomicU64::new(0),
            budget,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn budget(&self) -> FdBudget {
        self.budget
    }

    /// Reap least-recently-used down to `lowat`. Called from `insert` once the
    /// cache goes over `hiwat`, so the bound holds no matter WHICH path
    /// inserted — the point is that no future leak can reach the
    /// descriptor table, not that every leak has been found.
    fn reap_to_lowat(&self) -> usize {
        let len = self.by_stateid.len();
        if len <= self.budget.hiwat {
            return 0;
        }
        let target = len.saturating_sub(self.budget.lowat);
        let mut victims: Vec<(u64, [u8; 12])> =
            self.order.iter().map(|e| (*e.value(), *e.key())).collect();
        victims.sort_unstable_by_key(|(s, _)| *s);
        let mut reaped = 0;
        for (_, id) in victims.into_iter().take(target) {
            if self.remove(&id).is_some() {
                reaped += 1;
            }
        }
        if reaped > 0 {
            tracing::warn!(
                reaped,
                len_before = len,
                len_now = self.by_stateid.len(),
                hiwat = self.budget.hiwat,
                composition = %self.key_histogram(),
                "fd cache over its high-water mark — reaped least-recently-used entries \
                 (a cache miss re-opens; no lock is released, locks are server-side state)"
            );
        }
        reaped
    }

    pub(crate) fn contains(&self, other: &[u8; 12]) -> bool {
        self.by_stateid.contains_key(other)
    }

    /// Owned clone of the entry for this stateid, if any.
    pub(crate) fn get(&self, other: &[u8; 12]) -> Option<CachedFile> {
        let hit = self.by_stateid.get(other).map(|e| e.clone());
        if hit.is_some() {
            // TOUCH: this makes the reap least-RECENTLY-used rather than
            // oldest-inserted. FIFO would happily evict the hottest
            // descriptor in the cache purely because it was inserted
            // first, forcing a re-open of the file most in use — which
            // is the opposite of what a cache is for. Both reference
            // implementations order by recency (knfsd's LRU shrinker,
            // NFS-Ganesha's LRU reaper), and the cost here is one atomic
            // fetch_add plus a map write on a path that already took a
            // map read.
            self.order
                .insert(*other, self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        }
        hit
    }

    /// Insert (or replace) the fd for a stateid, keeping the path and
    /// ino indexes in step.
    pub(crate) fn insert(&self, other: [u8; 12], entry: CachedFile) {
        let path = entry.path.clone();
        let ino = entry.ino;
        self.order
            .insert(other, self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let prev = self.by_stateid.insert(other, entry);
        if let Some(prev) = prev {
            if prev.path != path {
                self.unindex(&prev.path, &other);
                self.by_path.entry(path).or_default().push(other);
            }
            if prev.ino != ino {
                self.unindex_ino(prev.ino, &other);
                if ino != 0 {
                    self.by_ino.entry(ino).or_default().push(other);
                }
            }
        } else {
            self.by_path.entry(path).or_default().push(other);
            if ino != 0 {
                self.by_ino.entry(ino).or_default().push(other);
            }
        }
        // Enforce the bound AFTER the indexes are consistent — reaping
        // walks them. Doing it on every insert, rather than at the
        // (unknown, possibly future) leak site, is the point: the
        // descriptor table stays safe even for a leak nobody has found.
        self.reap_to_lowat();
    }

    /// Remove the fd for a stateid, returning it.
    pub(crate) fn remove(&self, other: &[u8; 12]) -> Option<CachedFile> {
        let removed = self.by_stateid.remove(other)?.1;
        self.order.remove(other);
        self.unindex(&removed.path, other);
        self.unindex_ino(removed.ino, other);
        Some(removed)
    }

    /// An entry whose OPEN-time inode equals `ino` (optionally
    /// writable) — the v4-handle F17b/c fallback. Same stale-index
    /// discipline as `find_by_path`: candidates re-checked against
    /// the authoritative entry.
    pub(crate) fn find_by_ino(&self, ino: u64, require_writable: bool) -> Option<CachedFile> {
        if ino == 0 {
            return None;
        }
        let candidates: Vec<[u8; 12]> = self
            .by_ino
            .get(&ino)
            .map(|ids| ids.clone())
            .unwrap_or_default();
        for id in candidates {
            if let Some(e) = self.get(&id) {
                if e.ino == ino && (!require_writable || e.writable) {
                    return Some(e);
                }
            }
        }
        None
    }

    /// An entry whose OPEN-time path equals `path` (optionally
    /// writable), if any open fd targets it. Point lookup via the
    /// path index; the authoritative entry is re-checked so a stale
    /// index candidate degrades to a miss, never a wrong fd.
    pub(crate) fn find_by_path(
        &self,
        path: &Path,
        require_writable: bool,
    ) -> Option<CachedFile> {
        let candidates: Vec<[u8; 12]> = self
            .by_path
            .get(path)
            .map(|ids| ids.clone())
            .unwrap_or_default();
        // ^ index shard guard dropped at end of that statement.
        for id in candidates {
            if let Some(e) = self.get(&id) {
                if e.path == *path && (!require_writable || e.writable) {
                    return Some(e);
                }
            }
        }
        None
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn len(&self) -> usize {
        self.by_stateid.len()
    }

    /// A4/A5 (tier): purge every cached fd whose entry names `path`,
    /// returning the count. Eviction must call BOTH this and
    /// [`evict_ino`] — an fd cached before a rename-over is reachable
    /// only by its OPEN-time ino, one opened after only by path; the
    /// design review pinned "by path AND by ino" for exactly that
    /// aliasing (open-state enumeration alone is not sufficient).
    /// In-flight ops holding their own `Arc<File>` clones finish
    /// safely — same contract as the DS's `evict_path`. Candidates
    /// come from the index and are re-checked against the
    /// authoritative entry; a racing re-insert can at worst cost a
    /// later cache miss, never yield a wrong fd.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn evict_by_path(&self, path: &Path) -> usize {
        let candidates: Vec<[u8; 12]> = self
            .by_path
            .get(path)
            .map(|ids| ids.clone())
            .unwrap_or_default();
        let mut evicted = 0;
        for id in candidates {
            let matches = self.get(&id).is_some_and(|e| e.path == *path);
            if matches && self.remove(&id).is_some() {
                evicted += 1;
            }
        }
        evicted
    }

    /// Reap cached fds held under LOCK stateids for `ino`.
    ///
    /// THE LEAK THIS CLOSES (measured 2026-09-16, 2.04 fds per sqlite
    /// transaction, single client). READ and WRITE seed this cache under
    /// the stateid of the operation, and a LOCK mints its OWN stateid —
    /// `other[..4] == [0xFC, b'l', b'k', 0]` (`lockops.rs`, and see
    /// `minted_entry_counter` there). CLOSE reaps only the OPEN
    /// stateid's entry, so every fd cached under a lock stateid stayed
    /// for the life of the process. sqlite takes a shared then an
    /// exclusive lock per transaction, which is exactly the two
    /// descriptors per transaction that were measured.
    ///
    /// This is the same shape as the delegation leak already fixed with
    /// `fd_release` (`state/delegation.rs`): "the fd is cached under the
    /// DELEGATION stateid ... while CLOSE only reaps the OPEN stateid's
    /// entry. Nothing else was ever going to reap it." One layer over.
    ///
    /// Safe to close: flint keeps byte-range locks in its own in-memory
    /// table (`lockops.rs:23`), NOT as kernel fcntl locks on the backing
    /// file, so dropping a descriptor releases no lock. In-flight ops
    /// holding their own `Arc<File>` clone finish normally — same
    /// contract as [`evict_by_path`].
    /// Scans `by_stateid`, the AUTHORITATIVE map — not `by_ino`.
    ///
    /// The first cut of this walked `by_ino` and evicted exactly zero,
    /// which is how the fd count stayed at 2.04/txn after the "fix".
    /// `insert` only indexes by ino `if ino != 0`, so an entry cached
    /// with an unknown inode is invisible from that side. The plateau
    /// test then proved the entries ARE in this cache, so the index —
    /// not the theory — was what was wrong. Matching on path OR ino off
    /// the authoritative map cannot miss them. The cache is bounded now,
    /// so the scan is over at most `hiwat` entries.
    pub(crate) fn evict_lock_fds_for(&self, path: &Path, ino: u64) -> usize {
        let victims: Vec<[u8; 12]> = self
            .by_stateid
            .iter()
            // Only LOCK stateids (`lockops.rs` mints `other[..4] =
            // [0xFC,'l','k',0]`). An OPEN stateid's entry is reaped by
            // its own CLOSE and must not be taken from a peer that still
            // holds the file open.
            .filter(|e| e.key()[..4] == [0xFC, b'l', b'k', 0])
            .filter(|e| e.value().path == *path || (ino != 0 && e.value().ino == ino))
            .map(|e| *e.key())
            .collect();
        let mut evicted = 0;
        for id in victims {
            if self.remove(&id).is_some() {
                evicted += 1;
            }
        }
        evicted
    }

    /// What is actually IN this cache, by stateid-key prefix. Logged on
    /// reap so the composition is observable rather than inferred —
    /// three successive hypotheses about which key the leaked entries
    /// sat under were wrong before anyone simply looked.
    fn key_histogram(&self) -> String {
        let mut lock = 0usize;
        let mut other = 0usize;
        let mut zero_ino = 0usize;
        for e in self.by_stateid.iter() {
            if e.key()[..4] == [0xFC, b'l', b'k', 0] {
                lock += 1;
            } else {
                other += 1;
            }
            if e.value().ino == 0 {
                zero_ino += 1;
            }
        }
        format!("lock_stateids={lock} other_stateids={other} entries_with_ino_0={zero_ino}")
    }

    /// A4/A5 (tier): purge every cached fd whose OPEN-time inode is
    /// `ino` — the other half of eviction's purge (see
    /// [`evict_by_path`]).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn evict_by_ino(&self, ino: u64) -> usize {
        if ino == 0 {
            return 0;
        }
        let candidates: Vec<[u8; 12]> = self
            .by_ino
            .get(&ino)
            .map(|ids| ids.clone())
            .unwrap_or_default();
        let mut evicted = 0;
        for id in candidates {
            let matches = self.get(&id).is_some_and(|e| e.ino == ino);
            if matches && self.remove(&id).is_some() {
                evicted += 1;
            }
        }
        evicted
    }

    /// Drop `other` from `path`'s candidate list. Uses the Entry API:
    /// one write guard on one shard of one map for the whole
    /// retain-and-maybe-remove — no second acquisition anywhere.
    fn unindex(&self, path: &Path, other: &[u8; 12]) {
        use dashmap::mapref::entry::Entry;
        if let Entry::Occupied(mut e) = self.by_path.entry(path.to_path_buf()) {
            e.get_mut().retain(|id| id != other);
            if e.get().is_empty() {
                e.remove();
            }
        }
    }

    fn unindex_ino(&self, ino: u64, other: &[u8; 12]) {
        use dashmap::mapref::entry::Entry;
        if ino == 0 {
            return;
        }
        if let Entry::Occupied(mut e) = self.by_ino.entry(ino) {
            e.get_mut().retain(|id| id != other);
            if e.get().is_empty() {
                e.remove();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(dir: &std::path::Path, name: &str, writable: bool) -> CachedFile {
        let p = dir.join(name);
        if !p.exists() {
            std::fs::write(&p, b"x").unwrap();
        }
        let file = Arc::new(File::open(&p).unwrap());
        let ino = CachedFile::ino_of(&file);
        CachedFile {
            file,
            path: p,
            writable,
            ino,
        }
    }

    #[test]
    fn insert_get_remove_keep_index_in_step() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = FdCache::new();
        let a = entry(dir.path(), "a", true);

        cache.insert([1; 12], a.clone());
        assert!(cache.contains(&[1; 12]));
        assert_eq!(cache.get(&[1; 12]).unwrap().path, a.path);
        assert_eq!(cache.find_by_path(&a.path, false).unwrap().path, a.path);
        assert_eq!(cache.find_by_path(&a.path, true).unwrap().path, a.path);

        assert!(cache.remove(&[1; 12]).is_some());
        assert!(cache.find_by_path(&a.path, false).is_none());
        assert_eq!(cache.by_path.len(), 0, "index entry must be reaped");
    }

    #[test]
    fn writable_filter_skips_readonly_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = FdCache::new();
        cache.insert([1; 12], entry(dir.path(), "f", false));
        assert!(cache.find_by_path(&dir.path().join("f"), true).is_none());
        cache.insert([2; 12], entry(dir.path(), "f", true));
        assert!(cache.find_by_path(&dir.path().join("f"), true).is_some());
    }

    #[test]
    fn reinsert_under_new_path_moves_the_index_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = FdCache::new();
        let old = entry(dir.path(), "old", true);
        let new = entry(dir.path(), "new", true);

        cache.insert([1; 12], old.clone());
        cache.insert([1; 12], new.clone());
        assert!(cache.find_by_path(&old.path, false).is_none());
        assert_eq!(cache.find_by_path(&new.path, false).unwrap().path, new.path);
        assert_eq!(cache.by_path.len(), 1, "old index entry must be reaped");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn evict_by_path_removes_all_entries_and_reaps_indexes() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = FdCache::new();
        let a = entry(dir.path(), "a", true);
        let b = entry(dir.path(), "b", false);
        cache.insert([1; 12], a.clone());
        cache.insert([2; 12], a.clone()); // second open of the same file
        cache.insert([3; 12], b.clone());

        assert_eq!(cache.evict_by_path(&a.path), 2);
        assert_eq!(cache.len(), 1, "unrelated entry must survive");
        assert!(cache.find_by_path(&a.path, false).is_none());
        assert!(cache.find_by_ino(a.ino, false).is_none(), "ino index must be reaped too");
        assert_eq!(cache.evict_by_path(&a.path), 0, "second evict finds nothing");
    }

    /// The aliasing that pinned "by path AND by ino" in the design
    /// review: an fd cached before a rename keeps its OPEN-time path,
    /// so evicting the file at its CURRENT path misses it — only the
    /// ino reaches it.
    #[test]
    fn evict_by_ino_catches_the_renamed_over_fd() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = FdCache::new();
        let old = entry(dir.path(), "old-name", true);
        cache.insert([1; 12], old.clone());

        let new_path = dir.path().join("new-name");
        std::fs::rename(&old.path, &new_path).unwrap();

        assert_eq!(cache.evict_by_path(&new_path), 0, "OPEN-time path is stale");
        assert_eq!(cache.evict_by_ino(old.ino), 1, "the ino still names the fd");
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.evict_by_ino(0), 0, "ino 0 (unknown) must never match");
    }

    /// The F24 shape cannot recur here by construction, but keep the
    /// operational proof: hammer insert-find-insert on one shared path
    /// (the postgres pg_internal.init pattern) under a watchdog.
    #[test]
    fn shared_path_insert_find_storm_does_not_deadlock() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = Arc::new(FdCache::new());
        cache.insert([0; 12], entry(dir.path(), "shared", true));
        let path = dir.path().join("shared");

        let c = Arc::clone(&cache);
        let worker = std::thread::spawn(move || {
            for i in 1u16..=512 {
                let mut other = [0u8; 12];
                other[..2].copy_from_slice(&i.to_le_bytes());
                let found = c.find_by_path(&path, false).unwrap();
                c.insert(other, found);
            }
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !worker.is_finished() {
            assert!(
                std::time::Instant::now() < deadline,
                "FdCache deadlocked on a same-shard lookup+insert"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        worker.join().unwrap();
        assert_eq!(cache.len(), 513);
    }

    /// Guard-discipline lint (identity.rs precedent): no `if let` /
    /// `while let` in the NFS/pNFS trees may iterate a map in its
    /// scrutinee — scrutinee temporaries outlive the block, so an
    /// iterator's shard/lock guard would be held across everything the
    /// block does (the exact F24 deadlock shape). Bind the lookup with
    /// a standalone `let` instead. Test modules are exempt; deliberate
    /// exceptions carry `guard-lint: allow` on the offending line.
    #[test]
    fn no_iter_guards_in_scrutinees() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        for tree in ["nfs", "pnfs"] {
            walk(&base.join(tree), &mut files);
        }
        assert!(files.len() > 10, "source walk looks broken: {} files", files.len());

        let mut violations = Vec::new();
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            // Convention (same as identity-lint): unit-test modules sit
            // at the END of a file behind `#[cfg(test)]`.
            let prod = match text.find("#[cfg(test)]") {
                Some(i) => &text[..i],
                None => &text[..],
            };
            for kw in ["if let ", "while let "] {
                let mut from = 0;
                while let Some(rel) = prod[from..].find(kw) {
                    let start = from + rel;
                    from = start + kw.len();
                    // The scrutinee spans from the keyword to the block
                    // opener. A `{` inside the scrutinee (struct literal)
                    // would end the slice early — that only ever shrinks
                    // the lint's view, never flags extra.
                    let end = prod[start..]
                        .find('{')
                        .map(|i| start + i)
                        .unwrap_or(prod.len());
                    let scrutinee = &prod[start..end];
                    // The allow marker may sit in the scrutinee or on
                    // the line directly above the `if let`.
                    let win_start = prod[..start]
                        .rfind('\n')
                        .map(|i| prod[..i].rfind('\n').map(|j| j + 1).unwrap_or(0))
                        .unwrap_or(0);
                    let window = &prod[win_start..end];
                    if scrutinee.contains(".iter()") && !window.contains("guard-lint: allow")
                    {
                        let lineno = prod[..start].lines().count();
                        violations.push(format!(
                            "{}:{}: {}",
                            f.display(),
                            lineno,
                            scrutinee.split_whitespace().collect::<Vec<_>>().join(" ")
                        ));
                    }
                }
            }
        }
        assert!(
            violations.is_empty(),
            "iterator guard held across an if/while-let block (bind with a \
             standalone `let` first — see F24 in fd_cache.rs):\n{}",
            violations.join("\n")
        );
    }
}
