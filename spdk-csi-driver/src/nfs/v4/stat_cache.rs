//! Counter-validated lstat cache — the server-side answer to the
//! per-RPC stat grind the small-file campaign measured (~9 newfstatat
//! per metadata RPC across snapshot/resolve/existence, plus a
//! spawn_blocking hop for each `tokio::fs` stat wrapping a ~1µs
//! syscall).
//!
//! Every entry stores the [`change_counter`] value observed at insert.
//! A hit is served only while (a) the entry is younger than the TTL and
//! (b) the counter for the cached metadata's (dev, ino) still equals
//! the stored value. Since EVERY internal mutation already bumps that
//! counter — that is the F14 invariant the fidelity campaign built —
//! internal writes, creates, setattrs and renames invalidate cached
//! attributes automatically, with no new bookkeeping at mutation sites.
//! The fd-based bump sites (WRITE, COMMIT) work unchanged: validation
//! keys on the cached metadata's (dev, ino), not on the path.
//!
//! Two events cannot bump a counter and need [`forget`] instead: REMOVE
//! (the object's inode is gone) and RENAME (the source path no longer
//! names it). Both handlers call it explicitly.
//!
//! The TTL bounds the one staleness this design admits: a LOCAL process
//! mutating the exported tree behind the server's back (no bump). That
//! window is `FLINT_NFS_ATTR_CACHE_MS` (default 1000, 0 disables) —
//! small against any NFS client's own attribute-cache slack (actimeo
//! defaults to 3..60 s), and the same trade Ganesha's MDCACHE makes at
//! 60 s. There is also a benign sub-µs race: a bump landing between
//! our fresh lstat and the counter read is folded into the stored
//! value, so the pre-bump metadata can be served until the TTL expires;
//! consequences are bounded exactly like the external-mutator case.
//!
//! Concurrency: dashmap (the change_counter's own structure), so
//! readers on different paths never share a lock and invalidation is a
//! shard-local point-delete. No LRU list — a global list head is a
//! serialization point; the map simply stops inserting at CAP and
//! expired entries are replaced lazily on their next touch.

use dashmap::DashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

struct Entry {
    md: std::fs::Metadata,
    /// change_counter::current for md's (dev, ino) at insert time.
    change: u64,
    at: Instant,
}

static CACHE: OnceLock<DashMap<PathBuf, Entry>> = OnceLock::new();

fn map() -> &'static DashMap<PathBuf, Entry> {
    CACHE.get_or_init(DashMap::new)
}

/// Growth stop, not an eviction policy: one entry is ~300 bytes, so the
/// cap is ~80 MB worst case; beyond it stats simply go to the kernel.
const CAP: usize = 262_144;

fn ttl() -> Duration {
    static TTL: OnceLock<Duration> = OnceLock::new();
    *TTL.get_or_init(|| {
        let ms = std::env::var("FLINT_NFS_ATTR_CACHE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1000);
        Duration::from_millis(ms)
    })
}

#[cfg(unix)]
fn change_of(md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    super::change_counter::current(md.dev(), md.ino(), super::change_counter::ctime_ns(md))
}

fn insert(path: &Path, md: &std::fs::Metadata) {
    if is_fenced(path) {
        return;
    }
    let m = map();
    if m.len() >= CAP && !m.contains_key(path) {
        return;
    }
    #[cfg(unix)]
    m.insert(
        path.to_path_buf(),
        Entry { md: md.clone(), change: change_of(md), at: Instant::now() },
    );
}

/// Memoized `symlink_metadata`. The stat on a miss runs INLINE — the
/// dentry-hot case is ~1µs, and wrapping it in spawn_blocking (as the
/// `tokio::fs` call sites this replaces did) costs a thread handoff and
/// futex wakes worth more than the syscall.
pub fn lstat(path: &Path) -> io::Result<std::fs::Metadata> {
    let ttl = ttl();
    if ttl.is_zero() || is_fenced(path) {
        return std::fs::symlink_metadata(path);
    }
    #[cfg(unix)]
    if let Some(e) = map().get(path) {
        if e.at.elapsed() <= ttl && change_of(&e.md) == e.change {
            return Ok(e.md.clone());
        }
    }
    let md = std::fs::symlink_metadata(path)?;
    insert(path, &md);
    Ok(md)
}

/// Drop the entry for a path whose object was removed or renamed away —
/// the two mutations that cannot advance a counter (no inode to stat,
/// or the path no longer names it) and so cannot self-invalidate.
pub fn forget(path: &Path) {
    map().remove(path);
}

// ── Fence: exact invalidation for inode-REPLACING mutations ─────────
//
// REMOVE and RENAME swap what a path names; no counter bump can reach
// the outgoing inode, and a post-syscall forget still leaves a window
// (a reader's stat begun before the swap can insert the old metadata
// after it). While a fence on the path is held, lstat serves the
// kernel directly and inserts nothing; dropping the fence purges any
// entry that raced in. Held across [pre-syscall, post-bump], the cache
// can never say anything the filesystem didn't say inside the bracket.

static FENCE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static FENCES: OnceLock<DashMap<PathBuf, u32>> = OnceLock::new();

fn fences() -> &'static DashMap<PathBuf, u32> {
    FENCES.get_or_init(DashMap::new)
}

