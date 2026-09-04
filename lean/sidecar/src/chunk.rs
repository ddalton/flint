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

// ── the wire format ─────────────────────────────────────────────────

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::manifest::LeanEntry;
use super::{LeanError, LeanResult};

/// One entry of the pointer's chunk list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    /// Content address; the object lives at `.flint/lean/chunks/<addr>`.
    pub addr: String,
    /// The chunk's FIRST key. Carried so a reader can resolve a path to
    /// its chunk without fetching anything (design §5's partial reads),
    /// and so `assemble` can check the list covers the key space in
    /// order rather than assuming it.
    pub first: String,
    /// Entry count, so a truncated or substituted chunk is caught
    /// against a number the pointer committed to.
    pub n: usize,
}

/// A chunk object's body.
///
/// A struct rather than a bare map so the object is self-describing and
/// so a future field does not change what the address hashes over.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkBody {
    pub entries: BTreeMap<String, LeanEntry>,
}

fn encode(body: &ChunkBody) -> LeanResult<Vec<u8>> {
    serde_json::to_vec(body).map_err(|e| LeanError::State(format!("chunk encode: {e}")))
}

/// Split entries into chunk bodies and their references.
///
/// `BTreeMap` iterates in sorted order and `LeanEntry` holds no floats,
/// so identical entry sets encode to identical bytes and therefore to
/// identical addresses. That determinism is not a nicety — it is the
/// whole dedup mechanism: an untouched chunk must re-address to the
/// object that already exists, or every publish uploads everything.
pub fn split_with(
    entries: &BTreeMap<String, LeanEntry>,
    target: usize,
    min: usize,
    max: usize,
) -> LeanResult<Vec<(ChunkRef, Vec<u8>)>> {
    let keys: Vec<&str> = entries.keys().map(|s| s.as_str()).collect();
    let cuts = chunk_ranges_with(&keys, target, min, max);
    let mut out = Vec::with_capacity(cuts.len());
    let mut start = 0usize;
    for c in cuts {
        let slice: BTreeMap<String, LeanEntry> = keys[start..c]
            .iter()
            .map(|k| ((*k).to_string(), entries[*k].clone()))
            .collect();
        let bytes = encode(&ChunkBody { entries: slice })?;
        out.push((
            ChunkRef {
                addr: chunk_address(&bytes),
                first: keys[start].to_string(),
                n: c - start,
            },
            bytes,
        ));
        start = c;
    }
    Ok(out)
}

pub fn split(entries: &BTreeMap<String, LeanEntry>) -> LeanResult<Vec<(ChunkRef, Vec<u8>)>> {
    split_with(entries, CHUNK_TARGET, CHUNK_MIN, CHUNK_MAX)
}

/// Rebuild the entry map from a chunk list and the fetched bodies.
///
/// Every invariant the format depends on is CHECKED here rather than
/// assumed, because each one fails silently if it is not. A manifest
/// used to be one object: it was either there or it was not. Split
/// across N objects, a wrong list produces a well-formed manifest that
/// is quietly missing entries — and a missing entry reads to every
/// consumer as a file the agent deleted.
///
/// Checked: the list is in strictly increasing key order; each body
/// addresses to the reference that named it; each body's length and
/// first key match what the pointer committed to; no chunk spills past
/// the next chunk's first key; and no key arrives twice.
pub fn assemble(refs: &[ChunkRef], bodies: &[Vec<u8>]) -> LeanResult<BTreeMap<String, LeanEntry>> {
    if refs.len() != bodies.len() {
        return Err(LeanError::State(format!(
            "chunk list names {} chunks but {} bodies were fetched",
            refs.len(),
            bodies.len()
        )));
    }
    let mut out: BTreeMap<String, LeanEntry> = BTreeMap::new();
    for (i, (r, b)) in refs.iter().zip(bodies).enumerate() {
        if i > 0 && refs[i - 1].first >= r.first {
            return Err(LeanError::State(format!(
                "chunk list is not in strictly increasing key order: {:?} then {:?}",
                refs[i - 1].first, r.first
            )));
        }
        // The object was fetched BY address, so a mismatch means the
        // store handed back something else — a stale read, a wrong key,
        // a truncated body. Content addressing makes this checkable for
        // one hash per chunk, and declining to check it would be
        // choosing not to know.
        let got = chunk_address(b);
        if got != r.addr {
            return Err(LeanError::State(format!(
                "chunk {} does not match its address (body hashes to {got})",
                r.addr
            )));
        }
        let body: ChunkBody = serde_json::from_slice(b)
            .map_err(|e| LeanError::State(format!("chunk {} parse: {e}", r.addr)))?;
        if body.entries.len() != r.n {
            return Err(LeanError::State(format!(
                "chunk {} holds {} entries, the pointer says {}",
                r.addr,
                body.entries.len(),
                r.n
            )));
        }
        match body.entries.keys().next() {
            Some(k) if *k == r.first => {}
            Some(k) => {
                return Err(LeanError::State(format!(
                    "chunk {} starts at {k:?}, the pointer says {:?}",
                    r.addr, r.first
                )))
            }
            None => {
                return Err(LeanError::State(format!(
                    "chunk {} is empty; an empty chunk cannot be addressed by a first key",
                    r.addr
                )))
            }
        }
        if let (Some(next), Some(last)) = (refs.get(i + 1), body.entries.keys().next_back()) {
            if *last >= next.first {
                return Err(LeanError::State(format!(
                    "chunk {} runs to {last:?}, past the next chunk's first key {:?} — the \
                     chunks overlap and one shadows the other",
                    r.addr, next.first
                )));
            }
        }
        for (k, v) in body.entries {
            if out.insert(k.clone(), v).is_some() {
                return Err(LeanError::State(format!(
                    "key {k:?} appears in more than one chunk"
                )));
            }
        }
    }
    let expect: usize = refs.iter().map(|r| r.n).sum();
    if out.len() != expect {
        return Err(LeanError::State(format!(
            "assembled {} entries, the chunk list accounts for {expect}",
            out.len()
        )));
    }
    Ok(out)
}
