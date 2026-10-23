//! A local copy of manifest chunks, kept by content address (latency plan
//! 2026-10-23, lever A: `docs/plans/flint-lean-latency-plan-2026-10-23.md`).
//!
//! Every manifest load used to fetch every chunk the pointer names, one
//! sequential GET per 4,096 entries: 49 GETs at 200k files and 220 at 1M,
//! on every pointer move, in the consume AND the commit, and twice per UI
//! save in the gateway. Consecutive documents differ in one or two chunks,
//! and a chunk never changes once written (its key IS the hash of its
//! body), so a reader that kept the bodies it already has needs to fetch
//! only the chunks that are new.
//!
//! What makes keeping them safe:
//!
//! - **Content, not trust.** A cached body is used only after it hashes to
//!   the address the pointer names (`manifest::load_from`). A damaged,
//!   truncated or planted file is a miss: dropped, and fetched. A forged
//!   body would need a preimage of the 128-bit address.
//! - **Existence is still checked.** The cache can vouch for a chunk's
//!   bytes, never for the chunk still being in the bucket. A load that
//!   served any chunk from here also LISTs the chunk prefix, once, and a
//!   chunk the live pointer names that the bucket no longer holds is
//!   decided exactly as a 404 was: the pointer moved (a sweep took a
//!   superseded generation) restarts the load, an unchanged pointer is a
//!   hole and refuses. A document is never served short, and a live hole
//!   is never papered over.
//! - **Remote names never become local paths unchecked.** An address
//!   comes from the bucket; `chunk::is_chunk_address` admits 32 lowercase
//!   hex digits and nothing else, so no pointer can name `../x`.
//!
//! Two implementations: [`DiskChunkCache`] for the syncer (a directory in
//! its state dir, so it survives a container restart and costs no memory
//! between barriers), and [`MemoryChunkCache`] for the gateway (no local
//! tree; one per workspace, beside its manifest cache). Both keep only the
//! chunks of the document loaded last (`retain`), so either holds about
//! one manifest: ~277 bytes per entry, 66 MiB at 250k files.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use bytes::Bytes;

use super::chunk::is_chunk_address;

/// The largest body either cache keeps. A chunk is at most
/// `CHUNK_MAX` entries, a few MiB; the tombstones object is bounded by
/// `TOMBSTONE_KEEP_SEQS`. Anything larger is fetched every time rather than
/// read into memory from a file the app could have grown.
pub const MAX_CACHED_BODY: u64 = 64 * 1024 * 1024;

/// Chunk bodies by content address. Every method is best effort: a cache
/// that cannot read or write costs requests, never correctness, so nothing
/// here returns an error.
pub trait ChunkCache: Send + Sync {
    /// The body kept under `addr`, if any. NOT verified here: the loader
    /// hashes every body once, whichever way it arrived.
    fn get(&self, addr: &str) -> Option<Bytes>;
    /// Keep `body` under `addr`. The caller has verified it.
    fn put(&self, addr: &str, body: &[u8]);
    /// Drop `addr` (it failed verification).
    fn remove(&self, addr: &str);
    /// Keep only these addresses.
    fn retain(&self, live: &HashSet<String>);
}

/// The gateway's cache: bodies in memory, one map per workspace.
#[derive(Default)]
pub struct MemoryChunkCache {
    map: Mutex<HashMap<String, Bytes>>,
}

impl MemoryChunkCache {
    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ChunkCache for MemoryChunkCache {
    fn get(&self, addr: &str) -> Option<Bytes> {
        self.map.lock().unwrap().get(addr).cloned()
    }
    fn put(&self, addr: &str, body: &[u8]) {
        if !is_chunk_address(addr) || body.len() as u64 > MAX_CACHED_BODY {
            return;
        }
        self.map.lock().unwrap().insert(addr.to_string(), Bytes::copy_from_slice(body));
    }
    fn remove(&self, addr: &str) {
        self.map.lock().unwrap().remove(addr);
    }
    fn retain(&self, live: &HashSet<String>) {
        self.map.lock().unwrap().retain(|k, _| live.contains(k));
    }
}

/// The syncer's cache: one file per chunk under the state dir
/// (`.flint-sync/chunk-cache/<addr>`). The state dir sits in the
/// app-writable tree, so every access is the state dir's discipline: no
/// symlink is followed at the directory or the file, a FIFO or a
/// directory at a chunk's name is a miss, writes go through
/// `safefs::write_via_tmp_fast`, and every body read is verified by its
/// caller before use.
pub struct DiskChunkCache {
    dir: PathBuf,
    warned: AtomicBool,
}

impl DiskChunkCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        DiskChunkCache { dir: dir.into(), warned: AtomicBool::new(false) }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// The directory exists and is a directory, not a link to one.
    fn usable(&self) -> bool {
        std::fs::symlink_metadata(&self.dir).is_ok_and(|m| m.is_dir())
    }

    /// Make the directory if it is not there: refused when the state dir
    /// above it is a symlink (`check_parent`), and never through one.
    fn ensure(&self) -> bool {
        if self.usable() {
            return true;
        }
        if super::safefs::check_parent(&self.dir).is_err() {
            return false;
        }
        match std::fs::create_dir(&self.dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return false,
        }
        self.usable()
    }

    fn warn_once(&self, what: &str) {
        if !self.warned.swap(true, Ordering::Relaxed) {
            eprintln!(
                "flint-sync: the chunk cache at {} is not usable ({what}); manifest loads fetch \
                 every chunk from the bucket",
                self.dir.display()
            );
        }
    }
}

impl ChunkCache for DiskChunkCache {
    fn get(&self, addr: &str) -> Option<Bytes> {
        if !is_chunk_address(addr) || !self.usable() {
            return None;
        }
        // No link at the file either, a FIFO refused rather than waited on,
        // and only a regular file returned.
        let f = super::safefs::open_beneath_nofollow(&self.dir, addr).ok()?;
        let len = f.metadata().ok()?.len();
        if len > MAX_CACHED_BODY {
            return None;
        }
        let mut buf = Vec::with_capacity(len as usize);
        (&f).take(MAX_CACHED_BODY + 1).read_to_end(&mut buf).ok()?;
        Some(Bytes::from(buf))
    }

    fn put(&self, addr: &str, body: &[u8]) {
        if !is_chunk_address(addr) || body.len() as u64 > MAX_CACHED_BODY {
            return;
        }
        if !self.ensure() {
            self.warn_once("its directory cannot be made");
            return;
        }
        let path = self.dir.join(addr);
        // Per process: a `flint-sync status` beside the running syncer
        // shares this directory, and two writers of one temp name would
        // tear each other's file (harmlessly: it would fail verification).
        let tmp = self.dir.join(format!("{addr}.{}{}", std::process::id(), super::scan::TMP_SUFFIX));
        let wrote = super::safefs::check_parent(&path)
            .and_then(|_| super::safefs::write_via_tmp_fast(&path, &tmp, body, None).map(|_| ()));
        if let Err(e) = wrote {
            self.warn_once(&e.to_string());
        }
    }

    fn remove(&self, addr: &str) {
        if is_chunk_address(addr) && self.usable() {
            let _ = std::fs::remove_file(self.dir.join(addr));
        }
    }

    fn retain(&self, live: &HashSet<String>) {
        if !self.usable() {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return };
        for entry in rd.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Only names this cache writes are ever removed: a chunk no
            // longer named, or a temp file a crash left behind.
            let stale = if is_chunk_address(name) {
                !live.contains(name)
            } else {
                name.ends_with(super::scan::TMP_SUFFIX)
                    && name.get(..32).is_some_and(is_chunk_address)
            };
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}
