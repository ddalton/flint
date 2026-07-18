//! Server-maintained per-file change counter (F14) — the moral
//! equivalent of the kernel's i_version, which this userspace server
//! cannot read portably.
//!
//! The fattr4 CHANGE attribute is the client's only cache-ordering key.
//! Deriving it purely from ctime leaves ties within one filesystem
//! clock tick (~1-4ms on ext4): an OPEN-create and its first WRITE — or
//! two extends of a COPY burst — carry the SAME change value, so an
//! out-of-order GETATTR reply carrying the older (shorter, or zero)
//! size is indistinguishable from fresh and can land in the client's
//! inode cache after the newer one. Observed live as postgres reading
//! its own postmaster.pid back as empty ("lock file contains wrong
//! PID: 0") and as pgbench's "unexpected data beyond EOF" (F13's
//! whole-second variant of the same disease).
//!
//! Every mutating op bumps the file's counter with the post-mutation
//! ctime (ns) as a floor; GETATTR reports max(counter, ctime_ns).
//! Within a server lifetime the counter is strictly monotonic per
//! mutation; across a restart the map is empty and the ctime floor
//! keeps values monotonic as long as the clock advances past the last
//! mutation's tick — the same guarantee knfsd gives without i_version,
//! which the in-lifetime counter then strengthens.
//!
//! Keyed by (dev, ino): filehandles/paths may alias across exports.
//! Unbounded growth is one u64 per file ever mutated in this server's
//! lifetime — the NFS pod is per-volume and restarts with it.

use dashmap::DashMap;
use std::sync::OnceLock;

static COUNTERS: OnceLock<DashMap<(u64, u64), u64>> = OnceLock::new();

fn map() -> &'static DashMap<(u64, u64), u64> {
    COUNTERS.get_or_init(DashMap::new)
}

/// Record a mutation of (dev, ino). `floor` is the post-mutation ctime
/// in nanoseconds; the stored value always advances by at least 1.
pub fn bump(dev: u64, ino: u64, floor: u64) {
    let mut e = map().entry((dev, ino)).or_insert(floor);
    *e = (*e).max(floor).wrapping_add(1);
}

/// The change value to report for (dev, ino): the mutation counter when
/// this server lifetime has seen one, else the ctime floor.
pub fn current(dev: u64, ino: u64, floor: u64) -> u64 {
    map().get(&(dev, ino)).map(|v| (*v).max(floor)).unwrap_or(floor)
}

/// ctime of `md` composed to nanoseconds — the floor everywhere.
#[cfg(unix)]
pub fn ctime_ns(md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    (md.ctime() as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(md.ctime_nsec() as u64)
}

/// Convenience: stat `path` and bump its entry. Best effort — a failed
/// stat (raced unlink) simply skips the bump; the object is gone and
/// its parent directory's own bump carries the invalidation.
pub fn bump_path(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(md) = path.symlink_metadata() {
            bump(md.dev(), md.ino(), ctime_ns(&md));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_is_strictly_monotonic_even_with_tied_floors() {
        let (dev, ino) = (7, 777);
        let floor = 1_000_000_000_000;
        assert_eq!(current(dev, ino, floor), floor); // untouched: floor
        bump(dev, ino, floor);
        let a = current(dev, ino, floor);
        bump(dev, ino, floor); // SAME floor (same clock tick)
        let b = current(dev, ino, floor);
        assert!(b > a, "tied ctime floors must still advance: {} !> {}", b, a);
        // A later, larger floor wins over the counter.
        let big = floor + 10_000_000_000;
        bump(dev, ino, big);
        assert!(current(dev, ino, big) > big);
    }

    #[test]
    fn distinct_files_do_not_interfere() {
        bump(1, 1, 100);
        assert_eq!(current(1, 2, 50), 50);
    }
}
