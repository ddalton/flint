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
}

impl FdCache {
    pub(crate) fn new() -> Self {
        Self {
            by_stateid: DashMap::new(),
            by_path: DashMap::new(),
        }
    }

    pub(crate) fn contains(&self, other: &[u8; 12]) -> bool {
        self.by_stateid.contains_key(other)
    }

    /// Owned clone of the entry for this stateid, if any.
    pub(crate) fn get(&self, other: &[u8; 12]) -> Option<CachedFile> {
        self.by_stateid.get(other).map(|e| e.clone())
    }

    /// Insert (or replace) the fd for a stateid, keeping the path
    /// index in step.
    pub(crate) fn insert(&self, other: [u8; 12], entry: CachedFile) {
        let path = entry.path.clone();
        let prev = self.by_stateid.insert(other, entry);
        if let Some(prev) = prev {
            if prev.path == path {
                return; // already indexed under this path
            }
            self.unindex(&prev.path, &other);
        }
        self.by_path.entry(path).or_default().push(other);
    }

    /// Remove the fd for a stateid, returning it.
    pub(crate) fn remove(&self, other: &[u8; 12]) -> Option<CachedFile> {
        let removed = self.by_stateid.remove(other)?.1;
        self.unindex(&removed.path, other);
        Some(removed)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(dir: &std::path::Path, name: &str, writable: bool) -> CachedFile {
        let p = dir.join(name);
        if !p.exists() {
            std::fs::write(&p, b"x").unwrap();
        }
        CachedFile {
            file: Arc::new(File::open(&p).unwrap()),
            path: p,
            writable,
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
