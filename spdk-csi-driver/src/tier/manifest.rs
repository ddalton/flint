//! The DR manifest — L2 step 12 (design review A12).
//!
//! A small JSON object at `<prefix>.flint/manifest`, rewritten at
//! every flush barrier whose content changed. It is simultaneously:
//!
//! - **the metadata checkpoint** — per-file mode/uid/gid/mtime, plus
//!   directories and symlink targets, none of which the per-file
//!   objects can carry on their own;
//! - **the RPO record** — `seq` + `written_unix` + per-file
//!   {generation, ETag, CRC} say exactly which content the bucket
//!   holds, readable from the bucket ALONE (no local state, no hub);
//! - **the restore driver** — `tier::import` rebuilds the tree from
//!   it: directories and symlinks materialize directly, regular files
//!   materialize as evicted stubs that hydrate on first touch.
//!
//! DR is therefore: CAS (re-provision the PVC) + manifest-driven
//! restore + consumer remount.
//!
//! **What the tier declines to round-trip** (enumerated per A12 — the
//! restore is a lossy import for these, by contract):
//!
//! - **hard links** — LINK is refused on tiered volumes (A7); a
//!   pre-existing hard-linked pair restores as two independent files;
//! - **sockets, FIFOs, device nodes** — never uploaded, never
//!   manifested (counted in `skipped_special`);
//! - **sparseness** — a restored file is dense; holes come back as
//!   real zero bytes;
//! - **files never yet published** — dirty-at-loss content is beyond
//!   the RPO by definition (counted in `beyond_rpo`, so the manifest
//!   states its own loss bound);
//! - **atime, xattrs beyond the tier's own marker, and ACLs**.
//!
//! The write is guarded like every publish (A6): If-Match on the
//! previous manifest, If-None-Match:* for the first — the epoch holder
//! owns it; a 412 here means deposition or foreign interference and is
//! logged, never retried blind.

use crate::tier::flush::GenRecord;
use crate::tier::store::{GenerationStamps, ObjectStore, PutCondition, StoreError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::{debug, info, warn};

/// The manifest's bucket key under the tier's reserved namespace.
pub fn manifest_key(key_prefix: &str) -> String {
    format!("{}{}/manifest", key_prefix, crate::tier::epoch::RESERVED_DIR)
}

/// Import temp-file prefix (step 12's crash-safe stub materialization;
/// the walk and the flusher both skip these names).
pub const IMPORT_TMP_PREFIX: &str = ".flint-import.";

pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

/// One tree entry. Paths are export-root-relative, `/`-separated,
/// never absolute, never containing `..` (the reader re-validates —
/// a manifest is bucket data, not trusted input).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: EntryKind,
    /// Full st_mode (type bits included; restore applies the 07777).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime_unix: i64,
    // File-only: where the content lives and which version it is.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub etag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub crc64_b64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub size: Option<u64>,
    // Symlink-only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// Barrier sequence — strictly increasing across writes (seeded
    /// from the bucket's current manifest at startup, so it survives
    /// hub restarts and even state loss).
    pub seq: u64,
    pub epoch: u64,
    pub written_unix: u64,
    /// Regular files present locally but never yet published — the
    /// manifest's own statement of its loss bound.
    pub beyond_rpo: usize,
    /// Sockets/FIFOs/devices seen and declined (see module docs).
    pub skipped_special: usize,
    pub entries: Vec<Entry>,
}

