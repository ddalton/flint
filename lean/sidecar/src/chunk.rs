//! Content-defined chunking of the manifest's key stream
//! (`docs/plans/flint-lean-chunked-manifest-design.md` §3).
//!
//! The pointer layout made a TAKEOVER cheap; it did nothing for a
//! publish, which still writes every entry of the project to change one
//! file. Splitting the sorted key stream into immutable, content-
//! addressed chunks makes a publish rewrite only what it touched.
//!
//! Everything here is pure and synchronous on purpose: the boundary
//! rule is part of the on-the-wire format, and a format decision that
//! can only be exercised through a store is one nobody tests properly.

use flint_store::crc64_nvme;

/// Entries per chunk, in expectation. ~1 MiB of compact JSON at the
/// ~277 B/entry this manifest measures: big enough that per-object
/// overhead is noise, small enough that one changed file rewrites ~1
/// MiB rather than the whole project.
pub const CHUNK_TARGET: usize = 4096;
/// Floor. Hash boundaries are geometrically distributed, so without one
/// a project accumulates many tiny objects.
pub const CHUNK_MIN: usize = CHUNK_TARGET / 4;
/// Ceiling, so one pathological run cannot recreate the single-object
/// problem this exists to remove.
pub const CHUNK_MAX: usize = CHUNK_TARGET * 4;

/// Does a chunk end after this key?
///
/// A function of the KEY ALONE — never of the entry's contents, and
/// never of the key's position. Both matter:
///
/// - Position would be fixed-count chunking, where inserting one key
///   near the front shifts every later key by one slot, moves every
///   boundary and rewrites every chunk. That restores O(entries) on
///   exactly the operation being optimised (§3).
/// - Contents would move a boundary whenever a file's size or etag
///   changed, so editing a file in place would re-split the project
///   instead of rewriting one chunk.
fn cuts_after_with(key: &str, target: usize) -> bool {
    crc64_nvme(key.as_bytes()).is_multiple_of(target as u64)
}

/// Split a SORTED key stream into content-defined runs, as end-exclusive
/// indices. `[]` for no keys; the last run always ends at `keys.len()`.
///
/// `min`/`max` are where the pure-function property leaks: a suppressed
/// or forced cut depends on where the previous boundary fell, so a
/// change can cascade — but only to the next natural boundary that
/// satisfies `min`, which is one chunk in expectation. The guarantee is
/// "O(changed) in expectation", not worst case, and this is why.
pub fn chunk_ranges_with(keys: &[&str], target: usize, min: usize, max: usize) -> Vec<usize> {
    debug_assert!(target > 0 && min > 0 && max >= min, "degenerate chunk sizing");
    let mut cuts = Vec::new();
    let mut run = 0usize;
    for (i, k) in keys.iter().enumerate() {
        run += 1;
        if (run >= min && cuts_after_with(k, target)) || run >= max {
            cuts.push(i + 1);
            run = 0;
        }
    }
    // The tail is a chunk even when it never hit a boundary. Dropping it
    // would silently lose every entry after the last cut.
    if run > 0 {
        cuts.push(keys.len());
    }
    cuts
}

/// The shipped sizing.
pub fn chunk_ranges(keys: &[&str]) -> Vec<usize> {
    chunk_ranges_with(keys, CHUNK_TARGET, CHUNK_MIN, CHUNK_MAX)
}

/// The content address of a chunk body: SHA-256, truncated to 128 bits.
///
/// NOT `crc64_nvme`, which the boundary rule above uses. The two hashes
/// are answering different questions and carry different consequences:
/// a boundary collision merely moves a cut and costs nothing, while an
/// ADDRESS collision means two different chunks share an object key and
/// one silently shadows the other — a data-loss class, and a silent one.
/// 64 bits is not enough margin to take that risk for; 128 is past any
/// practical concern.
pub fn chunk_address(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(body);
    d.iter().take(16).map(|b| format!("{b:02x}")).collect()
}