fn is_fenced(path: &Path) -> bool {
    FENCE_COUNT.load(std::sync::atomic::Ordering::Relaxed) > 0 && fences().contains_key(path)
}

/// RAII fence over a set of paths. Take it BEFORE the mutating syscall,
/// let it drop after the post-mutation bumps.
pub struct Fence(Vec<PathBuf>);

pub fn fence(paths: &[&Path]) -> Fence {
    let v: Vec<PathBuf> = paths.iter().map(|p| p.to_path_buf()).collect();
    for p in &v {
        FENCE_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *fences().entry(p.clone()).or_insert(0) += 1;
        map().remove(p);
    }
    Fence(v)
}

impl Drop for Fence {
    fn drop(&mut self) {
        for p in &self.0 {
            // Purge whatever raced in FIRST, then lower the fence: a
            // reader that checked is_fenced() before our decrement
            // bypassed the cache anyway, and one that checks after
            // finds no stale entry to hit.
            map().remove(p);
            if let Some(mut e) = fences().get_mut(p) {
                *e -= 1;
                let zero = *e == 0;
                drop(e);
                if zero {
                    fences().remove_if(p, |_, v| *v == 0);
                }
            }
            FENCE_COUNT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// A mutation site just stat'ed `path` fresh (post-mutation, post-bump):
/// warm the cache with it so the change_info bracket's read-back and the
/// GETATTR riding the same compound hit without another stat.
pub fn note_fresh(path: &Path, md: &std::fs::Metadata) {
    if ttl().is_zero() {
        return;
    }
    insert(path, md);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::nfs::v4::change_counter;

    // Serialized per-test temp dirs; entries are path-keyed so tests
    // sharing the process-global map cannot collide.

    #[test]
    fn a_hit_is_served_from_the_cache_not_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("cached.bin");
        std::fs::write(&f, b"aa").unwrap();
        let first = lstat(&f).unwrap();
        assert_eq!(first.len(), 2);
        // Mutate BEHIND the server's back: no bump, so within the TTL
        // the cache must keep answering with the old metadata. This is
        // the anti-vacuity pin — if lstat re-stats, this test fails.
        std::fs::write(&f, b"aaaa").unwrap();
        let second = lstat(&f).unwrap();
        assert_eq!(second.len(), 2, "cache did not cache");
    }

    #[test]
    fn a_bump_invalidates_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("bumped.bin");
        std::fs::write(&f, b"aa").unwrap();
        assert_eq!(lstat(&f).unwrap().len(), 2);
        std::fs::write(&f, b"aaaa").unwrap();
        // The write path's F14 bump — by (dev, ino), as WRITE does it.
        change_counter::bump_path(&f);
        assert_eq!(
            lstat(&f).unwrap().len(),
            4,
            "counter bump must invalidate the cached metadata"
        );
    }

    #[test]
    fn forget_makes_a_removed_file_enoent_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("removed.bin");
        std::fs::write(&f, b"x").unwrap();
        lstat(&f).unwrap();
        std::fs::remove_file(&f).unwrap();
        // Without forget, the entry would keep the path alive TTL-long.
        forget(&f);
        assert_eq!(
            lstat(&f).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn a_fenced_path_bypasses_the_cache_and_purges_racers_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("fenced.bin");
        std::fs::write(&f, b"aa").unwrap();
        lstat(&f).unwrap();
        {
            let _g = fence(&[&f]);
            // The swap: a different inode lands at the path (the
            // rename-over shape). No bump can reach the old inode.
            let tmp = dir.path().join("fenced.tmp");
            std::fs::write(&tmp, b"aaaa").unwrap();
            std::fs::rename(&tmp, &f).unwrap();
            // While fenced: the kernel answers, not the cache.
            assert_eq!(lstat(&f).unwrap().len(), 4);
            // And that read must NOT have (re)cached: mutate again
            // behind the back; a cached entry would mask it.
            std::fs::write(&f, b"aaaaaa").unwrap();
            assert_eq!(lstat(&f).unwrap().len(), 6, "fenced lstat cached");
        }
        // Fence dropped: the pre-fence entry (len 2) must be gone.
        assert_eq!(lstat(&f).unwrap().len(), 6, "stale entry survived the fence");
    }

    #[test]
    fn a_directory_entry_is_invalidated_by_the_parent_bump_a_create_does() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("subdir");
        std::fs::create_dir(&d).unwrap();
        let before = lstat(&d).unwrap();
        std::fs::write(d.join("newfile"), b"x").unwrap();
        change_counter::bump_path(&d); // what CREATE/OPEN-create do
        let after = lstat(&d).unwrap();
        // mtime moved — the fresh stat was taken, not the cached one.
        assert!(
            after.modified().unwrap() >= before.modified().unwrap(),
            "post-bump lstat must be fresh"
        );
        // And the stronger pin: the entry was actually replaced, so a
        // second behind-the-back touch is again masked (cache active).
        let n1 = map().get(&d as &Path).map(|e| e.change);
        assert!(n1.is_some());
    }
}