impl Manifest {
    pub fn to_bytes(&self) -> Vec<u8> {
        // Infallible for this shape; a serializer error would be a
        // code bug, and an empty manifest must not clobber a good one.
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// The standalone reader — this is the "RPO readable from the
    /// bucket alone" surface, and the import's parser.
    pub fn parse(bytes: &[u8]) -> Result<Manifest, String> {
        let m: Manifest =
            serde_json::from_slice(bytes).map_err(|e| format!("manifest parse: {}", e))?;
        if m.version != MANIFEST_VERSION {
            return Err(format!("manifest version {} unsupported", m.version));
        }
        Ok(m)
    }
}

/// A built (not yet stamped) barrier snapshot.
#[derive(Debug)]
pub struct Built {
    pub entries: Vec<Entry>,
    pub beyond_rpo: usize,
    pub skipped_special: usize,
}

impl Built {
    /// Content digest for the skip-unchanged check — over everything
    /// EXCEPT seq/epoch/written_unix (those change every barrier;
    /// re-uploading an identical tree for them would put the manifest
    /// on the fsync-churn bill A11 exists to cap).
    pub fn digest(&self) -> u64 {
        let mut c = crate::tier::store::Crc64Nvme::new();
        for e in &self.entries {
            c.update(&serde_json::to_vec(e).unwrap_or_default());
        }
        c.update(&(self.beyond_rpo as u64).to_be_bytes());
        c.update(&(self.skipped_special as u64).to_be_bytes());
        c.finalize()
    }
}

/// Walk the export tree and assemble the barrier snapshot. BLOCKING
/// (call under spawn_blocking). `gens` is the orchestrator's registry
/// snapshot — a regular file appears iff it has a generation row (the
/// bucket really holds its content); everything else counts into
/// `beyond_rpo`.
pub fn build(export_root: &Path, gens: &HashMap<(u64, u64), GenRecord>) -> std::io::Result<Built> {
    let mut b = Built { entries: Vec::new(), beyond_rpo: 0, skipped_special: 0 };
    walk(export_root, export_root, gens, &mut b)?;
    // Deterministic serialization: digest-stable and parents-first for
    // the restore (a strict path prefix sorts before its extensions).
    b.entries.sort_by(|a, z| a.path.cmp(&z.path));
    Ok(b)
}

fn walk(
    root: &Path,
    dir: &Path,
    gens: &HashMap<(u64, u64), GenRecord>,
    out: &mut Built,
) -> std::io::Result<()> {
    for ent in std::fs::read_dir(dir)? {
        let ent = match ent {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = ent.path();
        let name = ent.file_name();
        let name = name.to_string_lossy();
        // The reserved control namespace and import temps are the
        // tier's own machinery, never manifested.
        if name == crate::tier::epoch::RESERVED_DIR || name.starts_with(IMPORT_TMP_PREFIX) {
            continue;
        }
        let Ok(md) = ent.path().symlink_metadata() else { continue };
        let Some(rel) = rel_path(root, &path) else { continue };
        #[cfg(unix)]
        let (posix, dev_ino) = {
            use std::os::unix::fs::MetadataExt;
            ((md.mode(), md.uid(), md.gid(), md.mtime()), (md.dev(), md.ino()))
        };
        #[cfg(not(unix))]
        let (posix, dev_ino) = ((0u32, 0u32, 0u32, 0i64), (0u64, 0u64));
        let (mode, uid, gid, mtime_unix) = posix;
        let base = Entry {
            path: rel,
            kind: EntryKind::Dir,
            mode,
            uid,
            gid,
            mtime_unix,
            key: None,
            generation: None,
            etag: None,
            crc64_b64: None,
            size: None,
            target: None,
        };
        let ft = md.file_type();
        if ft.is_dir() {
            out.entries.push(base);
            walk(root, &path, gens, out)?;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(&path)
                .map(|t| t.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.entries.push(Entry { kind: EntryKind::Symlink, target: Some(target), ..base });
        } else if ft.is_file() {
            match gens.get(&dev_ino) {
                Some(g) => out.entries.push(Entry {
                    kind: EntryKind::File,
                    key: Some(g.key.clone()),
                    generation: Some(g.generation),
                    etag: Some(g.etag.clone()),
                    crc64_b64: g.crc64_b64.clone(),
                    // The PUBLISHED size — for an evicted stub the
                    // local stat says 0; the bucket object is what a
                    // restore materializes.
                    size: Some(g.size),
                    ..base
                }),
                None => out.beyond_rpo += 1,
            }
        } else {
            // Socket/FIFO/device: declined by contract (module docs).
            out.skipped_special += 1;
        }
    }
    Ok(())
}

fn rel_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy().into_owned();
    #[cfg(windows)]
    let s = s.replace('\\', "/");
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The writer's persistent little state (owned by the orchestrator).
#[derive(Debug, Default)]
pub struct WriterState {
    pub seq: u64,
    /// ETag of the manifest we last observed/wrote — the If-Match
    /// guard for the next write. None ⇒ first write (If-None-Match:*).
    pub etag: Option<String>,
    pub last_digest: Option<u64>,
}

/// Seed from the bucket at startup: the manifest's own seq survives
/// restarts AND total local state loss (it is bucket data).
pub async fn seed(store: &dyn ObjectStore, key_prefix: &str) -> WriterState {
    let key = manifest_key(key_prefix);
    match store.get_whole(&key, None).await {
        Ok((meta, bytes)) => match Manifest::parse(&bytes) {
            Ok(m) => {
                debug!("tier manifest: seeded at seq {} (etag {})", m.seq, meta.etag);
                WriterState { seq: m.seq, etag: Some(meta.etag), last_digest: None }
            }
            Err(e) => {
                warn!("tier manifest: existing {} unparseable ({}) — will overwrite", key, e);
                WriterState { seq: 0, etag: Some(meta.etag), last_digest: None }
            }
        },
        Err(StoreError::NotFound(_)) => WriterState::default(),
        Err(e) => {
            warn!("tier manifest: seed read failed ({}) — first write will create", e);
            WriterState::default()
        }
    }
}

/// What one barrier did to the bucket's manifest.
///
/// The distinction between `Unchanged` and `Failed` is load-bearing and
/// used to be invisible: both returned `None`. "Unchanged" means the
/// standing manifest still describes the tree exactly — the normal arm
/// for an idle hub, which skips every barrier after its first. "Failed"
/// means the bucket's manifest is now BEHIND the tree. Anything that
/// decides whether the bucket can rebuild this volume — `rpoClean`, and
/// through it the hibernate that deletes the PVC — must be able to tell
/// those apart, because one is safe and the other loses data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BarrierOutcome {
    /// A new manifest landed at `seq`.
    Wrote { seq: u64, beyond_rpo: usize, skipped_special: usize },
    /// The tree is unchanged since `seq`; the standing manifest stands.
    Unchanged { seq: u64, beyond_rpo: usize, skipped_special: usize },
    /// The write failed (logged + metered). The previous manifest stays
    /// the RPO record, so the bucket no longer describes the tree.
    Failed,
}

impl BarrierOutcome {
    /// Does the bucket's manifest currently describe this tree?
    pub fn is_current(&self) -> bool {
        !matches!(self, BarrierOutcome::Failed)
    }

    /// Files present locally that the manifest cannot restore.
    pub fn beyond_rpo(&self) -> Option<usize> {
        match self {
            BarrierOutcome::Wrote { beyond_rpo, .. }
            | BarrierOutcome::Unchanged { beyond_rpo, .. } => Some(*beyond_rpo),
            BarrierOutcome::Failed => None,
        }
    }

    pub fn seq(&self) -> Option<u64> {
        match self {
            BarrierOutcome::Wrote { seq, .. } | BarrierOutcome::Unchanged { seq, .. } => Some(*seq),
            BarrierOutcome::Failed => None,
        }
    }
}

/// Write one barrier's manifest.
pub async fn write_at_barrier(
    store: &dyn ObjectStore,
    key_prefix: &str,
    epoch: u64,
    state: &mut WriterState,
    built: Built,
) -> BarrierOutcome {
    let digest = built.digest();
    if state.last_digest == Some(digest) {
        return BarrierOutcome::Unchanged {
            seq: state.seq,
            beyond_rpo: built.beyond_rpo,
            skipped_special: built.skipped_special,
        };
    }
    let (beyond_rpo, skipped_special) = (built.beyond_rpo, built.skipped_special);
    let m = Manifest {
        version: MANIFEST_VERSION,
        seq: state.seq + 1,
        epoch,
        written_unix: now_unix(),
        beyond_rpo: built.beyond_rpo,
        skipped_special: built.skipped_special,
        entries: built.entries,
    };
    let body = m.to_bytes();
    let crc = crate::tier::store::crc64_nvme(&body);
    let condition = match &state.etag {
        Some(e) => PutCondition::IfMatch(e.clone()),
        None => PutCondition::IfNoneMatchAny,
    };
    let stamps = GenerationStamps {
        generation: m.seq,
        epoch,
        flush_uuid: uuid::Uuid::new_v4().to_string(),
        posix: None,
    };
    let key = manifest_key(key_prefix);
    match store.put_whole(&key, body.into(), &condition, &stamps, crc).await {
        Ok(meta) => {
            state.seq = m.seq;
            state.etag = Some(meta.etag);
            state.last_digest = Some(digest);
            crate::tier::meter::bump(crate::tier::meter::Counter::ManifestWrites);
            info!(
                "tier manifest: barrier seq {} — {} entries, {} beyond RPO",
                m.seq,
                m.entries.len(),
                m.beyond_rpo
            );
            BarrierOutcome::Wrote { seq: m.seq, beyond_rpo, skipped_special }
        }
        Err(e) => {
            crate::tier::meter::bump(crate::tier::meter::Counter::ManifestFailures);
            warn!(
                "tier manifest: barrier write failed ({}); previous manifest remains the \
                 RPO record — re-seeding the guard",
                e
            );
            // A 412 means the bucket's manifest is not what we last
            // saw (deposition, foreign interference, or a guard state
            // that predates the bucket's manifest): re-seed seq+etag
            // from the bucket so the next barrier guards correctly.
            if matches!(e, StoreError::PreconditionFailed(_) | StoreError::Conflict(_)) {
                *state = seed(store, key_prefix).await;
            }
            BarrierOutcome::Failed
        }
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    fn ident(path: &Path) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path).unwrap();
        (m.dev(), m.ino())
    }

    fn rec(key: &str, generation: u64, size: u64) -> GenRecord {
        GenRecord {
            key: key.into(),
            generation,
            etag: format!("e-{}", generation),
            crc64_b64: Some("AAAAAAAAAAA=".into()),
            size,
            copy_allowed: true,
        }
    }

    #[tokio::test]
    async fn build_walks_the_tree_and_states_its_loss_bound() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/pub.bin"), b"published").unwrap();
        std::fs::write(root.join("dirty.bin"), b"never flushed").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("sub/pub.bin", root.join("link")).unwrap();
        // Control namespace + import temps never manifest.
        std::fs::create_dir(root.join(".flint")).unwrap();
        std::fs::write(root.join(".flint/epoch-shadow"), b"x").unwrap();
        std::fs::write(root.join(".flint-import.abc"), b"tmp").unwrap();

        let mut gens = HashMap::new();
        gens.insert(ident(&root.join("sub/pub.bin")), rec("vol/sub/pub.bin", 3, 9));
        let b = build(root, &gens).unwrap();

        let paths: Vec<&str> = b.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["link", "sub", "sub/pub.bin"]);
        assert_eq!(b.beyond_rpo, 1, "dirty.bin is beyond the RPO");
        let f = b.entries.iter().find(|e| e.path == "sub/pub.bin").unwrap();
        assert_eq!(f.kind, EntryKind::File);
        assert_eq!(f.generation, Some(3));
        assert_eq!(f.size, Some(9));
        let l = b.entries.iter().find(|e| e.path == "link").unwrap();
        assert_eq!(l.kind, EntryKind::Symlink);
        assert_eq!(l.target.as_deref(), Some("sub/pub.bin"));
        let d = b.entries.iter().find(|e| e.path == "sub").unwrap();
        assert_eq!(d.kind, EntryKind::Dir);
        assert!(d.mode & 0o40000 != 0, "dir type bits present");
    }

    #[tokio::test]
    async fn barrier_writes_are_guarded_versioned_and_skip_unchanged() {
        use crate::tier::store::memory::MemoryStore;
        let store = MemoryStore::new();
        let td = TempDir::new().unwrap();
        std::fs::write(td.path().join("a.bin"), b"aaaa").unwrap();
        let mut gens = HashMap::new();
        gens.insert(ident(&td.path().join("a.bin")), rec("p/a.bin", 1, 4));

        let mut st = seed(&store, "p/").await;
        assert_eq!(st.seq, 0, "no manifest yet");

        let b1 = build(td.path(), &gens).unwrap();
        let w1 = write_at_barrier(&store, "p/", 5, &mut st, b1).await;
        assert_eq!(w1.seq(), Some(1));
        assert!(matches!(w1, BarrierOutcome::Wrote { .. }));

        // Unchanged tree ⇒ no write (the digest short-circuit). The
        // manifest still describes the tree, so this is CURRENT — the
        // arm an idle hub takes on every barrier, and the one the RPO
        // predicate must not confuse with a failed write.
        let b2 = build(td.path(), &gens).unwrap();
        let w2 = write_at_barrier(&store, "p/", 5, &mut st, b2).await;
        assert!(matches!(w2, BarrierOutcome::Unchanged { seq: 1, .. }));
        assert!(w2.is_current());

        // The RPO is readable from the bucket ALONE.
        let (_, bytes) = store.get_whole(&manifest_key("p/"), None).await.unwrap();
        let m = Manifest::parse(&bytes).unwrap();
        assert_eq!((m.seq, m.epoch), (1, 5));
        assert_eq!(m.entries.len(), 1);

        // A change advances seq under the If-Match guard.
        std::fs::write(td.path().join("b.bin"), b"bb").unwrap();
        gens.insert(ident(&td.path().join("b.bin")), rec("p/b.bin", 1, 2));
        let b3 = build(td.path(), &gens).unwrap();
        assert_eq!(write_at_barrier(&store, "p/", 5, &mut st, b3).await.seq(), Some(2));

        // A fresh writer seeds seq from the bucket (restart survival).
        let st2 = seed(&store, "p/").await;
        assert_eq!(st2.seq, 2);
    }
}
